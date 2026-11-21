//! The audio side of the engine: the device callback.
//!
//! Each device block it drains the control queue, advances the transport, runs the plan, feeds
//! the capture rings and publishes meters and position. It never allocates, frees, locks or
//! blocks: anything it stops needing goes back to the control thread to be dropped there.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use riddim_disk::capture::CaptureSink;
use riddim_io::midi::MidiMessage;
use riddim_io::{AudioCallback, IoBlock};
use riddim_model::{SlotId, TrackId};
use riddim_plugin::{Event, EventBody, EventBuffer, TransportInfo};
use riddim_rt::garbage::GarbageChute;
use riddim_rt::spsc::{Consumer, Producer};
use riddim_time::{samples_to_superclock, TempoMap};

use crate::exec::Executor;
use crate::nodes::{BlockCtx, NodeMessage};
use crate::plan::{Buffers, NodeRt, RtPlan};
use crate::shared::Shared;

/// One track's recording in progress, as the audio thread holds it.
pub(crate) struct CaptureRt {
    pub sink: CaptureSink,
    /// Device input channels to record: (first, count).
    pub first_channel: usize,
    pub channels: usize,
}

/// Control thread to audio thread.
pub(crate) enum ToAudio {
    /// Run this plan from the next block on. `buffers` comes along when the plan needs more
    /// memory than the audio thread has.
    SwapPlan {
        plan: Box<RtPlan>,
        buffers: Option<Box<Buffers>>,
    },
    Play,
    Stop,
    Locate(i64),
    /// Start recording from where the transport is when this arrives (or `punch_ahead` samples
    /// later if it is already rolling), after `count_in_bars` of click if it is not. Capture
    /// starts `round_trip` samples after that point, so the first captured frame is the one
    /// played at it.
    Record {
        captures: Vec<CaptureRt>,
        round_trip: i64,
        count_in_bars: u32,
        punch_ahead: i64,
    },
    /// A new bypass delay for a plugin whose latency changed (`None` for no latency).
    BypassDelay {
        slot: SlotId,
        line: Option<Box<crate::dsp::DelayLine>>,
    },
    /// A parameter value for a plugin, applied at the start of the next block.
    Param {
        slot: SlotId,
        id: u32,
        value: f64,
    },
    /// A note or controller played from the application (auditioning, on-screen keyboard).
    Midi {
        track: TrackId,
        body: EventBody,
    },
}

/// Audio thread to control thread.
pub(crate) enum FromAudio {
    /// Recording will start at this timeline sample.
    RecordingStarted {
        start: i64,
    },
    /// Recording ended at this timeline sample; the sinks come back so their files can be
    /// finished.
    RecordingEnded {
        captures: Vec<CaptureRt>,
        stopped_at: i64,
    },
    RecordedMidi {
        track: TrackId,
        sample: i64,
        body: EventBody,
    },
    PluginParam {
        slot: SlotId,
        id: u32,
        value: f64,
    },
    /// While playing, the playhead left `from` and carried on at `to`: a loop wrapped or the
    /// transport was located.
    Jumped {
        from: i64,
        to: i64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rolling {
    Stopped,
    Playing,
}

struct Recording {
    captures: Vec<CaptureRt>,
    capture_from: i64,
    count_in_until: i64,
}

pub(crate) struct AudioEngine {
    plan: Option<Box<RtPlan>>,
    buffers: Box<Buffers>,
    control: Consumer<ToAudio>,
    replies: Producer<FromAudio>,
    chute: GarbageChute,
    midi_in: Consumer<MidiMessage>,
    live_midi: EventBuffer,
    /// Notes injected by the application this block, per track.
    injected: Vec<(TrackId, EventBody)>,
    rolling: Rolling,
    position: i64,
    continuous: i64,
    discontinuity: bool,
    recording: Option<Recording>,
    shared: Arc<Shared>,
    sample_rate: u32,
    max_block: usize,
    executor: Executor,
}

impl AudioEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        control: Consumer<ToAudio>,
        replies: Producer<FromAudio>,
        chute: GarbageChute,
        midi_in: Consumer<MidiMessage>,
        shared: Arc<Shared>,
        sample_rate: u32,
        max_block: usize,
        executor: Executor,
    ) -> Self {
        Self {
            plan: None,
            buffers: Box::new(Buffers::new(0, 0, 0, max_block)),
            control,
            replies,
            chute,
            midi_in,
            live_midi: EventBuffer::with_capacity(1024),
            injected: Vec::with_capacity(256),
            rolling: Rolling::Stopped,
            position: 0,
            continuous: 0,
            discontinuity: false,
            recording: None,
            shared,
            sample_rate,
            max_block,
            executor,
        }
    }

    /// Sends a message to the control thread. If its queue is full the message is lost (and
    /// counted), but never freed on this thread.
    fn reply(&mut self, message: FromAudio) {
        if let Err(riddim_rt::spsc::PushError::Full(message)) = self.replies.push(message) {
            self.shared.events_lost.fetch_add(1, Ordering::Relaxed);
            // Never free here: what cannot be handed back is leaked, and counted.
            std::mem::forget(message);
        }
    }

    fn end_recording(&mut self) {
        if let Some(recording) = self.recording.take() {
            self.shared.recording.store(false, Ordering::Release);
            self.reply(FromAudio::RecordingEnded {
                captures: recording.captures,
                stopped_at: self.position,
            });
        }
    }

    fn drain_control(&mut self) {
        while let Ok(message) = self.control.pop() {
            match message {
                ToAudio::SwapPlan { plan, buffers } => {
                    if let Some(buffers) = buffers {
                        // The block size the engine runs at changes with the buffers that
                        // were sized for it, and never apart from them.
                        self.max_block = buffers.max_block;
                        let old = std::mem::replace(&mut self.buffers, buffers);
                        self.chute.discard(old);
                    }
                    self.shared
                        .plan_serial
                        .store(plan.serial, Ordering::Release);
                    self.shared
                        .plan_latency
                        .store(plan.latency, Ordering::Release);
                    if let Some(old) = self.plan.replace(plan) {
                        self.chute.discard(old);
                    }
                }
                ToAudio::Play => {
                    if self.rolling == Rolling::Stopped {
                        self.rolling = Rolling::Playing;
                        self.discontinuity = true;
                    }
                }
                ToAudio::Stop => {
                    if self.rolling == Rolling::Playing {
                        self.rolling = Rolling::Stopped;
                        self.discontinuity = true;
                        self.end_recording();
                    } else {
                        self.end_recording();
                    }
                }
                ToAudio::Locate(position) => {
                    if self.rolling == Rolling::Playing {
                        self.reply(FromAudio::Jumped {
                            from: self.position,
                            to: position,
                        });
                    }
                    self.position = position;
                    self.discontinuity = true;
                }
                ToAudio::Record {
                    captures,
                    round_trip,
                    count_in_bars,
                    punch_ahead,
                } => {
                    self.end_recording();
                    let rolling = self.rolling == Rolling::Playing;
                    // The start is decided here, where the position is known exactly.
                    let start = if rolling {
                        self.position + punch_ahead
                    } else {
                        self.position
                    };
                    if !rolling && count_in_bars > 0 {
                        if let Some(plan) = &self.plan {
                            let map = &plan.tempo_map;
                            let tick = map.samples_to_ticks(start, self.sample_rate);
                            let bar = map.meter_at(tick).ticks_per_bar();
                            let from = riddim_time::Ticks(tick.0 - count_in_bars as i64 * bar);
                            self.position = map.ticks_to_samples(from, self.sample_rate);
                            self.discontinuity = true;
                        }
                    }
                    self.recording = Some(Recording {
                        captures,
                        capture_from: start + round_trip,
                        count_in_until: start,
                    });
                    self.shared.recording.store(true, Ordering::Release);
                    self.reply(FromAudio::RecordingStarted { start });
                }
                ToAudio::Param { slot, id, value } => {
                    if let Some(plan) = &self.plan {
                        if let Some(step) = plan.plugin_step(slot) {
                            // SAFETY: we are the audio thread, between blocks; no step is
                            // running.
                            if let NodeRt::Plugin(plugin) = unsafe { plan.steps[step].cell.get() } {
                                plugin.pending_params.push(Event::param(0, id, value));
                            }
                        }
                    }
                }
                ToAudio::BypassDelay { slot, line } => {
                    let mut line = line;
                    if let Some(plan) = &self.plan {
                        if let Some(step) = plan.plugin_step(slot) {
                            // SAFETY: we are the audio thread, between blocks.
                            if let NodeRt::Plugin(plugin) = unsafe { plan.steps[step].cell.get() } {
                                std::mem::swap(&mut plugin.bypass_delay, &mut line);
                            }
                        }
                    }
                    // Whatever is left (the old line, or the new one if the plugin has gone) is
                    // freed on the control thread.
                    if let Some(old) = line {
                        self.chute.discard(old);
                    }
                }
                ToAudio::Midi { track, body } => {
                    if self.injected.len() < self.injected.capacity() {
                        self.injected.push((track, body));
                    }
                }
            }
        }
    }

    fn transport_info(&self, map: &TempoMap) -> TransportInfo {
        let ticks = map.superclock_to_ticks(samples_to_superclock(self.position, self.sample_rate));
        let meter = map.meter_at(ticks);
        let (loop_start, loop_end) = match self.plan.as_ref().and_then(|p| p.loop_range) {
            Some((start, end)) => {
                let quarters = |sample: i64| {
                    map.superclock_to_ticks(samples_to_superclock(sample, self.sample_rate))
                        .as_beats_f64()
                };
                (quarters(start), quarters(end))
            }
            None => (0.0, 0.0),
        };
        TransportInfo {
            playing: self.rolling == Rolling::Playing,
            recording: self.recording.is_some(),
            looping: loop_end > loop_start,
            sample_rate: self.sample_rate as f64,
            position_samples: self.position,
            continuous_samples: self.continuous,
            position_quarters: ticks.as_beats_f64(),
            bar_start_quarters: map.bar_start(ticks).as_beats_f64(),
            loop_start_quarters: loop_start,
            loop_end_quarters: loop_end,
            tempo_bpm: map.tempo_at(ticks).bpm(),
            time_sig_numerator: meter.numerator,
            time_sig_denominator: meter.denominator,
        }
    }

    /// Runs the plan for `frames` frames starting `offset` frames into the device block.
    fn run_segment(&mut self, block: &mut IoBlock<'_>, offset: usize, frames: usize) {
        let Some(plan) = self.plan.as_deref() else {
            for output in block.outputs.iter_mut() {
                output[offset..offset + frames].fill(0.0);
            }
            return;
        };
        let transport = self.transport_info(&plan.tempo_map);
        let force_click = self
            .recording
            .as_ref()
            .is_some_and(|r| self.position < r.count_in_until);
        let ctx = BlockCtx {
            frames,
            sample_rate: self.sample_rate,
            playing: self.rolling == Rolling::Playing,
            recording: self.recording.is_some(),
            position: self.position,
            discontinuity: self.discontinuity,
            device_inputs: block.inputs,
            device_offset: offset,
            live_midi: self.live_midi.as_slice(),
            injected: &self.injected,
            transport: &transport,
            tempo_map: &plan.tempo_map,
            force_click,
        };
        let buffers = &mut *self.buffers;
        let replies = &mut self.replies;
        let shared = &self.shared;
        let dropped = self.executor.run(plan, buffers, &ctx, |message| {
            let message = match message {
                NodeMessage::PluginParam { slot, id, value } => {
                    FromAudio::PluginParam { slot, id, value }
                }
                NodeMessage::RecordedMidi {
                    track,
                    sample,
                    body,
                } => FromAudio::RecordedMidi {
                    track,
                    sample,
                    body,
                },
            };
            if replies.push(message).is_err() {
                shared.events_lost.fetch_add(1, Ordering::Relaxed);
            }
        });
        if dropped > 0 {
            shared.events_lost.fetch_add(dropped, Ordering::Relaxed);
        }

        // The output step's input sum is what goes to the device.
        let out = &plan.steps[plan.output_step];
        for (c, output) in block.outputs.iter_mut().enumerate() {
            let target = &mut output[offset..offset + frames];
            if c < out.in_channels {
                target.copy_from_slice(&buffers.scratch[out.first_scratch + c][..frames]);
            } else {
                target.fill(0.0);
            }
        }
    }

    /// Feeds the capture rings with the part of this segment that is being recorded.
    fn capture_segment(&mut self, block: &IoBlock<'_>, offset: usize, frames: usize) {
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        if self.rolling != Rolling::Playing {
            return;
        }
        let segment_end = self.position + frames as i64;
        if segment_end <= recording.capture_from {
            return;
        }
        let skip = (recording.capture_from - self.position).max(0) as usize;
        let start = offset + skip;
        let count = frames - skip;
        for capture in recording.captures.iter_mut() {
            let mut channels: [&[f32]; 2] = [&[], &[]];
            let width = capture.channels.min(2);
            let mut complete = true;
            for (c, slot) in channels.iter_mut().enumerate().take(width) {
                match block.inputs.get(capture.first_channel + c) {
                    Some(input) => *slot = &input[start..start + count],
                    None => complete = false,
                }
            }
            // A device with fewer inputs than the track asked for records silence, not garbage.
            let pushed = if complete {
                capture.sink.push(&channels[..width], count)
            } else {
                capture.sink.push(&[], count)
            };
            if !pushed {
                self.shared
                    .capture_dropped
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
        }
    }
}

impl AudioCallback for AudioEngine {
    fn process(&mut self, block: &mut IoBlock<'_>) {
        self.injected.clear();
        self.drain_control();

        // Live MIDI plays from the start of the block it arrived before.
        self.live_midi.clear();
        while let Ok(message) = self.midi_in.pop() {
            if let Some(event) = Event::from_midi(0, &message.bytes[..message.len as usize]) {
                self.live_midi.push(event);
            }
        }

        let mut done = 0;
        while done < block.frames {
            let mut frames = (block.frames - done).min(self.max_block);
            let mut wrap_to = None;
            if self.rolling == Rolling::Playing && self.recording.is_none() {
                if let Some((start, end)) = self.plan.as_ref().and_then(|p| p.loop_range) {
                    if self.position >= end {
                        // Located past the loop: play on without wrapping.
                    } else if self.position + frames as i64 >= end {
                        frames = (end - self.position) as usize;
                        wrap_to = Some(start);
                    }
                }
            }
            if frames > 0 {
                self.run_segment(block, done, frames);
                self.capture_segment(block, done, frames);
                self.discontinuity = false;
                if self.rolling == Rolling::Playing {
                    self.position += frames as i64;
                }
                self.continuous += frames as i64;
                done += frames;
            }
            if let Some(start) = wrap_to {
                self.reply(FromAudio::Jumped {
                    from: self.position,
                    to: start,
                });
                self.position = start;
                self.discontinuity = true;
            }
        }

        self.shared.position.store(self.position, Ordering::Release);
        self.shared
            .playing
            .store(self.rolling == Rolling::Playing, Ordering::Release);
        self.shared.blocks.fetch_add(1, Ordering::Relaxed);
        self.shared
            .leaked
            .store(self.chute.leaked(), Ordering::Relaxed);
        self.shared.rt_violations.store(
            riddim_rt::guard::thread_violations() + self.executor.worker_violations(),
            Ordering::Relaxed,
        );
    }
}
