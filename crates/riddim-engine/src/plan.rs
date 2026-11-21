//! The render plan as the audio thread runs it.
//!
//! A plan is immutable once built: the control thread compiles a new one for every change and
//! hands it over whole. What must *survive* a change (plugin processors, delay lines, held
//! notes, gain ramps, meters) does not live in the plan; it lives in [`NodeCell`]s and
//! [`DelayCell`]s that successive plans share by reference. So an edit recompiles the plan, and
//! nothing else.

use std::cell::UnsafeCell;
use std::sync::Arc;

use riddim_model::{SlotId, TrackId};
use riddim_plugin::{EventBody, EventBuffer, PluginProcessor};
use riddim_rt::planar::PlanarScratch;
use riddim_time::TempoMap;

use crate::dsp::{Click, DelayLine, StereoGain};
use crate::shared::MeterCell;

/// Decoded media at the session's sample rate.
pub struct MediaAudio {
    pub channels: Vec<Vec<f32>>,
}

impl MediaAudio {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

/// An audio clip with every position already converted to session samples.
pub(crate) struct RtAudioClip {
    /// Timeline sample of the clip's first frame.
    pub start: i64,
    pub length: i64,
    /// First frame of the media to play.
    pub offset: i64,
    pub gain: f32,
    pub fade_in: i64,
    pub fade_out: i64,
    pub media: Arc<MediaAudio>,
}

/// A MIDI event at an absolute timeline sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TimedEvent {
    pub sample: i64,
    pub body: EventBody,
}

/// One breakpoint of an automation curve, at an absolute timeline sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RtPoint {
    pub sample: i64,
    pub value: f32,
    /// Stay at this value until the next point, instead of ramping towards it.
    pub hold: bool,
}

/// An automation lane with its positions already converted to samples.
pub(crate) struct RtCurve {
    pub points: Vec<RtPoint>,
}

impl RtCurve {
    /// The curve's value at `sample`. Before the first point and after the last, their values
    /// hold. `None` for a curve with no points.
    #[inline]
    pub fn value_at(&self, sample: i64) -> Option<f32> {
        let next = self.points.partition_point(|p| p.sample <= sample);
        let Some(before) = next.checked_sub(1).map(|i| self.points[i]) else {
            return self.points.first().map(|p| p.value);
        };
        match self.points.get(next) {
            Some(after) if !before.hold => {
                let span = (after.sample - before.sample) as f64;
                let along = (sample - before.sample) as f64 / span;
                Some(before.value + ((after.value - before.value) as f64 * along) as f32)
            }
            _ => Some(before.value),
        }
    }

    /// A reader for walking the curve forwards one sample at a time.
    #[inline]
    pub fn cursor(&self) -> CurveCursor<'_> {
        CurveCursor {
            curve: self,
            next: 0,
        }
    }

    /// The breakpoints strictly inside `(start, end)`.
    #[inline]
    pub fn points_inside(&self, start: i64, end: i64) -> &[RtPoint] {
        let from = self.points.partition_point(|p| p.sample <= start);
        let to = self.points.partition_point(|p| p.sample < end);
        &self.points[from..to.max(from)]
    }
}

/// Reads a curve at increasing sample positions in constant time per sample.
///
/// The value at a sample is computed from the curve's own breakpoints, never from the previous
/// sample, so it is the same whatever block size the engine happens to run at.
pub(crate) struct CurveCursor<'a> {
    curve: &'a RtCurve,
    next: usize,
}

impl CurveCursor<'_> {
    /// The curve's value at `sample`. Calls must not go backwards.
    #[inline]
    pub fn value_at(&mut self, sample: i64) -> Option<f32> {
        let points = &self.curve.points;
        while self.next < points.len() && points[self.next].sample <= sample {
            self.next += 1;
        }
        let Some(before) = self.next.checked_sub(1).map(|i| points[i]) else {
            return points.first().map(|p| p.value);
        };
        match points.get(self.next) {
            Some(after) if !before.hold => {
                let span = (after.sample - before.sample) as f64;
                let along = (sample - before.sample) as f64 / span;
                Some(before.value + ((after.value - before.value) as f64 * along) as f32)
            }
            _ => Some(before.value),
        }
    }
}

/// What a node is told to do for this plan. Small values and shared lists only: reading it
/// costs nothing and it never has to be freed on the audio thread, because the plan that owns
/// it is retired through the garbage chute.
pub(crate) enum StepParams {
    AudioSource {
        clips: Arc<Vec<RtAudioClip>>,
        /// Device input to pass through: (first channel, channel count).
        monitor_input: Option<(usize, usize)>,
    },
    MidiSource {
        track: TrackId,
        events: Arc<Vec<TimedEvent>>,
        /// Pass live MIDI input through to the instrument.
        live_input: bool,
        /// Send live MIDI input to the control thread while recording.
        record: bool,
    },
    BusInput,
    Plugin {
        bypass: bool,
        /// Automation for the plugin's parameters: (parameter id, curve).
        curves: Arc<Vec<(u32, Arc<RtCurve>)>>,
    },
    Strip {
        gain: f32,
        pan: f32,
        mute: bool,
        invert: bool,
        gain_curve: Option<Arc<RtCurve>>,
        pan_curve: Option<Arc<RtCurve>>,
    },
    Send {
        gain: f32,
        mute: bool,
        curve: Option<Arc<RtCurve>>,
    },
    Metronome {
        enabled: bool,
        gain: f32,
    },
    Output,
}

/// State that outlives any one plan.
pub(crate) enum NodeRt {
    /// Clip playback is a pure function of the position; nothing to keep.
    Stateless,
    MidiSource(MidiSourceRt),
    Plugin(Box<PluginRt>),
    Strip(StripRt),
    /// A send's gain stage.
    Gain(GainRt),
    Metronome(Click),
}

pub(crate) struct MidiSourceRt {
    /// How many times each (channel, key) is currently held, so every note-on gets its note-off
    /// when the transport stops or jumps.
    pub held: Box<[[u8; 128]; 16]>,
    pub any_held: bool,
}

impl MidiSourceRt {
    pub fn new() -> Self {
        Self {
            held: Box::new([[0; 128]; 16]),
            any_held: false,
        }
    }
}

pub(crate) struct PluginRt {
    pub slot: SlotId,
    pub processor: Box<dyn PluginProcessor>,
    pub scratch: PlanarScratch,
    /// Events for the plugin this block: upstream MIDI plus parameter changes.
    pub events_in: EventBuffer,
    pub events_out: EventBuffer,
    /// Parameter changes waiting for the next block.
    pub pending_params: EventBuffer,
    /// The last value sent for each automated parameter, so a flat curve sends nothing.
    pub automated: Vec<(u32, f32)>,
    /// Delays the dry signal by the plugin's latency while it is bypassed, so bypassing does
    /// not shift the track in time. `None` for a plugin with no latency.
    pub bypass_delay: Option<Box<DelayLine>>,
    pub was_bypassed: bool,
    /// Channels of the main input and main output buses: the first of the channels the
    /// plugin is given. What follows them is the sidechain and the extra outputs.
    pub main_in: usize,
    pub main_out: usize,
}

/// How many automated parameters per plugin are tracked for change detection. Lanes beyond
/// this still work; they just resend their value every block.
pub(crate) const AUTOMATED_CAPACITY: usize = 256;

pub(crate) struct GainRt {
    pub gain: StereoGain,
    pub primed: bool,
}

pub(crate) struct StripRt {
    pub gain: StereoGain,
    /// False until the first block, which sets the gain without a ramp.
    pub primed: bool,
    pub meter: Arc<MeterCell>,
}

/// A node's persistent state. Shared by reference between the control thread's bookkeeping and
/// every plan that includes the node.
pub(crate) struct NodeCell {
    rt: UnsafeCell<NodeRt>,
}

// SAFETY: the inner state is only dereferenced through `get`, whose contract confines it to
// whichever single thread is executing the node's step.
unsafe impl Send for NodeCell {}
unsafe impl Sync for NodeCell {}

impl NodeCell {
    pub fn new(rt: NodeRt) -> Arc<NodeCell> {
        Arc::new(NodeCell {
            rt: UnsafeCell::new(rt),
        })
    }

    /// # Safety
    /// Only the thread currently executing this node's step may call this, and the reference
    /// must not outlive the step. The control thread never calls it while any plan that holds
    /// the cell may still run.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut NodeRt {
        // SAFETY: per the contract above.
        unsafe { &mut *self.rt.get() }
    }

    /// Takes the state back once no plan references the cell any more.
    pub fn into_inner(self) -> NodeRt {
        self.rt.into_inner()
    }
}

/// A latency-compensation delay line that outlives any one plan.
pub(crate) struct DelayCell {
    line: UnsafeCell<DelayLine>,
    pub samples: u32,
    pub channels: usize,
}

// SAFETY: as for `NodeCell`.
unsafe impl Send for DelayCell {}
unsafe impl Sync for DelayCell {}

impl DelayCell {
    pub fn new(channels: usize, samples: u32) -> Arc<DelayCell> {
        Arc::new(DelayCell {
            line: UnsafeCell::new(DelayLine::new(channels, samples as usize)),
            samples,
            channels,
        })
    }

    /// # Safety
    /// As for [`NodeCell::get`]: only the thread executing the step that owns this source.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut DelayLine {
        // SAFETY: per the contract above.
        unsafe { &mut *self.line.get() }
    }
}

/// One upstream signal summed into an input port.
pub(crate) struct RtSource {
    pub first_buffer: usize,
    pub channels: usize,
    pub delay: Option<Arc<DelayCell>>,
}

/// An input port: where its sum is assembled, and from what.
pub(crate) struct RtInput {
    pub first_scratch: usize,
    pub channels: usize,
    pub sources: Vec<RtSource>,
}

pub(crate) struct RtStep {
    pub cell: Arc<NodeCell>,
    pub params: StepParams,
    pub inputs: Vec<RtInput>,
    /// The step's input channels, flat across ports, are `scratch[first_scratch..][..in_channels]`.
    pub first_scratch: usize,
    pub in_channels: usize,
    /// How late this node's input is relative to the timeline, in samples. Automation is read
    /// that much earlier so it lines up with the audio it acts on.
    pub input_latency: i64,
    /// Its output channels, flat across ports, are `audio[first_buffer..][..out_channels]`.
    pub first_buffer: usize,
    pub out_channels: usize,
    /// Event buffers this step reads.
    pub event_sources: Vec<usize>,
    /// The event buffer this step writes, if it produces events.
    pub event_output: Option<usize>,
}

/// A compiled plan, ready to run.
pub(crate) struct RtPlan {
    pub steps: Vec<RtStep>,
    /// Index of the device-output step.
    pub output_step: usize,
    /// (slot, step) for every plugin, sorted by slot, to route parameter changes.
    pub plugins: Vec<(SlotId, usize)>,
    pub buffer_count: usize,
    pub scratch_count: usize,
    pub event_buffer_count: usize,
    pub tempo_map: Arc<TempoMap>,
    /// Loop region in samples, if looping is on.
    pub loop_range: Option<(i64, i64)>,
    /// Samples by which the output lags the timeline.
    pub latency: u32,
    pub serial: u64,
}

impl RtPlan {
    pub fn plugin_step(&self, slot: SlotId) -> Option<usize> {
        self.plugins
            .binary_search_by_key(&slot, |p| p.0)
            .ok()
            .map(|i| self.plugins[i].1)
    }
}

/// The memory a plan runs in. Owned by the audio thread, replaced (never resized) when a plan
/// needs more than is there.
pub(crate) struct Buffers {
    pub audio: Vec<Vec<f32>>,
    pub scratch: Vec<Vec<f32>>,
    pub events: Vec<EventBuffer>,
    pub merged_events: EventBuffer,
    /// The most frames these buffers can carry: the largest block the engine may run with
    /// them.
    pub max_block: usize,
}

/// Events per block that an event buffer can carry.
pub(crate) const EVENT_CAPACITY: usize = 2048;

impl Buffers {
    pub fn new(audio: usize, scratch: usize, events: usize, max_block: usize) -> Buffers {
        Buffers {
            audio: vec![vec![0.0; max_block]; audio],
            scratch: vec![vec![0.0; max_block]; scratch],
            events: (0..events)
                .map(|_| EventBuffer::with_capacity(EVENT_CAPACITY))
                .collect(),
            merged_events: EventBuffer::with_capacity(EVENT_CAPACITY),
            max_block,
        }
    }
}
