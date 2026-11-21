//! The engine's C ABI, version 0.
//!
//! A small set of functions around an opaque engine handle. Anything structured (the
//! configuration, edit ops, the model, events) crosses the boundary as JSON, so a binding in
//! another language needs a JSON library and nothing else; the handful of hot calls (transport,
//! parameters, status) take plain values.
//!
//! The header, `include/riddim.h`, is generated from this file by cbindgen and checked in; a
//! test fails if the two drift apart.

#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::ptr;
use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use riddim::files::wav::SampleFormat;
use riddim::model::{AutomationTarget, Model, Op, SlotId, TrackId};
use riddim::plugins::EventBody;
use riddim::time::{samples_to_superclock, Position};
use riddim::{
    BounceRequest, DeviceConfig, Engine, EngineConfig, Event, MainThread, MainThreadMode,
    NullConfig, TransportCommand, TransportState,
};
use serde::Deserialize;

/// Version of this ABI. Bumped whenever a function or struct here changes incompatibly.
pub const RIDDIM_ABI_VERSION: u32 = 0;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(message: impl Into<String>) {
    let message = message.into().replace('\0', " ");
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::new(message).unwrap_or_default());
}

fn clear_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::default());
}

/// An engine. Opaque: create with `riddim_engine_new`, release with `riddim_engine_free`.
pub struct RiddimEngine {
    engine: Mutex<Engine>,
    events: Mutex<Receiver<Event>>,
    main_thread: Mutex<Option<MainThread>>,
}

/// Transport requests for `riddim_transport`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiddimTransportCommand {
    /// Start playback from the current position.
    RiddimPlay = 0,
    /// Stop (and end any recording).
    RiddimStop = 1,
    /// Move the playhead to the sample given as the argument.
    RiddimLocate = 2,
    /// Start recording on every armed track.
    RiddimRecord = 3,
}

/// Transport state in `RiddimStatus`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiddimTransportState {
    RiddimStopped = 0,
    RiddimPlaying = 1,
    RiddimRecording = 2,
}

/// A snapshot of what the engine is doing. Filled in by `riddim_status`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RiddimStatus {
    pub transport: RiddimTransportState,
    /// Timeline position in samples at the session rate.
    pub position_samples: i64,
    pub sample_rate: u32,
    pub block_frames: u32,
    /// Increases by one with every applied batch of ops.
    pub model_revision: u64,
    pub blocks: u64,
    pub xruns: u64,
    /// Heap operations seen on the audio thread (only measured in instrumented builds).
    pub rt_violations: u64,
    /// Recorded frames lost because the disk could not keep up.
    pub capture_dropped: u64,
    /// Recorded frames waiting in memory for the disk. Climbing means the disk is falling behind.
    pub capture_backlog: u64,
    /// Samples by which the output lags the timeline (plugin latency compensation).
    pub plan_latency: u32,
    pub input_latency: u32,
    pub output_latency: u32,
    /// The largest share of a block's duration that rendering one took over the last second
    /// or so: 1.0 is all of it.
    pub dsp_load: f32,
    /// Messages from the audio thread that were lost. Should stay at zero.
    pub events_lost: u64,
    /// Frames of files played from disk that were not read in time and played as silence.
    pub playback_starved: u64,
}

/// Engine configuration as JSON.
///
/// ```json
/// {
///   "device": "system",           // "system" | "null" | "manual"
///   "sample_rate": 48000,
///   "buffer_frames": 128,
///   "capture": true,              // open the device's inputs too ("system" only)
///   "capture_dir": "/path/to/project/media",
///   "media_dir": "/path/to/project/media",
///   "main_thread": "external",    // "external" (pump it yourself) | "internal"
///   "midi_input": true,
///   "offline": false,             // tell plugins this is an offline render
///   "render_threads": null,       // extra render threads; null picks for the device
///   "bridge_executable": null     // where riddim-bridge is, for bridged plugin slots
/// }
/// ```
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_device")]
    device: String,
    #[serde(default = "default_rate")]
    sample_rate: f64,
    #[serde(default = "default_buffer")]
    buffer_frames: u32,
    #[serde(default)]
    capture: bool,
    capture_dir: PathBuf,
    #[serde(default)]
    media_dir: Option<PathBuf>,
    #[serde(default = "default_main_thread")]
    main_thread: String,
    #[serde(default)]
    midi_input: bool,
    /// Tell plugins they are being rendered offline (bounces on the "manual" device).
    #[serde(default)]
    offline: bool,
    /// "null" and "manual" only: input and output channel counts.
    #[serde(default = "default_channels")]
    input_channels: u32,
    #[serde(default = "default_channels")]
    output_channels: u32,
    /// Threads that help the audio thread render. Absent or null: one fewer than the machine
    /// has cores (at most 7) on audio hardware, none on "null" and "manual".
    #[serde(default)]
    render_threads: Option<usize>,
    /// The `riddim-bridge` executable, for plugin slots marked `"bridged": true`. Absent or
    /// null: `$RIDDIM_BRIDGE`, then beside the running program.
    #[serde(default)]
    bridge_executable: Option<PathBuf>,
}

fn default_device() -> String {
    "system".into()
}
fn default_rate() -> f64 {
    48_000.0
}
fn default_buffer() -> u32 {
    128
}
fn default_main_thread() -> String {
    "external".into()
}
fn default_channels() -> u32 {
    2
}

fn engine_config(json: &str) -> Result<EngineConfig, String> {
    let config: Config = serde_json::from_str(json).map_err(|e| format!("configuration: {e}"))?;
    let null = NullConfig {
        sample_rate: config.sample_rate,
        buffer_frames: config.buffer_frames,
        input_channels: config.input_channels,
        output_channels: config.output_channels,
        ..NullConfig::default()
    };
    let device = match config.device.as_str() {
        "manual" => DeviceConfig::Manual(null),
        "null" => DeviceConfig::Null(null),
        "system" => DeviceConfig::System(riddim::devices::StreamConfig {
            device: None,
            sample_rate: config.sample_rate,
            buffer_frames: config.buffer_frames,
            capture: config.capture,
        }),
        other => return Err(format!("configuration: unknown device '{other}'")),
    };
    let main_thread = match config.main_thread.as_str() {
        "external" => MainThreadMode::External,
        "internal" => MainThreadMode::Internal,
        other => return Err(format!("configuration: unknown main_thread '{other}'")),
    };
    Ok(EngineConfig {
        device,
        media_dir: config
            .media_dir
            .unwrap_or_else(|| config.capture_dir.clone()),
        capture_dir: config.capture_dir,
        main_thread,
        butler: Default::default(),
        midi_input: config.midi_input,
        process_mode: if config.offline {
            riddim::plugins::ProcessMode::Offline
        } else {
            riddim::plugins::ProcessMode::Realtime
        },
        render_threads: config.render_threads,
        parallel_from: riddim::PARALLEL_FROM_STEPS,
        streaming: Default::default(),
        bridge_executable: config.bridge_executable,
    })
}

unsafe fn text<'a>(pointer: *const c_char, what: &str) -> Result<&'a str, String> {
    if pointer.is_null() {
        return Err(format!("{what} is NULL"));
    }
    // SAFETY: the caller passes a NUL-terminated string that outlives the call.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|_| format!("{what} is not UTF-8"))
}

fn owned(text: String) -> *mut c_char {
    CString::new(text.replace('\0', " "))
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

unsafe fn handle<'a>(engine: *const RiddimEngine) -> Option<&'a RiddimEngine> {
    if engine.is_null() {
        set_error("the engine handle is NULL");
    }
    // SAFETY: a non-null handle came from riddim_engine_new and has not been freed.
    unsafe { engine.as_ref() }
}

/// The ABI version this library implements.
#[no_mangle]
pub extern "C" fn riddim_abi_version() -> u32 {
    RIDDIM_ABI_VERSION
}

/// The message for the most recent failure on the calling thread, or an empty string. The
/// pointer is valid until the next call into this library on the same thread.
#[no_mangle]
pub extern "C" fn riddim_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Releases a string returned by this library. NULL is ignored.
#[no_mangle]
pub unsafe extern "C" fn riddim_string_free(string: *mut c_char) {
    if !string.is_null() {
        // SAFETY: the pointer came from CString::into_raw in this library.
        drop(unsafe { CString::from_raw(string) });
    }
}

/// Starts an engine from a JSON configuration. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn riddim_engine_new(config_json: *const c_char) -> *mut RiddimEngine {
    clear_error();
    let result = unsafe { text(config_json, "the configuration") }
        .and_then(engine_config)
        .and_then(|config| Engine::start(config).map_err(|e| e.to_string()));
    match result {
        Ok(mut engine) => {
            let events = engine.subscribe();
            let main_thread = engine.take_main_thread();
            Box::into_raw(Box::new(RiddimEngine {
                engine: Mutex::new(engine),
                events: Mutex::new(events),
                main_thread: Mutex::new(main_thread),
            }))
        }
        Err(message) => {
            set_error(message);
            ptr::null_mut()
        }
    }
}

/// Stops and releases an engine. NULL is ignored.
#[no_mangle]
pub unsafe extern "C" fn riddim_engine_free(engine: *mut RiddimEngine) {
    if !engine.is_null() {
        // SAFETY: the pointer came from riddim_engine_new and is not used again.
        drop(unsafe { Box::from_raw(engine) });
    }
}

/// Submits a batch of edit ops, given as a JSON array. Returns a ticket (never 0) that the
/// matching "applied" or "rejected" event will carry, or 0 if the JSON could not be read.
#[no_mangle]
pub unsafe extern "C" fn riddim_apply(engine: *mut RiddimEngine, ops_json: *const c_char) -> u64 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    let ops = unsafe { text(ops_json, "the ops") }
        .and_then(|json| serde_json::from_str::<Vec<Op>>(json).map_err(|e| format!("ops: {e}")));
    match ops {
        Ok(ops) => handle.engine.lock().unwrap().apply(ops),
        Err(message) => {
            set_error(message);
            0
        }
    }
}

/// Sends a transport command. `argument` is the target sample for `RIDDIM_LOCATE` and is
/// ignored otherwise. Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn riddim_transport(
    engine: *mut RiddimEngine,
    command: RiddimTransportCommand,
    argument: i64,
) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    let engine = handle.engine.lock().unwrap();
    let rate = engine.status().sample_rate;
    engine.transport(match command {
        RiddimTransportCommand::RiddimPlay => TransportCommand::Play,
        RiddimTransportCommand::RiddimStop => TransportCommand::Stop,
        RiddimTransportCommand::RiddimLocate => {
            TransportCommand::Locate(Position::Time(samples_to_superclock(argument, rate)))
        }
        RiddimTransportCommand::RiddimRecord => TransportCommand::Record,
    });
    1
}

/// Sets a plugin parameter to a normalised value (0 to 1). Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn riddim_set_param(
    engine: *mut RiddimEngine,
    slot: u64,
    parameter: u32,
    value: f64,
) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    handle
        .engine
        .lock()
        .unwrap()
        .set_param(SlotId(slot), parameter, value);
    1
}

/// Plays a note into a track's instrument: velocity above 0 is a note-on, 0 a note-off.
/// Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn riddim_play_note(
    engine: *mut RiddimEngine,
    track: u64,
    channel: u8,
    key: u8,
    velocity: f32,
) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    let body = if velocity > 0.0 {
        EventBody::NoteOn {
            channel: channel & 15,
            key: key & 127,
            velocity: velocity.min(1.0),
            note_id: -1,
        }
    } else {
        EventBody::NoteOff {
            channel: channel & 15,
            key: key & 127,
            velocity: 0.0,
            note_id: -1,
        }
    };
    handle
        .engine
        .lock()
        .unwrap()
        .play_note(TrackId(track), body);
    1
}

/// The current model as JSON. The caller frees the string. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn riddim_model_json(engine: *mut RiddimEngine) -> *mut c_char {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return ptr::null_mut();
    };
    let model = handle.engine.lock().unwrap().model();
    owned(model.to_json())
}

/// Replaces the whole model with one given as JSON (opening a project). Returns the new model
/// revision, or 0 on failure.
#[no_mangle]
pub unsafe extern "C" fn riddim_load_model(
    engine: *mut RiddimEngine,
    model_json: *const c_char,
) -> u64 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    let result = unsafe { text(model_json, "the model") }
        .and_then(Model::from_json)
        .and_then(|model| handle.engine.lock().unwrap().load(model));
    match result {
        Ok(revision) => revision,
        Err(message) => {
            set_error(message);
            0
        }
    }
}

/// Asks every loaded plugin for its state and stores it in the model, so that the model JSON
/// is complete enough to save. Blocks until done. Returns 1 on success.
///
/// With an external main thread, call this from a thread other than the main thread while the
/// main thread keeps pumping, or the plugins cannot answer.
#[no_mangle]
pub unsafe extern "C" fn riddim_capture_plugin_states(engine: *mut RiddimEngine) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    // Take the answer channel while holding the lock, wait for it without.
    let result = {
        let engine = handle.engine.lock().unwrap();
        engine.capture_plugin_states()
    };
    match result {
        Ok(_) => 1,
        Err(message) => {
            set_error(message);
            0
        }
    }
}

/// Changes the audio buffer size while the engine runs, without reopening the project.
/// Returns the size the device settled on (it may clamp what was asked for), or 0 on failure.
///
/// Every plugin is told the new size, which takes each out of the mix for a moment; a
/// `plugin_restarted` event says when one is back, and `stream_changed` describes the device.
/// Not allowed while recording.
#[no_mangle]
pub unsafe extern "C" fn riddim_set_buffer_size(engine: *mut RiddimEngine, frames: u32) -> u32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    match handle.engine.lock().unwrap().set_buffer_size(frames) {
        Ok(info) => info.max_frames,
        Err(error) => {
            set_error(error.to_string());
            0
        }
    }
}

/// Tells the engine that a control is being held (`down` = 1) or was released (0). This is
/// what drives touch and latch automation for faders, pans and sends. `target_json` names the
/// control the way automation lanes do, for example `{"type":"track_gain"}` or
/// `{"type":"send_gain","send":3}`. Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn riddim_touch(
    engine: *mut RiddimEngine,
    track: u64,
    target_json: *const c_char,
    down: i32,
) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    let target = unsafe { text(target_json, "the target") }.and_then(|json| {
        serde_json::from_str::<AutomationTarget>(json)
            .map_err(|e| format!("the target is not valid: {e}"))
    });
    match target {
        Ok(target) => {
            handle
                .engine
                .lock()
                .unwrap()
                .touch(TrackId(track), target, down != 0);
            1
        }
        Err(message) => {
            set_error(message);
            0
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BounceSpec {
    /// The WAV file to write.
    path: PathBuf,
    #[serde(default = "start_position")]
    start: Position,
    end: Position,
    #[serde(default)]
    tail_seconds: f64,
    #[serde(default = "default_rate")]
    sample_rate: f64,
    #[serde(default = "default_bounce_block")]
    block_frames: u32,
    /// "float32" (the default), "pcm24" or "pcm16".
    #[serde(default = "default_format")]
    format: String,
    /// Where relative media paths in the model are relative to.
    media_dir: PathBuf,
    /// Track ids to render as a stem. Absent or null: the whole mix.
    #[serde(default)]
    tracks: Option<Vec<TrackId>>,
    /// Threads to render with besides the calling one. The file is the same whatever it is.
    #[serde(default)]
    render_threads: usize,
}

fn start_position() -> Position {
    Position::START
}
fn default_bounce_block() -> u32 {
    512
}
fn default_format() -> String {
    "float32".into()
}

fn run_bounce(model_json: &str, request_json: &str) -> Result<String, String> {
    let model = Model::from_json(model_json)?;
    let spec: BounceSpec = serde_json::from_str(request_json)
        .map_err(|e| format!("the bounce request is not valid: {e}"))?;
    let format = match spec.format.as_str() {
        "float32" => SampleFormat::Float32,
        "pcm24" => SampleFormat::Pcm24,
        "pcm16" => SampleFormat::Pcm16,
        other => return Err(format!("unknown sample format '{other}'")),
    };
    let result = riddim::bounce(
        &model,
        &BounceRequest {
            path: spec.path,
            start: spec.start,
            end: spec.end,
            tail_seconds: spec.tail_seconds,
            sample_rate: spec.sample_rate as u32,
            block_frames: spec.block_frames,
            format,
            media_dir: spec.media_dir,
            tracks: spec.tracks,
            render_threads: spec.render_threads,
        },
    )?;
    Ok(serde_json::json!({
        "path": result.path,
        "frames": result.frames,
        "peak": result.peak,
        "latency_trimmed": result.latency_trimmed,
        "missing_plugins": result.missing_plugins,
    })
    .to_string())
}

/// Renders a model to a WAV file, offline, and returns a JSON description of the result (the
/// caller frees it), or NULL on failure.
///
/// `model_json` is a whole model, as `riddim_model_json` returns it; call
/// `riddim_capture_plugin_states` first so that it carries the plugins' current settings.
/// `request_json` says what to render:
///
/// ```json
/// { "path": "/tmp/mix.wav", "media_dir": "/path/to/project/media",
///   "start": {"beats": 0}, "end": {"beats": 58060800},
///   "tail_seconds": 2.0, "sample_rate": 48000, "block_frames": 512,
///   "format": "float32", "tracks": null }
/// ```
///
/// The bounce runs in an engine of its own with its own plugin instances, independent of any
/// live engine, and this call blocks until it is finished: call it from a worker thread.
#[no_mangle]
pub unsafe extern "C" fn riddim_bounce(
    model_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    clear_error();
    let result = unsafe { text(model_json, "the model") }.and_then(|model| {
        unsafe { text(request_json, "the bounce request") }
            .and_then(|request| run_bounce(model, request))
    });
    match result {
        Ok(json) => owned(json),
        Err(message) => {
            set_error(message);
            ptr::null_mut()
        }
    }
}

/// The next pending event as JSON, or NULL if there is none. The caller frees the string.
/// Call it until it returns NULL, a few dozen times a second.
#[no_mangle]
pub unsafe extern "C" fn riddim_poll_event(engine: *mut RiddimEngine) -> *mut c_char {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return ptr::null_mut();
    };
    match handle.events.lock().unwrap().try_recv() {
        Ok(event) => owned(serde_json::to_string(&event).unwrap_or_else(|_| "{}".into())),
        Err(_) => ptr::null_mut(),
    }
}

/// Fills in `status`. Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn riddim_status(
    engine: *mut RiddimEngine,
    status: *mut RiddimStatus,
) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    if status.is_null() {
        set_error("the status pointer is NULL");
        return 0;
    }
    let s = handle.engine.lock().unwrap().status();
    // SAFETY: checked non-null; the caller provides room for one RiddimStatus.
    unsafe {
        *status = RiddimStatus {
            transport: match s.transport {
                TransportState::Stopped => RiddimTransportState::RiddimStopped,
                TransportState::Playing => RiddimTransportState::RiddimPlaying,
                TransportState::Recording => RiddimTransportState::RiddimRecording,
            },
            position_samples: s.position_samples,
            sample_rate: s.sample_rate,
            block_frames: s.block_frames,
            model_revision: s.model_revision,
            blocks: s.blocks,
            xruns: s.xruns,
            rt_violations: s.rt_violations,
            capture_dropped: s.capture_dropped,
            capture_backlog: s.capture_backlog,
            plan_latency: s.plan_latency,
            input_latency: s.input_latency,
            output_latency: s.output_latency,
            dsp_load: s.dsp_load,
            events_lost: s.events_lost,
            playback_starved: s.playback_starved,
        };
    }
    1
}

/// Waits until the engine has dealt with everything sent to it so far, including plugins
/// finishing loading. Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn riddim_sync(engine: *mut RiddimEngine) -> i32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    handle.engine.lock().unwrap().sync();
    1
}

/// Lets the engine's plugin host do its work. With `"main_thread": "external"`, call this from
/// the application's main thread on every pass of its event loop (or on a timer, sixty times a
/// second or more). Returns 1 while the engine is alive, 0 otherwise.
#[no_mangle]
pub unsafe extern "C" fn riddim_main_thread_pump(engine: *mut RiddimEngine) -> i32 {
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    match handle.main_thread.lock().unwrap().as_mut() {
        Some(main_thread) => main_thread.pump() as i32,
        None => 1,
    }
}

/// Advances an engine on the "manual" device by `frames` frames, writing the interleaved
/// output (`frames` times the output channel count samples) to `output` if it is not NULL.
/// For offline rendering and tests. Returns the number of output channels, or 0 on failure.
#[no_mangle]
pub unsafe extern "C" fn riddim_run(
    engine: *mut RiddimEngine,
    frames: u32,
    output: *mut f32,
) -> u32 {
    clear_error();
    let Some(handle) = (unsafe { handle(engine) }) else {
        return 0;
    };
    let mut engine = handle.engine.lock().unwrap();
    if !engine.is_manual() {
        set_error("riddim_run needs an engine on the manual device");
        return 0;
    }
    let rendered = engine.run(frames as usize);
    let channels = rendered.len();
    if !output.is_null() {
        // SAFETY: the caller provides room for frames * channels samples.
        let out = unsafe { std::slice::from_raw_parts_mut(output, frames as usize * channels) };
        for (index, sample) in out.iter_mut().enumerate() {
            *sample = rendered[index % channels][index / channels];
        }
    }
    channels as u32
}
