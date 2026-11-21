//! The audio side of a hosted VST3 plugin.
//!
//! Everything [`Vst3Processor::process`] touches is allocated in [`Vst3Processor::new`], on the
//! main thread. The block itself only fills fixed-size tables and calls the plugin.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, Event as VstEvent, Event__type0, IAudioProcessor,
    IAudioProcessorTrait, IEventList, IParameterChanges, LegacyMIDICCOutEvent, NoteOffEvent,
    NoteOnEvent, PolyPressureEvent, ProcessContext as VstProcessContext, ProcessData,
};
use vst3::Steinberg::kResultOk;
use vst3::{ComPtr, ComWrapper};

use super::com::{EventList, ParamChanges};
use crate::descriptor::{BusInfo, BusLayout};
use crate::event::{Event, EventBody};
use crate::instance::{ActivateConfig, PluginProcessor, ProcessMode};
use crate::process::{ProcessContext, ProcessStatus, TransportInfo};

const EVENT_NOTE_ON: u16 = 0;
const EVENT_NOTE_OFF: u16 = 1;
const EVENT_POLY_PRESSURE: u16 = 3;
const EVENT_LEGACY_MIDI_CC_OUT: u16 = 65535;

const CTRL_AFTERTOUCH: usize = 128;
const CTRL_PITCH_BEND: usize = 129;
const CTRL_PROGRAM_CHANGE: usize = 130;

// ProcessContext::StatesAndFlags
const STATE_PLAYING: u32 = 1 << 1;
const STATE_CYCLE_ACTIVE: u32 = 1 << 2;
const STATE_RECORDING: u32 = 1 << 3;
const STATE_PROJECT_TIME_MUSIC_VALID: u32 = 1 << 9;
const STATE_TEMPO_VALID: u32 = 1 << 10;
const STATE_BAR_POSITION_VALID: u32 = 1 << 11;
const STATE_CYCLE_VALID: u32 = 1 << 12;
const STATE_TIME_SIG_VALID: u32 = 1 << 13;
const STATE_CONT_TIME_VALID: u32 = 1 << 17;

/// Events per block, in each direction.
const MAX_EVENTS: usize = 1024;
/// Distinct parameters that can change within one block, in each direction.
const MAX_PARAM_QUEUES: usize = 256;

/// Which MIDI controllers the plugin wants delivered as parameter changes
/// (`IMidiMapping`). Written by the main thread, read by the audio thread, lock-free.
pub struct MidiMap {
    table: Box<[AtomicU32]>,
}

impl MidiMap {
    pub const CHANNELS: usize = 16;
    /// 0..=127 are MIDI CCs; 128 aftertouch, 129 pitch bend, 130 program change.
    pub const CONTROLLERS: usize = 131;
    const NONE: u32 = u32::MAX;

    pub fn new() -> Self {
        let table = (0..Self::CHANNELS * Self::CONTROLLERS).map(|_| AtomicU32::new(Self::NONE));
        Self { table: table.collect() }
    }

    pub fn clear(&self) {
        for slot in self.table.iter() {
            slot.store(Self::NONE, Ordering::Relaxed);
        }
    }

    pub fn set(&self, channel: usize, controller: usize, param: Option<u32>) {
        self.table[channel * Self::CONTROLLERS + controller]
            .store(param.unwrap_or(Self::NONE), Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self, channel: usize, controller: usize) -> Option<u32> {
        let id = self.table.get(channel * Self::CONTROLLERS + controller)?.load(Ordering::Relaxed);
        (id != Self::NONE).then_some(id)
    }
}

impl Default for MidiMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Where one bus's channel pointers live in the flat pointer table.
struct BusSlot {
    first: usize,
    channels: usize,
    active: bool,
}

/// The audio side of a [`Vst3Instance`](super::Vst3Instance).
pub struct Vst3Processor {
    processor: ComPtr<IAudioProcessor>,
    midi_map: Arc<MidiMap>,
    in_events: ComWrapper<EventList>,
    out_events: ComWrapper<EventList>,
    in_params: ComWrapper<ParamChanges>,
    out_params: ComWrapper<ParamChanges>,
    in_buses: Vec<AudioBusBuffers>,
    out_buses: Vec<AudioBusBuffers>,
    in_slots: Vec<BusSlot>,
    out_slots: Vec<BusSlot>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    /// Zeros fed to inactive input buses.
    silence: Vec<f32>,
    /// Somewhere harmless for the plugin to write inactive output buses.
    scratch: Vec<f32>,
    context: VstProcessContext,
    process_mode: i32,
    max_block: usize,
}

// SAFETY: a VST3 audio processor is designed to have `process` called from the audio thread
// while the rest of the component stays on the main thread. The host objects it holds are only
// used from inside `process`.
unsafe impl Send for Vst3Processor {}

fn slots(buses: &[BusInfo]) -> (Vec<BusSlot>, usize) {
    let mut first = 0;
    let slots = buses
        .iter()
        .map(|bus| {
            let slot = BusSlot { first, channels: bus.channels as usize, active: bus.active };
            first += slot.channels;
            slot
        })
        .collect();
    (slots, first)
}

fn empty_bus() -> AudioBusBuffers {
    AudioBusBuffers {
        numChannels: 0,
        silenceFlags: 0,
        __field0: AudioBusBuffers__type0 { channelBuffers32: std::ptr::null_mut() },
    }
}

impl Vst3Processor {
    pub(super) fn new(
        processor: ComPtr<IAudioProcessor>,
        layout: &BusLayout,
        config: &ActivateConfig,
        midi_map: Arc<MidiMap>,
    ) -> Self {
        let max_block = config.max_block as usize;
        let (in_slots, in_channels) = slots(&layout.audio_inputs);
        let (out_slots, out_channels) = slots(&layout.audio_outputs);
        // SAFETY: an all-zero ProcessContext is a valid "nothing is valid" context.
        let mut context: VstProcessContext = unsafe { std::mem::zeroed() };
        context.sampleRate = config.sample_rate;
        context.tempo = 120.0;
        context.timeSigNumerator = 4;
        context.timeSigDenominator = 4;
        Self {
            processor,
            midi_map,
            in_events: EventList::with_capacity(MAX_EVENTS),
            out_events: EventList::with_capacity(MAX_EVENTS),
            in_params: ParamChanges::with_capacity(MAX_PARAM_QUEUES),
            out_params: ParamChanges::with_capacity(MAX_PARAM_QUEUES),
            in_buses: in_slots.iter().map(|_| empty_bus()).collect(),
            out_buses: out_slots.iter().map(|_| empty_bus()).collect(),
            in_slots,
            out_slots,
            in_ptrs: vec![std::ptr::null_mut(); in_channels],
            out_ptrs: vec![std::ptr::null_mut(); out_channels],
            silence: vec![0.0; max_block.max(1)],
            scratch: vec![0.0; max_block.max(1)],
            context,
            process_mode: match config.mode {
                ProcessMode::Realtime => 0,
                ProcessMode::Offline => 2,
            },
            max_block,
        }
    }

    /// Translates host events into the plugin's event list and parameter queues.
    fn queue_inputs(&mut self, events: &[Event]) {
        // SAFETY: we are the only thread using these lists; the plugin is not running.
        let list = unsafe { self.in_events.events() };
        list.clear();
        self.in_params.clear();

        for event in events {
            let offset = event.offset.min(self.max_block.saturating_sub(1) as u32) as i32;
            match event.body {
                EventBody::NoteOn { channel, key, velocity, note_id } => {
                    push_event(list, offset, EVENT_NOTE_ON, Event__type0 {
                        noteOn: NoteOnEvent {
                            channel: channel as i16,
                            pitch: key as i16,
                            tuning: 0.0,
                            velocity,
                            length: 0,
                            noteId: note_id,
                        },
                    });
                }
                EventBody::NoteOff { channel, key, velocity, note_id } => {
                    push_event(list, offset, EVENT_NOTE_OFF, Event__type0 {
                        noteOff: NoteOffEvent {
                            channel: channel as i16,
                            pitch: key as i16,
                            velocity,
                            noteId: note_id,
                            tuning: 0.0,
                        },
                    });
                }
                EventBody::Midi { data, .. } => {
                    let channel = (data[0] & 0x0F) as usize;
                    // VST3 has no raw MIDI input: controllers arrive as parameter changes on
                    // whatever parameter the plugin mapped them to, if any.
                    let mapped = match data[0] & 0xF0 {
                        0xB0 => Some((data[1] as usize, data[2] as f64 / 127.0)),
                        0xD0 => Some((CTRL_AFTERTOUCH, data[1] as f64 / 127.0)),
                        0xE0 => {
                            let bend = ((data[2] as u32) << 7) | data[1] as u32;
                            Some((CTRL_PITCH_BEND, bend as f64 / 16383.0))
                        }
                        0xC0 => Some((CTRL_PROGRAM_CHANGE, data[1] as f64 / 127.0)),
                        0xA0 => {
                            push_event(list, offset, EVENT_POLY_PRESSURE, Event__type0 {
                                polyPressure: PolyPressureEvent {
                                    channel: channel as i16,
                                    pitch: data[1] as i16,
                                    pressure: data[2] as f32 / 127.0,
                                    noteId: -1,
                                },
                            });
                            None
                        }
                        _ => None,
                    };
                    if let Some((controller, value)) = mapped {
                        if let Some(param) = self.midi_map.get(channel, controller) {
                            self.in_params.add(param, offset, value);
                        }
                    }
                }
                EventBody::Param { id, value } => {
                    self.in_params.add(id, offset, value.clamp(0.0, 1.0));
                }
            }
        }
    }

    /// Translates what the plugin emitted back into host events.
    fn collect_outputs(&mut self, context: &mut ProcessContext<'_>) {
        // SAFETY: the plugin has returned from `process`; we are the only user again.
        let list = unsafe { self.out_events.events() };
        for event in list.iter() {
            let offset = event.sampleOffset.max(0) as u32;
            // SAFETY: the union member read in each arm is the one `type` says is active.
            let body = unsafe {
                match event.r#type {
                    EVENT_NOTE_ON => {
                        let e = event.__field0.noteOn;
                        Some(EventBody::NoteOn {
                            channel: e.channel as u8,
                            key: e.pitch as u8,
                            velocity: e.velocity,
                            note_id: e.noteId,
                        })
                    }
                    EVENT_NOTE_OFF => {
                        let e = event.__field0.noteOff;
                        Some(EventBody::NoteOff {
                            channel: e.channel as u8,
                            key: e.pitch as u8,
                            velocity: e.velocity,
                            note_id: e.noteId,
                        })
                    }
                    EVENT_LEGACY_MIDI_CC_OUT => legacy_cc_to_midi(event.__field0.midiCCOut),
                    _ => None,
                }
            };
            if let Some(body) = body {
                context.events_out.push(Event { offset, body });
            }
        }
        for queue in self.out_params.used() {
            // SAFETY: as above.
            if let Some((offset, value)) = unsafe { queue.last() } {
                context.events_out.push(Event::param(offset.max(0) as u32, queue.id(), value));
            }
        }
    }

    fn fill_context(&mut self, t: &TransportInfo) {
        let c = &mut self.context;
        let mut state = STATE_TEMPO_VALID
            | STATE_TIME_SIG_VALID
            | STATE_PROJECT_TIME_MUSIC_VALID
            | STATE_BAR_POSITION_VALID
            | STATE_CONT_TIME_VALID;
        if t.playing {
            state |= STATE_PLAYING;
        }
        if t.recording {
            state |= STATE_RECORDING;
        }
        if t.looping {
            state |= STATE_CYCLE_ACTIVE | STATE_CYCLE_VALID;
        }
        c.state = state;
        c.sampleRate = t.sample_rate;
        c.projectTimeSamples = t.position_samples;
        c.continousTimeSamples = t.continuous_samples;
        c.projectTimeMusic = t.position_quarters;
        c.barPositionMusic = t.bar_start_quarters;
        c.cycleStartMusic = t.loop_start_quarters;
        c.cycleEndMusic = t.loop_end_quarters;
        c.tempo = t.tempo_bpm;
        c.timeSigNumerator = t.time_sig_numerator as i32;
        c.timeSigDenominator = t.time_sig_denominator as i32;
    }
}

#[inline]
fn push_event(list: &mut Vec<VstEvent>, offset: i32, kind: u16, body: Event__type0) {
    if list.len() < list.capacity() {
        list.push(VstEvent {
            busIndex: 0,
            sampleOffset: offset,
            ppqPosition: 0.0,
            flags: 0,
            r#type: kind,
            __field0: body,
        });
    }
}

/// Converts the VST3 "legacy MIDI CC out" event into a raw MIDI message.
fn legacy_cc_to_midi(e: LegacyMIDICCOutEvent) -> Option<EventBody> {
    let channel = (e.channel as u8) & 0x0F;
    let value = (e.value as u8) & 0x7F;
    let value2 = (e.value2 as u8) & 0x7F;
    let (data, len) = match e.controlNumber as usize {
        cc @ 0..=127 => ([0xB0 | channel, cc as u8, value], 3),
        CTRL_AFTERTOUCH => ([0xD0 | channel, value, 0], 2),
        CTRL_PITCH_BEND => ([0xE0 | channel, value, value2], 3),
        CTRL_PROGRAM_CHANGE => ([0xC0 | channel, value, 0], 2),
        _ => return None,
    };
    Some(EventBody::Midi { data, len })
}

impl PluginProcessor for Vst3Processor {
    fn process(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus {
        let frames = context.frames;
        if frames > self.max_block {
            for channel in context.outputs.iter_mut() {
                channel.fill(0.0);
            }
            return ProcessStatus::Failed;
        }

        self.queue_inputs(context.events_in);
        // SAFETY: we are the only thread using the output lists; the plugin is not running.
        unsafe { self.out_events.events() }.clear();
        self.out_params.clear();
        self.fill_context(context.transport);

        // Point every bus at real memory: host buffers for active buses, shared silence or
        // scratch for inactive ones. Plugins differ on whether they check activation before
        // touching a bus, so nothing is ever left null.
        let mut next = 0;
        for (bus, slot) in self.in_buses.iter_mut().zip(&self.in_slots) {
            for c in 0..slot.channels {
                let src = if slot.active { context.inputs.get(next) } else { None };
                self.in_ptrs[slot.first + c] = match src {
                    Some(channel) => {
                        next += 1;
                        channel.as_ptr() as *mut f32
                    }
                    None => self.silence.as_ptr() as *mut f32,
                };
            }
            bus.numChannels = slot.channels as i32;
            bus.silenceFlags = if slot.active { 0 } else { u64::MAX };
            // SAFETY: `first` is within `in_ptrs` by construction in `slots`.
            bus.__field0.channelBuffers32 = unsafe { self.in_ptrs.as_mut_ptr().add(slot.first) };
        }
        let mut next = 0;
        for (bus, slot) in self.out_buses.iter_mut().zip(&self.out_slots) {
            for c in 0..slot.channels {
                let dst = if slot.active { context.outputs.get_mut(next) } else { None };
                self.out_ptrs[slot.first + c] = match dst {
                    Some(channel) => {
                        next += 1;
                        channel.as_mut_ptr()
                    }
                    None => self.scratch.as_mut_ptr(),
                };
            }
            bus.numChannels = slot.channels as i32;
            bus.silenceFlags = 0;
            // SAFETY: as for the inputs.
            bus.__field0.channelBuffers32 = unsafe { self.out_ptrs.as_mut_ptr().add(slot.first) };
        }

        let mut data = ProcessData {
            processMode: self.process_mode,
            symbolicSampleSize: 0,
            numSamples: frames as i32,
            numInputs: self.in_buses.len() as i32,
            numOutputs: self.out_buses.len() as i32,
            inputs: if self.in_buses.is_empty() {
                std::ptr::null_mut()
            } else {
                self.in_buses.as_mut_ptr()
            },
            outputs: if self.out_buses.is_empty() {
                std::ptr::null_mut()
            } else {
                self.out_buses.as_mut_ptr()
            },
            inputParameterChanges: self
                .in_params
                .as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputParameterChanges: self
                .out_params
                .as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            inputEvents: self
                .in_events
                .as_com_ref::<IEventList>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputEvents: self
                .out_events
                .as_com_ref::<IEventList>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            processContext: &mut self.context,
        };

        // What the plugin does inside its own process call is outside our real-time guarantee.
        // SAFETY: every pointer in `data` refers to memory that outlives the call, and each
        // channel pointer addresses at least `frames` samples.
        let result =
            riddim_rt::guard::permit(|| unsafe { self.processor.process(&mut data) });

        if result != kResultOk {
            for channel in context.outputs.iter_mut() {
                channel.fill(0.0);
            }
            return ProcessStatus::Failed;
        }
        self.collect_outputs(context);
        ProcessStatus::Ok
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
