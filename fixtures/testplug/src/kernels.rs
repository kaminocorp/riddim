//! The DSP of the test synth and the test delay.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

use vst3::Steinberg::Vst::{
    AudioBusBuffers, Event, IEventListTrait, IParamValueQueueTrait, IParameterChangesTrait,
    ProcessData,
};
use vst3::Steinberg::{kResultOk, TUID};
use vst3::{uid, ComRef};

use crate::framework::{
    BusSpec, Kernel, ParamSpec, Shared, PARAM_CAN_AUTOMATE, PARAM_IS_READ_ONLY,
};

const EVENT_NOTE_ON: u16 = 0;
const EVENT_NOTE_OFF: u16 = 1;

/// Channel `c` of bus `b` as a mutable slice, or `None` if the host did not provide it.
unsafe fn channel<'a>(
    buses: *mut AudioBusBuffers,
    bus_count: i32,
    b: usize,
    c: usize,
    frames: usize,
) -> Option<&'a mut [f32]> {
    if buses.is_null() || b >= bus_count as usize {
        return None;
    }
    let bus = unsafe { &*buses.add(b) };
    let channels = unsafe { bus.__field0.channelBuffers32 };
    if channels.is_null() || c >= bus.numChannels as usize {
        return None;
    }
    let ptr = unsafe { *channels.add(c) };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts_mut(ptr, frames) })
    }
}

unsafe fn write_output_param(data: &ProcessData, id: u32, value: f64) {
    if let Some(changes) = unsafe { ComRef::from_raw(data.outputParameterChanges) } {
        let mut index = 0;
        if let Some(queue) = unsafe { ComRef::from_raw(changes.addParameterData(&id, &mut index)) }
        {
            unsafe { queue.addPoint(0, value, &mut index) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Synth
// ---------------------------------------------------------------------------------------------

pub mod synth_params {
    pub const GAIN: u32 = 100;
    pub const MOD: u32 = 101;
    /// Output: peak of the main bus in the last block.
    pub const METER: u32 = 103;
    /// Output: host tempo / 1000.
    pub const TEMPO_SEEN: u32 = 104;
    /// Output: 1 while the host transport is playing.
    pub const PLAYING_SEEN: u32 = 105;
}

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    key: i16,
    channel: i16,
    phase: f64,
    increment: f64,
    amplitude: f32,
}

const NO_VOICE: Voice = Voice {
    active: false,
    key: 0,
    channel: 0,
    phase: 0.0,
    increment: 0.0,
    amplitude: 0.0,
};

struct SynthState {
    sample_rate: f64,
    voices: [Voice; 16],
}

/// A polyphonic sine synth. Phase starts at zero on every note-on, so the first sample of a note
/// is exactly 0.0 and the second is not: tests use that to check sample-accurate event timing.
///
/// Notes on MIDI channel 2 and up go to the second output bus when the host has activated it.
pub struct SynthKernel {
    state: UnsafeCell<SynthState>,
}

// SAFETY: `state` is touched only from `setup` (while inactive) and `process` (audio thread).
unsafe impl Sync for SynthKernel {}

impl Kernel for SynthKernel {
    const NAME: &'static str = "Riddim Test Synth";
    const SUB_CATEGORIES: &'static str = "Instrument|Synth";
    const CID: TUID = uid(0x52444D54, 0x53594E31, 0x50524F43, 0x00000001);
    const CONTROLLER_CID: TUID = uid(0x52444D54, 0x53594E31, 0x4354524C, 0x00000001);
    const STATE_MAGIC: [u8; 4] = *b"TSY1";
    const AUDIO_INS: &'static [BusSpec] = &[];
    const AUDIO_OUTS: &'static [BusSpec] = &[
        BusSpec {
            name: "Main",
            channels: 2,
            main: true,
            default_active: true,
        },
        BusSpec {
            name: "Aux",
            channels: 2,
            main: false,
            default_active: false,
        },
    ];
    const EVENT_INS: i32 = 1;
    const EVENT_OUTS: i32 = 1;
    const PARAMS: &'static [ParamSpec] = &[
        ParamSpec {
            id: synth_params::GAIN,
            title: "Gain",
            units: "",
            default: 0.5,
            steps: 0,
            flags: PARAM_CAN_AUTOMATE,
            stored: true,
        },
        ParamSpec {
            id: synth_params::MOD,
            title: "Mod",
            units: "",
            default: 0.0,
            steps: 0,
            flags: PARAM_CAN_AUTOMATE,
            stored: true,
        },
        ParamSpec {
            id: synth_params::METER,
            title: "Meter",
            units: "",
            default: 0.0,
            steps: 0,
            flags: PARAM_IS_READ_ONLY,
            stored: false,
        },
        ParamSpec {
            id: synth_params::TEMPO_SEEN,
            title: "Tempo Seen",
            units: "",
            default: 0.0,
            steps: 0,
            flags: PARAM_IS_READ_ONLY,
            stored: false,
        },
        ParamSpec {
            id: synth_params::PLAYING_SEEN,
            title: "Playing Seen",
            units: "",
            default: 0.0,
            steps: 1,
            flags: PARAM_IS_READ_ONLY,
            stored: false,
        },
    ];
    const MIDI_MAP: &'static [(i16, u32)] = &[(1, synth_params::MOD)];
    const LATENCY_PARAM: Option<u32> = None;

    fn new() -> Self {
        Self {
            state: UnsafeCell::new(SynthState {
                sample_rate: 48_000.0,
                voices: [NO_VOICE; 16],
            }),
        }
    }

    fn setup(&self, sample_rate: f64, _max_block: i32) {
        // SAFETY: called while the component is inactive; nothing else touches the state.
        let state = unsafe { &mut *self.state.get() };
        state.sample_rate = sample_rate;
        state.voices = [NO_VOICE; 16];
    }

    fn latency(&self, _shared: &Shared) -> u32 {
        0
    }

    unsafe fn process(&self, shared: &Shared, data: &ProcessData) {
        // SAFETY: only the audio thread runs process.
        let state = unsafe { &mut *self.state.get() };
        let frames = data.numSamples.max(0) as usize;
        // Gain 0.5 is unity. Mod wheel fully up halves the level.
        let gain = (shared.value(synth_params::GAIN) * 2.0) as f32
            * (1.0 - 0.5 * shared.value(synth_params::MOD)) as f32;

        let mut main_l = unsafe { channel(data.outputs, data.numOutputs, 0, 0, frames) };
        let mut main_r = unsafe { channel(data.outputs, data.numOutputs, 0, 1, frames) };
        let aux_on = shared.output_active(1);
        let mut aux_l = unsafe { channel(data.outputs, data.numOutputs, 1, 0, frames) };
        let mut aux_r = unsafe { channel(data.outputs, data.numOutputs, 1, 1, frames) };
        for buffer in [&mut main_l, &mut main_r, &mut aux_l, &mut aux_r]
            .into_iter()
            .flatten()
        {
            buffer.fill(0.0);
        }

        // Pull the block's note events onto the stack.
        let mut events: [Option<Event>; 128] = [None; 128];
        let mut event_count = 0;
        if let Some(list) = unsafe { ComRef::from_raw(data.inputEvents) } {
            let available = unsafe { list.getEventCount() }.max(0) as usize;
            for i in 0..available.min(events.len()) {
                let mut event: Event = unsafe { std::mem::zeroed() };
                if unsafe { list.getEvent(i as i32, &mut event) } == kResultOk {
                    events[event_count] = Some(event);
                    event_count += 1;
                }
            }
        }
        let out_events = unsafe { ComRef::from_raw(data.outputEvents) };

        let mut next_event = 0;
        let mut peak = 0.0f32;
        for frame in 0..frames {
            while next_event < event_count {
                let mut event = events[next_event].unwrap();
                if event.sampleOffset as usize > frame {
                    break;
                }
                next_event += 1;
                match event.r#type {
                    EVENT_NOTE_ON => {
                        let note = unsafe { event.__field0.noteOn };
                        let slot = state.voices.iter().position(|v| !v.active).unwrap_or(0);
                        let hz = 440.0 * 2f64.powf((note.pitch as f64 - 69.0) / 12.0);
                        state.voices[slot] = Voice {
                            active: true,
                            key: note.pitch,
                            channel: note.channel,
                            phase: 0.0,
                            increment: hz / state.sample_rate,
                            amplitude: note.velocity * 0.25,
                        };
                        if let Some(out) = &out_events {
                            unsafe { out.addEvent(&mut event) };
                        }
                    }
                    EVENT_NOTE_OFF => {
                        let note = unsafe { event.__field0.noteOff };
                        for voice in state.voices.iter_mut() {
                            if voice.active
                                && voice.key == note.pitch
                                && voice.channel == note.channel
                            {
                                voice.active = false;
                            }
                        }
                        if let Some(out) = &out_events {
                            unsafe { out.addEvent(&mut event) };
                        }
                    }
                    _ => {}
                }
            }

            let mut main = 0.0f32;
            let mut aux = 0.0f32;
            for voice in state.voices.iter_mut().filter(|v| v.active) {
                let sample =
                    (voice.phase * std::f64::consts::TAU).sin() as f32 * voice.amplitude * gain;
                voice.phase += voice.increment;
                if voice.phase >= 1.0 {
                    voice.phase -= 1.0;
                }
                if aux_on && voice.channel >= 1 {
                    aux += sample;
                } else {
                    main += sample;
                }
            }
            peak = peak.max(main.abs());
            if let Some(buffer) = main_l.as_deref_mut() {
                buffer[frame] = main;
            }
            if let Some(buffer) = main_r.as_deref_mut() {
                buffer[frame] = main;
            }
            if aux_on {
                if let Some(buffer) = aux_l.as_deref_mut() {
                    buffer[frame] = aux;
                }
                if let Some(buffer) = aux_r.as_deref_mut() {
                    buffer[frame] = aux;
                }
            }
        }

        unsafe {
            write_output_param(data, synth_params::METER, peak.min(1.0) as f64);
            if !data.processContext.is_null() {
                let context = &*data.processContext;
                write_output_param(data, synth_params::TEMPO_SEEN, context.tempo / 1000.0);
                let playing = context.state & (1 << 1) != 0;
                write_output_param(data, synth_params::PLAYING_SEEN, playing as u8 as f64);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Delay
// ---------------------------------------------------------------------------------------------

pub mod delay_params {
    /// Delay (and reported latency) in samples = round(value * 1024).
    pub const DELAY: u32 = 0;
    pub const GAIN: u32 = 1;
    pub const MAX_DELAY: usize = 1024;
}

struct DelayState {
    lines: [Vec<f32>; 2],
    write: usize,
}

/// A pure delay that reports its delay as latency, so a compensating host should make it
/// vanish. With the sidechain bus active, the sidechain signal is added to the output undelayed.
pub struct DelayKernel {
    state: UnsafeCell<DelayState>,
    delay_samples: AtomicU32,
}

// SAFETY: as for `SynthKernel`.
unsafe impl Sync for DelayKernel {}

const LINE_LEN: usize = 2048;

impl DelayKernel {
    fn delay_from(shared: &Shared) -> u32 {
        (shared.value(delay_params::DELAY).clamp(0.0, 1.0) * delay_params::MAX_DELAY as f64).round()
            as u32
    }
}

impl Kernel for DelayKernel {
    const NAME: &'static str = "Riddim Test Delay";
    const SUB_CATEGORIES: &'static str = "Fx|Delay";
    const CID: TUID = uid(0x52444D54, 0x444C5931, 0x50524F43, 0x00000001);
    const CONTROLLER_CID: TUID = uid(0x52444D54, 0x444C5931, 0x4354524C, 0x00000001);
    const STATE_MAGIC: [u8; 4] = *b"TDL1";
    const AUDIO_INS: &'static [BusSpec] = &[
        BusSpec {
            name: "Input",
            channels: 2,
            main: true,
            default_active: true,
        },
        BusSpec {
            name: "Sidechain",
            channels: 2,
            main: false,
            default_active: false,
        },
    ];
    const AUDIO_OUTS: &'static [BusSpec] = &[BusSpec {
        name: "Output",
        channels: 2,
        main: true,
        default_active: true,
    }];
    const EVENT_INS: i32 = 0;
    const EVENT_OUTS: i32 = 0;
    const PARAMS: &'static [ParamSpec] = &[
        ParamSpec {
            id: delay_params::DELAY,
            title: "Delay",
            units: "smp",
            default: 64.0 / delay_params::MAX_DELAY as f64,
            steps: delay_params::MAX_DELAY as i32,
            flags: PARAM_CAN_AUTOMATE,
            stored: true,
        },
        ParamSpec {
            id: delay_params::GAIN,
            title: "Gain",
            units: "",
            default: 0.5,
            steps: 0,
            flags: PARAM_CAN_AUTOMATE,
            stored: true,
        },
    ];
    const MIDI_MAP: &'static [(i16, u32)] = &[];
    const LATENCY_PARAM: Option<u32> = Some(delay_params::DELAY);

    fn new() -> Self {
        Self {
            state: UnsafeCell::new(DelayState {
                lines: [vec![0.0; LINE_LEN], vec![0.0; LINE_LEN]],
                write: 0,
            }),
            delay_samples: AtomicU32::new(64),
        }
    }

    fn setup(&self, _sample_rate: f64, _max_block: i32) {
        // SAFETY: called while inactive.
        let state = unsafe { &mut *self.state.get() };
        state.lines[0].fill(0.0);
        state.lines[1].fill(0.0);
        state.write = 0;
    }

    fn latency(&self, shared: &Shared) -> u32 {
        let delay = Self::delay_from(shared);
        self.delay_samples.store(delay, Ordering::Relaxed);
        delay
    }

    unsafe fn process(&self, shared: &Shared, data: &ProcessData) {
        // SAFETY: only the audio thread runs process.
        let state = unsafe { &mut *self.state.get() };
        let frames = data.numSamples.max(0) as usize;
        let delay = Self::delay_from(shared) as usize;
        let gain = (shared.value(delay_params::GAIN) * 2.0) as f32;
        let sidechain_on = shared.input_active(1);

        let start = state.write;
        for (c, line) in state.lines.iter_mut().enumerate() {
            let input = unsafe { channel(data.inputs, data.numInputs, 0, c, frames) };
            let sidechain = if sidechain_on {
                unsafe { channel(data.inputs, data.numInputs, 1, c, frames) }
            } else {
                None
            };
            let Some(output) = (unsafe { channel(data.outputs, data.numOutputs, 0, c, frames) })
            else {
                continue;
            };
            let mut write = start;
            for frame in 0..frames {
                line[write] = input.as_ref().map_or(0.0, |i| i[frame]);
                let read = (write + LINE_LEN - delay) % LINE_LEN;
                let mut sample = line[read] * gain;
                if let Some(sc) = &sidechain {
                    sample += sc[frame];
                }
                output[frame] = sample;
                write = (write + 1) % LINE_LEN;
            }
        }
        state.write = (start + frames) % LINE_LEN;
    }
}
