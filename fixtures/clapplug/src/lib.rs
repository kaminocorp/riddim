//! Test fixture: two small deterministic CLAP plugins.
//!
//! They are written straight against the C ABI (`clap-sys`), not with the library the host is
//! built on, so that the host under test and the plugins it is tested with share no code: a
//! misreading of the specification would have to be made twice, in two different ways, to
//! cancel out.
//!
//! * **Riddim CLAP Synth** (`dev.riddim.test.clap-synth`): no input, one stereo output, one
//!   note port. A sine per held key at a quarter of full scale times the velocity, starting at
//!   phase zero on the exact sample of the note-on and stopping on the exact sample of the
//!   note-off. A gain parameter with a plain range of 0 to 2, and a read-only one that is 1
//!   after the host has said (through the render extension) that it is rendering offline.
//! * **Riddim CLAP Delay** (`dev.riddim.test.clap-delay`): stereo in, stereo out, delayed by
//!   exactly [`DELAY_LATENCY`] samples, which it reports as its latency. The same gain
//!   parameter.
//!
//! Both keep their gain as their state, apply parameter events on the sample they are for,
//! and never touch the heap or a lock in `process`.

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_note, clap_event_param_value, clap_input_events,
    clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_CHOKE,
    CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_STEREO,
};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::note_ports::{
    clap_note_port_info, clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP,
    CLAP_NOTE_DIALECT_MIDI,
};
use clap_sys::ext::params::{
    clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE,
    CLAP_PARAM_IS_READONLY,
};
use clap_sys::ext::render::{
    clap_plugin_render, clap_plugin_render_mode, CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE,
    CLAP_RENDER_REALTIME,
};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{
    clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR,
};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;

const SYNTH_ID: &CStr = c"dev.riddim.test.clap-synth";
const DELAY_ID: &CStr = c"dev.riddim.test.clap-delay";

/// The synth's gain parameter. Plain range 0 to 2, default 1.
const SYNTH_GAIN: clap_id = 100;
/// The synth's read-only parameter: 1 once the host has announced an offline render, else 0.
const SYNTH_OFFLINE: clap_id = 101;
/// The delay's gain parameter. The same range.
const DELAY_GAIN: clap_id = 7;
const GAIN_MAX: f64 = 2.0;
const GAIN_DEFAULT: f64 = 1.0;

/// How many samples the delay delays by, and reports.
const DELAY_LATENCY: usize = 32;
const VOICES: usize = 16;
/// A held key sounds at this amplitude times its velocity.
const AMPLITUDE: f64 = 0.25;

// ---------------------------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------------------------

/// A null-terminated list of C strings that can live in a static.
struct Features<const N: usize>([*const c_char; N]);
// SAFETY: the pointers are to string literals, which are immutable and live for ever.
unsafe impl<const N: usize> Sync for Features<N> {}

static SYNTH_FEATURES: Features<4> = Features([
    c"instrument".as_ptr(),
    c"synthesizer".as_ptr(),
    c"stereo".as_ptr(),
    ptr::null(),
]);
static DELAY_FEATURES: Features<4> = Features([
    c"audio-effect".as_ptr(),
    c"delay".as_ptr(),
    c"stereo".as_ptr(),
    ptr::null(),
]);

const fn descriptor(
    id: &'static CStr,
    name: &'static CStr,
    description: &'static CStr,
    features: *const *const c_char,
) -> clap_plugin_descriptor {
    clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: id.as_ptr(),
        name: name.as_ptr(),
        vendor: c"Riddim tests".as_ptr(),
        url: c"".as_ptr(),
        manual_url: c"".as_ptr(),
        support_url: c"".as_ptr(),
        version: c"1.0.0".as_ptr(),
        description: description.as_ptr(),
        features,
    }
}

static SYNTH_DESCRIPTOR: clap_plugin_descriptor = descriptor(
    SYNTH_ID,
    c"Riddim CLAP Synth",
    c"A sine per held key, sample-accurate",
    SYNTH_FEATURES.0.as_ptr(),
);
static DELAY_DESCRIPTOR: clap_plugin_descriptor = descriptor(
    DELAY_ID,
    c"Riddim CLAP Delay",
    c"Delays by exactly its reported latency",
    DELAY_FEATURES.0.as_ptr(),
);

// ---------------------------------------------------------------------------------------------
// The plugins
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Synth,
    Delay,
}

impl Kind {
    fn gain_id(self) -> clap_id {
        match self {
            Kind::Synth => SYNTH_GAIN,
            Kind::Delay => DELAY_GAIN,
        }
    }

    /// What a saved state of this plugin starts with, so that the other's is refused.
    fn magic(self) -> [u8; 4] {
        match self {
            Kind::Synth => *b"RCS1",
            Kind::Delay => *b"RCD1",
        }
    }
}

#[derive(Clone, Copy)]
struct Voice {
    key: i16,
    channel: i16,
    amplitude: f64,
    phase: f64,
    step: f64,
    on: bool,
}

const SILENT: Voice = Voice {
    key: 0,
    channel: 0,
    amplitude: 0.0,
    phase: 0.0,
    step: 0.0,
    on: false,
};

/// What only the audio thread touches (and the main thread while the plugin is not
/// processing, which is when `activate` and `reset` are called).
struct Audio {
    sample_rate: f64,
    voices: [Voice; VOICES],
    line: [[f32; DELAY_LATENCY]; 2],
    position: usize,
}

#[repr(C)]
struct Plugin {
    /// First, so that a pointer to it is a pointer to the whole.
    clap: clap_plugin,
    kind: Kind,
    /// The gain, as the bits of an `f64`: read by the main thread, written by whichever
    /// thread is handed parameter events.
    gain: AtomicU64,
    /// Whether the host last announced an offline render.
    offline: AtomicBool,
    audio: UnsafeCell<Audio>,
}

impl Plugin {
    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn set_gain(&self, value: f64) {
        self.gain
            .store(value.clamp(0.0, GAIN_MAX).to_bits(), Ordering::Relaxed);
    }
}

/// The plugin behind a `clap_plugin` pointer the host hands back.
///
/// # Safety
/// `plugin` came from [`create_plugin`] and has not been destroyed.
unsafe fn this<'a>(plugin: *const clap_plugin) -> &'a Plugin {
    // SAFETY: `plugin_data` was set to the `Plugin` that holds this `clap_plugin`.
    unsafe { &*((*plugin).plugin_data as *const Plugin) }
}

unsafe extern "C" fn init(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    // SAFETY: the host destroys a plugin once, and does not use it again.
    drop(unsafe { Box::from_raw((*plugin).plugin_data as *mut Plugin) });
}

unsafe extern "C" fn activate(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames: u32,
    _max_frames: u32,
) -> bool {
    // SAFETY: the host does not process while it activates.
    let audio = unsafe { &mut *this(plugin).audio.get() };
    audio.sample_rate = sample_rate;
    audio.voices = [SILENT; VOICES];
    audio.line = [[0.0; DELAY_LATENCY]; 2];
    audio.position = 0;
    true
}

unsafe extern "C" fn deactivate(_plugin: *const clap_plugin) {}

unsafe extern "C" fn start_processing(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn stop_processing(_plugin: *const clap_plugin) {}

unsafe extern "C" fn reset(plugin: *const clap_plugin) {
    // SAFETY: `reset` is an audio-thread call, never made during `process`.
    let audio = unsafe { &mut *this(plugin).audio.get() };
    audio.voices = [SILENT; VOICES];
    audio.line = [[0.0; DELAY_LATENCY]; 2];
    audio.position = 0;
}

unsafe extern "C" fn on_main_thread(_plugin: *const clap_plugin) {}

/// Applies one event. Notes only mean something to the synth.
///
/// # Safety
/// `header` points to a whole event of the type and size it claims.
unsafe fn apply(plugin: &Plugin, audio: Option<&mut Audio>, header: *const clap_event_header) {
    // SAFETY: per the contract above; each cast is to the struct its type names.
    unsafe {
        if (*header).space_id != CLAP_CORE_EVENT_SPACE_ID {
            return;
        }
        match (*header).type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let event = &*(header as *const clap_event_param_value);
                if event.param_id == plugin.kind.gain_id() {
                    plugin.set_gain(event.value);
                }
            }
            CLAP_EVENT_NOTE_ON => {
                let event = &*(header as *const clap_event_note);
                if let Some(audio) = audio {
                    note_on(audio, event.channel, event.key, event.velocity);
                }
            }
            CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE => {
                let event = &*(header as *const clap_event_note);
                if let Some(audio) = audio {
                    note_off(audio, event.channel, event.key);
                }
            }
            CLAP_EVENT_MIDI => {
                let event = &*(header as *const clap_event_midi);
                let [status, key, velocity] = event.data;
                let channel = (status & 0x0F) as i16;
                if let Some(audio) = audio {
                    match status & 0xF0 {
                        0x90 if velocity > 0 => {
                            note_on(audio, channel, key as i16, velocity as f64 / 127.0)
                        }
                        0x80 | 0x90 => note_off(audio, channel, key as i16),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn note_on(audio: &mut Audio, channel: i16, key: i16, velocity: f64) {
    let Some(voice) = audio.voices.iter_mut().find(|voice| !voice.on) else {
        return;
    };
    let frequency = 440.0 * 2f64.powf((key as f64 - 69.0) / 12.0);
    *voice = Voice {
        key,
        channel,
        amplitude: AMPLITUDE * velocity,
        phase: 0.0,
        step: std::f64::consts::TAU * frequency / audio.sample_rate,
        on: true,
    };
}

fn note_off(audio: &mut Audio, channel: i16, key: i16) {
    for voice in &mut audio.voices {
        // A key or channel of -1 is CLAP's wildcard.
        if voice.on && (key < 0 || voice.key == key) && (channel < 0 || voice.channel == channel) {
            voice.on = false;
        }
    }
}

/// The channels of the first port of `buffers`, as slices of `frames` samples; null where the
/// host gave no such channel.
///
/// # Safety
/// `buffers` points to `count` buffers, each with `channel_count` channel pointers, each of
/// which is valid for `frames` samples.
unsafe fn channels(
    buffers: *const clap_audio_buffer,
    count: u32,
    frames: usize,
) -> Option<[*mut f32; 2]> {
    if buffers.is_null() || count == 0 {
        return None;
    }
    // SAFETY: per the contract above.
    unsafe {
        let buffer = &*buffers;
        if buffer.data32.is_null() || buffer.channel_count == 0 || frames == 0 {
            return None;
        }
        let left = *buffer.data32;
        // A mono port is played as two channels of the same.
        let right = if buffer.channel_count > 1 {
            *buffer.data32.add(1)
        } else {
            left
        };
        (!left.is_null() && !right.is_null()).then_some([left, right])
    }
}

unsafe extern "C" fn process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    // SAFETY: the host hands us a valid process structure, and calls `process` from one
    // thread at a time, which is what makes the audio state ours.
    unsafe {
        let plugin = this(plugin);
        let audio = &mut *plugin.audio.get();
        let process = &*process;
        let frames = process.frames_count as usize;
        let events = process.in_events;
        let event_count = match events.as_ref().and_then(|events| events.size) {
            Some(size) => size(events),
            None => 0,
        };
        let event_at = |index: u32| -> *const clap_event_header {
            match (*events).get {
                Some(get) => get(events, index),
                None => ptr::null(),
            }
        };

        let outputs = channels(process.audio_outputs, process.audio_outputs_count, frames);
        let inputs = channels(process.audio_inputs, process.audio_inputs_count, frames);
        if frames > 0 && outputs.is_none() {
            return CLAP_PROCESS_ERROR;
        }

        // Render up to each event, apply it, and carry on: events land on their sample.
        let mut next_event = 0u32;
        let mut frame = 0usize;
        loop {
            let mut until = frames;
            while next_event < event_count {
                let header = event_at(next_event);
                if header.is_null() {
                    next_event += 1;
                    continue;
                }
                let time = ((*header).time as usize).min(frames);
                if time > frame {
                    until = time;
                    break;
                }
                apply(plugin, Some(audio), header);
                next_event += 1;
            }
            if let Some([left, right]) = outputs {
                let gain = plugin.gain();
                for n in frame..until {
                    match plugin.kind {
                        Kind::Synth => {
                            let mut sum = 0.0f64;
                            for voice in audio.voices.iter_mut().filter(|voice| voice.on) {
                                sum += voice.amplitude * voice.phase.sin();
                                voice.phase += voice.step;
                            }
                            let sample = (sum * gain) as f32;
                            *left.add(n) = sample;
                            *right.add(n) = sample;
                        }
                        Kind::Delay => {
                            // Read the input before writing the output: the host may hand
                            // us the same memory for both.
                            let (in_left, in_right) = match inputs {
                                Some([l, r]) => (*l.add(n), *r.add(n)),
                                None => (0.0, 0.0),
                            };
                            let position = audio.position;
                            *left.add(n) = (audio.line[0][position] as f64 * gain) as f32;
                            *right.add(n) = (audio.line[1][position] as f64 * gain) as f32;
                            audio.line[0][position] = in_left;
                            audio.line[1][position] = in_right;
                            audio.position = (position + 1) % DELAY_LATENCY;
                        }
                    }
                }
            }
            frame = until;
            if frame >= frames && next_event >= event_count {
                break;
            }
        }
        CLAP_PROCESS_CONTINUE
    }
}

// ---------------------------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------------------------

fn write_name<const N: usize>(out: &mut [c_char; N], text: &CStr) {
    let bytes = text.to_bytes();
    let length = bytes.len().min(N - 1);
    for (slot, byte) in out.iter_mut().zip(&bytes[..length]) {
        *slot = *byte as c_char;
    }
    out[length] = 0;
}

unsafe extern "C" fn audio_ports_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
    // SAFETY: a live plugin of ours.
    match (unsafe { this(plugin) }.kind, is_input) {
        (Kind::Synth, true) => 0,
        _ => 1,
    }
}

unsafe extern "C" fn audio_ports_get(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    // SAFETY: a live plugin of ours, and `info` is the host's to fill.
    unsafe {
        if index >= audio_ports_count(plugin, is_input) || info.is_null() {
            return false;
        }
        let info = &mut *info;
        info.id = if is_input { 0 } else { 1 };
        write_name(&mut info.name, if is_input { c"Input" } else { c"Output" });
        info.flags = CLAP_AUDIO_PORT_IS_MAIN;
        info.channel_count = 2;
        info.port_type = CLAP_PORT_STEREO.as_ptr();
        info.in_place_pair = CLAP_INVALID_ID;
        true
    }
}

static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(audio_ports_count),
    get: Some(audio_ports_get),
};

unsafe extern "C" fn note_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    is_input as u32
}

unsafe extern "C" fn note_ports_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    if index != 0 || !is_input || info.is_null() {
        return false;
    }
    // SAFETY: `info` is the host's to fill.
    let info = unsafe { &mut *info };
    info.id = 0;
    info.supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
    info.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
    write_name(&mut info.name, c"Notes");
    true
}

static NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
    count: Some(note_ports_count),
    get: Some(note_ports_get),
};

unsafe extern "C" fn params_count(plugin: *const clap_plugin) -> u32 {
    // SAFETY: a live plugin of ours.
    match unsafe { this(plugin) }.kind {
        Kind::Synth => 2,
        Kind::Delay => 1,
    }
}

unsafe extern "C" fn params_get_info(
    plugin: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    // SAFETY: a live plugin of ours, and `info` is the host's to fill.
    unsafe {
        if index >= params_count(plugin) || info.is_null() {
            return false;
        }
        let info = &mut *info;
        info.cookie = ptr::null_mut();
        write_name(&mut info.module, c"");
        info.min_value = 0.0;
        if index == 0 {
            info.id = this(plugin).kind.gain_id();
            info.flags = CLAP_PARAM_IS_AUTOMATABLE;
            write_name(&mut info.name, c"Gain");
            info.max_value = GAIN_MAX;
            info.default_value = GAIN_DEFAULT;
        } else {
            info.id = SYNTH_OFFLINE;
            info.flags = CLAP_PARAM_IS_READONLY;
            write_name(&mut info.name, c"Offline");
            info.max_value = 1.0;
            info.default_value = 0.0;
        }
    }
    true
}

unsafe extern "C" fn params_get_value(
    plugin: *const clap_plugin,
    id: clap_id,
    out: *mut f64,
) -> bool {
    // SAFETY: a live plugin of ours, and `out` is the host's to fill.
    unsafe {
        let plugin = this(plugin);
        if out.is_null() {
            return false;
        }
        if id == plugin.kind.gain_id() {
            *out = plugin.gain();
        } else if id == SYNTH_OFFLINE && plugin.kind == Kind::Synth {
            *out = plugin.offline.load(Ordering::Relaxed) as u8 as f64;
        } else {
            return false;
        }
    }
    true
}

unsafe extern "C" fn params_value_to_text(
    plugin: *const clap_plugin,
    id: clap_id,
    value: f64,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    // SAFETY: a live plugin of ours, and `out` has room for `capacity` bytes.
    unsafe {
        if id != this(plugin).kind.gain_id() || out.is_null() || capacity == 0 {
            return false;
        }
        let text = format!("{value:.2} x");
        let length = text.len().min(capacity as usize - 1);
        ptr::copy_nonoverlapping(text.as_ptr().cast::<c_char>(), out, length);
        *out.add(length) = 0;
    }
    true
}

unsafe extern "C" fn params_text_to_value(
    plugin: *const clap_plugin,
    id: clap_id,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    // SAFETY: a live plugin of ours; `text` is a C string and `out` the host's to fill.
    unsafe {
        if id != this(plugin).kind.gain_id() || text.is_null() || out.is_null() {
            return false;
        }
        let text = CStr::from_ptr(text).to_string_lossy();
        match text.trim().trim_end_matches('x').trim().parse::<f64>() {
            Ok(value) => {
                *out = value;
                true
            }
            Err(_) => false,
        }
    }
}

unsafe extern "C" fn params_flush(
    plugin: *const clap_plugin,
    input: *const clap_input_events,
    _output: *const clap_output_events,
) {
    // SAFETY: a live plugin of ours and a valid event list. Only parameters are applied:
    // `flush` may be called on the main thread, where the audio state is not ours.
    unsafe {
        let plugin = this(plugin);
        let Some(events) = input.as_ref() else {
            return;
        };
        let (Some(size), Some(get)) = (events.size, events.get) else {
            return;
        };
        for index in 0..size(input) {
            let header = get(input, index);
            if !header.is_null() {
                apply(plugin, None, header);
            }
        }
    }
}

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    // SAFETY: a live plugin of ours and a valid stream.
    unsafe {
        let plugin = this(plugin);
        let Some(write) = stream.as_ref().and_then(|stream| stream.write) else {
            return false;
        };
        let mut bytes = [0u8; 12];
        bytes[..4].copy_from_slice(&plugin.kind.magic());
        bytes[4..].copy_from_slice(&plugin.gain().to_le_bytes());
        // A stream may take less than it was offered.
        let mut done = 0usize;
        while done < bytes.len() {
            let written = write(
                stream,
                bytes[done..].as_ptr().cast::<c_void>(),
                (bytes.len() - done) as u64,
            );
            if written <= 0 {
                return false;
            }
            done += written as usize;
        }
    }
    true
}

unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    // SAFETY: a live plugin of ours and a valid stream.
    unsafe {
        let plugin = this(plugin);
        let Some(read) = stream.as_ref().and_then(|stream| stream.read) else {
            return false;
        };
        let mut bytes = [0u8; 12];
        let mut done = 0usize;
        while done < bytes.len() {
            let got = read(
                stream,
                bytes[done..].as_mut_ptr().cast::<c_void>(),
                (bytes.len() - done) as u64,
            );
            if got <= 0 {
                return false;
            }
            done += got as usize;
        }
        if bytes[..4] != plugin.kind.magic() {
            return false;
        }
        let mut gain = [0u8; 8];
        gain.copy_from_slice(&bytes[4..]);
        let gain = f64::from_le_bytes(gain);
        if !gain.is_finite() {
            return false;
        }
        plugin.set_gain(gain);
    }
    true
}

unsafe extern "C" fn render_has_hard_realtime_requirement(_plugin: *const clap_plugin) -> bool {
    false
}

unsafe extern "C" fn render_set(plugin: *const clap_plugin, mode: clap_plugin_render_mode) -> bool {
    let offline = match mode {
        CLAP_RENDER_OFFLINE => true,
        CLAP_RENDER_REALTIME => false,
        _ => return false,
    };
    // SAFETY: a live plugin of ours.
    unsafe { this(plugin) }
        .offline
        .store(offline, Ordering::Relaxed);
    true
}

static RENDER: clap_plugin_render = clap_plugin_render {
    has_hard_realtime_requirement: Some(render_has_hard_realtime_requirement),
    set: Some(render_set),
};

static STATE: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

unsafe extern "C" fn latency_get(_plugin: *const clap_plugin) -> u32 {
    DELAY_LATENCY as u32
}

static LATENCY: clap_plugin_latency = clap_plugin_latency {
    get: Some(latency_get),
};

unsafe extern "C" fn get_extension(plugin: *const clap_plugin, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY: a live plugin of ours, and `id` is a C string.
    let (kind, id) = unsafe { (this(plugin).kind, CStr::from_ptr(id)) };
    if id == CLAP_EXT_AUDIO_PORTS {
        (&raw const AUDIO_PORTS).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&raw const PARAMS).cast()
    } else if id == CLAP_EXT_STATE {
        (&raw const STATE).cast()
    } else if id == CLAP_EXT_RENDER {
        (&raw const RENDER).cast()
    } else if id == CLAP_EXT_NOTE_PORTS && kind == Kind::Synth {
        (&raw const NOTE_PORTS).cast()
    } else if id == CLAP_EXT_LATENCY && kind == Kind::Delay {
        (&raw const LATENCY).cast()
    } else {
        ptr::null()
    }
}

// ---------------------------------------------------------------------------------------------
// Factory and entry point
// ---------------------------------------------------------------------------------------------

unsafe extern "C" fn get_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
    2
}

unsafe extern "C" fn get_plugin_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    match index {
        0 => &raw const SYNTH_DESCRIPTOR,
        1 => &raw const DELAY_DESCRIPTOR,
        _ => ptr::null(),
    }
}

unsafe extern "C" fn create_plugin(
    _factory: *const clap_plugin_factory,
    _host: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY: `id` is a C string.
    let id = unsafe { CStr::from_ptr(id) };
    let (kind, descriptor) = if id == SYNTH_ID {
        (Kind::Synth, &raw const SYNTH_DESCRIPTOR)
    } else if id == DELAY_ID {
        (Kind::Delay, &raw const DELAY_DESCRIPTOR)
    } else {
        return ptr::null();
    };
    let plugin = Box::into_raw(Box::new(Plugin {
        clap: clap_plugin {
            desc: descriptor,
            plugin_data: ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        kind,
        gain: AtomicU64::new(GAIN_DEFAULT.to_bits()),
        offline: AtomicBool::new(false),
        audio: UnsafeCell::new(Audio {
            sample_rate: 48_000.0,
            voices: [SILENT; VOICES],
            line: [[0.0; DELAY_LATENCY]; 2],
            position: 0,
        }),
    }));
    // SAFETY: the box was just made; the host owns it until it calls `destroy`.
    unsafe {
        (*plugin).clap.plugin_data = plugin.cast();
        &raw const (*plugin).clap
    }
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(get_plugin_count),
    get_plugin_descriptor: Some(get_plugin_descriptor),
    create_plugin: Some(create_plugin),
};

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    // SAFETY: `id` is a C string when it is not null.
    if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        (&raw const FACTORY).cast()
    } else {
        ptr::null()
    }
}

/// What a CLAP host looks up in the library.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};
