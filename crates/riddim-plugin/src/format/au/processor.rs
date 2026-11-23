//! The audio side of a hosted Audio Unit.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use objc2_audio_toolbox::{
    AudioComponentInstanceDispose, AudioUnit, AudioUnitRender, AudioUnitRenderActionFlags,
    AudioUnitSetParameter, AudioUnitUninitialize, MusicDeviceMIDIEvent,
};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};

use super::sys::{buffers_mut, BufferList, OSStatus, NO_ERR, SCOPE_GLOBAL};
use crate::event::EventBody;
use crate::instance::PluginProcessor;
use crate::process::{ProcessContext, ProcessStatus, TransportInfo};

/// What the unit's callbacks read while it renders. Written by the processor at the top of each
/// block, read by the callbacks the unit makes from inside `AudioUnitRender` on the same thread.
pub(super) struct RenderState {
    pub transport: TransportInfo,
    pub frames: usize,
    /// For each of the unit's input buses: where its channels start in `input_ptrs`, and how
    /// many there are. Inactive buses have zero channels.
    pub input_buses: Vec<(usize, usize)>,
    /// Channel pointers for this block, flat across active input buses.
    pub input_ptrs: Vec<*const f32>,
    /// Private copies handed to the unit when it asks us to supply the memory, so it can never
    /// scribble on a buffer other nodes read.
    pub input_copies: Vec<Vec<f32>>,
    pub silence: Vec<f32>,
}

/// The unit itself plus the state its callbacks point at. Shared between the instance and the
/// processor; the unit is disposed when the last of them lets go.
pub(super) struct Shared {
    pub unit: AudioUnit,
    pub render: UnsafeCell<RenderState>,
    pub initialized: std::sync::atomic::AtomicBool,
}

// SAFETY: the AudioUnit C API is callable from any thread. `render` is written only by the
// processor on the audio thread (or by the instance while no processor exists).
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: we own the unit; nothing else can be using it once the last Arc is gone.
        unsafe {
            if self.initialized.load(std::sync::atomic::Ordering::Acquire) {
                AudioUnitUninitialize(self.unit);
            }
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

/// Supplies input audio when the unit pulls one of its input buses.
pub(super) unsafe extern "C-unwind" fn input_callback(
    ref_con: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _time: NonNull<AudioTimeStamp>,
    bus: u32,
    frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if io_data.is_null() {
        return NO_ERR;
    }
    // SAFETY: `ref_con` is the `Shared` registered at activation, which outlives the unit's
    // initialised state; we are inside AudioUnitRender on the processing thread, the only
    // writer of `render`.
    unsafe {
        let shared = &*(ref_con.as_ptr() as *const Shared);
        let state = &mut *shared.render.get();
        let frames = (frames as usize).min(state.frames);
        let (first, channels) = state
            .input_buses
            .get(bus as usize)
            .copied()
            .unwrap_or((0, 0));
        for (i, buffer) in buffers_mut(io_data).iter_mut().enumerate() {
            if i < channels {
                let source = state.input_ptrs[first + i];
                if buffer.mData.is_null() {
                    let copy = &mut state.input_copies[first + i];
                    std::ptr::copy_nonoverlapping(source, copy.as_mut_ptr(), frames);
                    buffer.mData = copy.as_mut_ptr() as *mut c_void;
                } else {
                    std::ptr::copy_nonoverlapping(source, buffer.mData as *mut f32, frames);
                }
            } else if buffer.mData.is_null() {
                // A channel we have nothing for: hand out zeros the unit may overwrite.
                state.silence[..frames].fill(0.0);
                buffer.mData = state.silence.as_mut_ptr() as *mut c_void;
            } else {
                std::slice::from_raw_parts_mut(buffer.mData as *mut f32, frames).fill(0.0);
            }
            buffer.mDataByteSize = (frames * std::mem::size_of::<f32>()) as u32;
        }
    }
    NO_ERR
}

pub(super) unsafe extern "C-unwind" fn beat_and_tempo(
    user_data: *mut c_void,
    out_beat: *mut f64,
    out_tempo: *mut f64,
) -> OSStatus {
    // SAFETY: as for `input_callback`; out-pointers may be null and are checked.
    unsafe {
        let t = &(*(*(user_data as *const Shared)).render.get()).transport;
        if !out_beat.is_null() {
            *out_beat = t.position_quarters;
        }
        if !out_tempo.is_null() {
            *out_tempo = t.tempo_bpm;
        }
    }
    NO_ERR
}

pub(super) unsafe extern "C-unwind" fn musical_time_location(
    user_data: *mut c_void,
    out_delta_to_next_beat: *mut u32,
    out_numerator: *mut f32,
    out_denominator: *mut u32,
    out_measure_downbeat: *mut f64,
) -> OSStatus {
    // SAFETY: as for `beat_and_tempo`.
    unsafe {
        let t = &(*(*(user_data as *const Shared)).render.get()).transport;
        if !out_delta_to_next_beat.is_null() {
            let to_next_beat = t.position_quarters.ceil() - t.position_quarters;
            let samples_per_beat = t.sample_rate * 60.0 / t.tempo_bpm.max(1.0);
            *out_delta_to_next_beat = (to_next_beat * samples_per_beat) as u32;
        }
        if !out_numerator.is_null() {
            *out_numerator = t.time_sig_numerator as f32;
        }
        if !out_denominator.is_null() {
            *out_denominator = t.time_sig_denominator as u32;
        }
        if !out_measure_downbeat.is_null() {
            *out_measure_downbeat = t.bar_start_quarters;
        }
    }
    NO_ERR
}

pub(super) unsafe extern "C-unwind" fn transport_state(
    user_data: *mut c_void,
    out_playing: *mut u8,
    out_changed: *mut u8,
    out_sample: *mut f64,
    out_cycling: *mut u8,
    out_cycle_start: *mut f64,
    out_cycle_end: *mut f64,
) -> OSStatus {
    // SAFETY: forwards to the extended callback with a null "recording" pointer.
    unsafe {
        transport_state2(
            user_data,
            out_playing,
            std::ptr::null_mut(),
            out_changed,
            out_sample,
            out_cycling,
            out_cycle_start,
            out_cycle_end,
        )
    }
}

pub(super) unsafe extern "C-unwind" fn transport_state2(
    user_data: *mut c_void,
    out_playing: *mut u8,
    out_recording: *mut u8,
    out_changed: *mut u8,
    out_sample: *mut f64,
    out_cycling: *mut u8,
    out_cycle_start: *mut f64,
    out_cycle_end: *mut f64,
) -> OSStatus {
    // SAFETY: as for `beat_and_tempo`.
    unsafe {
        let t = &(*(*(user_data as *const Shared)).render.get()).transport;
        if !out_playing.is_null() {
            *out_playing = t.playing as u8;
        }
        if !out_recording.is_null() {
            *out_recording = t.recording as u8;
        }
        if !out_changed.is_null() {
            *out_changed = 0;
        }
        if !out_sample.is_null() {
            *out_sample = t.position_samples as f64;
        }
        if !out_cycling.is_null() {
            *out_cycling = t.looping as u8;
        }
        if !out_cycle_start.is_null() {
            *out_cycle_start = t.loop_start_quarters;
        }
        if !out_cycle_end.is_null() {
            *out_cycle_end = t.loop_end_quarters;
        }
    }
    NO_ERR
}

/// The audio side of an [`AuInstance`](super::AuInstance).
pub struct AuProcessor {
    shared: Arc<Shared>,
    /// Active output buses: (unit bus number, channel count).
    output_buses: Vec<(u32, usize)>,
    /// (parameter id, minimum, maximum), sorted by id, for de-normalising values.
    param_ranges: Vec<(u32, f32, f32)>,
    buffer_list: BufferList,
    sample_time: f64,
    max_block: usize,
}

impl AuProcessor {
    pub(super) fn new(
        shared: Arc<Shared>,
        output_buses: Vec<(u32, usize)>,
        mut param_ranges: Vec<(u32, f32, f32)>,
        max_block: usize,
    ) -> Self {
        param_ranges.sort_by_key(|r| r.0);
        let widest = output_buses.iter().map(|b| b.1).max().unwrap_or(1);
        Self {
            shared,
            output_buses,
            param_ranges,
            buffer_list: BufferList::with_capacity(widest),
            sample_time: 0.0,
            max_block,
        }
    }

    #[inline]
    fn plain_value(&self, id: u32, normalized: f64) -> Option<f32> {
        let index = self.param_ranges.binary_search_by_key(&id, |r| r.0).ok()?;
        let (_, min, max) = self.param_ranges[index];
        Some(min + (max - min) * normalized.clamp(0.0, 1.0) as f32)
    }
}

impl PluginProcessor for AuProcessor {
    fn process(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus {
        let frames = context.frames;
        let silence = |context: &mut ProcessContext<'_>| {
            for channel in context.outputs.iter_mut() {
                channel.fill(0.0);
            }
            ProcessStatus::Failed
        };
        let wanted_outputs: usize = self.output_buses.iter().map(|b| b.1).sum();
        // SAFETY: reading the length of a table only this processor writes.
        let wanted_inputs = unsafe { (*self.shared.render.get()).input_ptrs.len() };
        if frames > self.max_block
            || context.outputs.len() < wanted_outputs
            || context.inputs.len() < wanted_inputs
        {
            return silence(context);
        }
        let unit = self.shared.unit;

        // SAFETY: the processor is the only writer of the render state, and the unit is not
        // rendering yet.
        unsafe {
            let state = &mut *self.shared.render.get();
            state.transport = *context.transport;
            state.frames = frames;
            for (slot, channel) in state.input_ptrs.iter_mut().zip(context.inputs.iter()) {
                *slot = channel.as_ptr();
            }
        }

        // What the unit does inside its own calls is outside our real-time guarantee.
        let status = riddim_rt::guard::permit(|| {
            // SAFETY: MusicDeviceMIDIEvent and AudioUnitSetParameter are documented as callable
            // on the render thread ahead of AudioUnitRender, with sample offsets.
            unsafe {
                for event in context.events_in {
                    let offset = event.offset.min(frames.saturating_sub(1) as u32);
                    match event.body {
                        EventBody::Param { id, value } => {
                            if let Some(plain) = self.plain_value(id, value) {
                                AudioUnitSetParameter(unit, id, SCOPE_GLOBAL, 0, plain, offset);
                            }
                        }
                        _ => {
                            if let Some((bytes, _)) = event.to_midi() {
                                MusicDeviceMIDIEvent(
                                    unit,
                                    bytes[0] as u32,
                                    bytes[1] as u32,
                                    bytes[2] as u32,
                                    offset,
                                );
                            }
                        }
                    }
                }
            }

            // SAFETY: an all-zero AudioTimeStamp is valid; we then mark the sample time.
            let mut time: AudioTimeStamp = unsafe { std::mem::zeroed() };
            time.mSampleTime = self.sample_time;
            time.mFlags = AudioTimeStampFlags::SampleTimeValid;

            let mut first_channel = 0;
            for &(bus, channels) in &self.output_buses {
                let outputs = &mut context.outputs[first_channel..first_channel + channels];
                self.buffer_list
                    .prepare(channels, frames, |c| outputs[c].as_mut_ptr());
                let mut flags = AudioUnitRenderActionFlags(0);
                // SAFETY: the buffer list describes `channels` buffers of `frames` samples that
                // stay valid for the call.
                let status = unsafe {
                    AudioUnitRender(
                        unit,
                        &mut flags,
                        NonNull::from(&mut time),
                        bus,
                        frames as u32,
                        self.buffer_list.as_ptr(),
                    )
                };
                if status != NO_ERR {
                    return status;
                }
                // The unit may have rendered into its own memory and repointed the buffers.
                for (c, output) in outputs.iter_mut().enumerate() {
                    let data = self.buffer_list.data(c);
                    if !data.is_null() && data != output.as_mut_ptr() {
                        // SAFETY: the unit promises `frames` samples at the pointer it left.
                        unsafe { std::ptr::copy_nonoverlapping(data, output.as_mut_ptr(), frames) };
                    }
                }
                first_channel += channels;
            }
            NO_ERR
        });

        self.sample_time += frames as f64;
        if status != NO_ERR {
            return silence(context);
        }
        ProcessStatus::Ok
    }

    fn reset(&mut self) {
        // SAFETY: AudioUnitReset is callable from the render thread.
        unsafe { objc2_audio_toolbox::AudioUnitReset(self.shared.unit, SCOPE_GLOBAL, 0) };
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
