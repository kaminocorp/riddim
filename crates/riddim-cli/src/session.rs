//! Commands that run the engine: playing a plugin, rendering, and small recording sessions.
//!
//! A "session" here is the engine model saved as JSON next to a media folder. It is a stand-in
//! for a Whiplash project, just enough to exercise the engine's whole path from a terminal:
//! build, play, record a take, save, reopen, render.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use riddim::files::wav::{self, SampleFormat};
use riddim::files::AudioData;
use riddim::model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Clip, ClipContent, ClipId,
    CurveShape, LaneId, LoopRange, Metronome, Model, Note, Op, PluginSlot, Send, SendId, SlotId,
    Track, TrackChange, TrackId, TrackInput, TrackKind,
};
use riddim::plugins::{plugin_ref, PluginDescriptor};
use riddim::time::{Position, Ticks};
use riddim::{
    DeviceConfig, Engine, EngineConfig, Event, MainThread, MainThreadMode, NullConfig,
    TransportCommand,
};
use riddim_plugin::StateBlob;

use crate::commands::{load_state, CommandResult, Context};

pub const INSTRUMENT_TRACK: TrackId = TrackId(1);
pub const INSTRUMENT_SLOT: SlotId = SlotId(2);
pub const INSTRUMENT_CLIP: ClipId = ClipId(3);
pub const AUDIO_TRACK: TrackId = TrackId(4);
pub const EFFECT_BUS: TrackId = TrackId(1000);
pub const EFFECT_SLOT: SlotId = SlotId(1001);
pub const EFFECT_SEND: SendId = SendId(1002);
pub const FADE_LANE: LaneId = LaneId(1003);
/// Insert effects on the instrument track are numbered from here.
pub const INSERT_SLOT: SlotId = SlotId(1010);

/// Where the audio comes from and goes to.
#[derive(Clone, Copy, Debug)]
pub struct DeviceOptions {
    pub rate: f64,
    pub buffer: u32,
    /// Use the timer-driven null device instead of hardware.
    pub null_device: bool,
    /// Null device only: wire the output back to the input with this many frames of delay.
    pub loopback: Option<u32>,
    /// Open the device's inputs.
    pub capture: bool,
    /// Threads that help the audio thread render. `None` lets the engine pick.
    pub render_threads: Option<usize>,
    /// Change to this buffer size a few seconds into playing, to show that it can be done
    /// on the fly.
    pub switch_buffer: Option<u32>,
}

impl DeviceOptions {
    fn config(&self, manual: bool) -> DeviceConfig {
        let null = NullConfig {
            sample_rate: self.rate,
            buffer_frames: self.buffer,
            loopback_delay: self.loopback,
            reported_input_latency: self.loopback.map_or(0, |d| d / 2),
            reported_output_latency: self.loopback.map_or(0, |d| d - d / 2),
            ..NullConfig::default()
        };
        if manual {
            DeviceConfig::Manual(null)
        } else if self.null_device || !cfg!(target_os = "macos") {
            if !self.null_device {
                eprintln!("note: no audio hardware backend on this platform; using the silent null device");
            }
            DeviceConfig::Null(null)
        } else {
            DeviceConfig::System(riddim::devices::StreamConfig {
                device: None,
                sample_rate: self.rate,
                buffer_frames: self.buffer,
                capture: self.capture,
            })
        }
    }
}

/// The eight-note test arpeggio as a one-bar clip of eighth notes.
pub fn arpeggio_clip(id: ClipId) -> Clip {
    const KEYS: [u8; 8] = [60, 64, 67, 72, 76, 72, 67, 64];
    let notes = KEYS
        .iter()
        .enumerate()
        .map(|(i, &key)| Note {
            start: Ticks::from_beat_fraction(i as i64, 2),
            length: Ticks::from_beat_fraction(2, 5),
            key,
            velocity: 100,
            channel: 0,
        })
        .collect();
    Clip {
        id,
        name: "arpeggio".into(),
        start: Position::START,
        muted: false,
        take: None,
        content: ClipContent::Midi {
            length: Ticks::from_beats(4),
            notes: std::sync::Arc::new(notes),
            events: Default::default(),
        },
    }
}

/// What the instrument track should play.
pub enum Material {
    /// Nothing: live MIDI only.
    Live,
    Arpeggio,
    MidiFile(PathBuf),
}

/// The ops that build a one-instrument session.
pub fn instrument_ops(
    descriptor: &PluginDescriptor,
    state: Option<&Path>,
    material: &Material,
    looped: bool,
) -> Result<Vec<Op>, String> {
    let state = match state {
        Some(path) => Some(std::sync::Arc::new(load_state(path)?.to_bytes())),
        None => None,
    };
    let mut track = Track::new(
        INSTRUMENT_TRACK,
        descriptor.name.clone(),
        TrackKind::Instrument,
    );
    track.input = TrackInput::Midi;
    track.monitor = true;
    let mut ops = vec![
        Op::AddTrack { index: 0, track },
        Op::InsertPlugin {
            track: INSTRUMENT_TRACK,
            index: 0,
            slot: PluginSlot {
                id: INSTRUMENT_SLOT,
                plugin: plugin_ref(descriptor),
                state,
                bypass: false,
                outputs: Vec::new(),
                sidechain: None,
                bridged: false,
            },
        },
    ];
    let clip = match material {
        Material::Live => None,
        Material::Arpeggio => Some(arpeggio_clip(INSTRUMENT_CLIP)),
        Material::MidiFile(path) => {
            let imported = riddim::import::midi_file(path, INSTRUMENT_CLIP)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if let Some(map) = imported.tempo_map {
                ops.push(Op::SetTempoMap { map });
            }
            Some(imported.clip)
        }
    };
    if let Some(clip) = clip {
        if looped {
            if let ClipContent::Midi { length, .. } = &clip.content {
                ops.push(Op::SetLoop {
                    range: Some(LoopRange {
                        start: Position::START,
                        end: Position::Beats(*length),
                    }),
                    enabled: true,
                });
            }
        }
        ops.push(Op::AddClip {
            track: INSTRUMENT_TRACK,
            clip,
        });
    }
    Ok(ops)
}

/// A running engine with its main thread and event stream, plus the editor window if any.
pub struct Running {
    pub engine: Engine,
    pub main: MainThread,
    pub events: Receiver<Event>,
    /// A buffer size to change to while running (see [`DeviceOptions::switch_buffer`]).
    switch_buffer: Option<u32>,
}

impl Running {
    /// Starts an engine on the device `options` describe (or, with `manual`, on the manual
    /// device, for rendering).
    pub fn open(
        options: &DeviceOptions,
        manual: bool,
        dir: &Path,
        midi_input: bool,
    ) -> Result<Running, String> {
        let mut running = Running::start(
            options.config(manual),
            dir,
            midi_input,
            options.render_threads,
        )?;
        running.switch_buffer = options.switch_buffer;
        Ok(running)
    }

    fn start(
        device: DeviceConfig,
        dir: &Path,
        midi_input: bool,
        render_threads: Option<usize>,
    ) -> Result<Running, String> {
        // The manual device is only ever used for rendering.
        let process_mode = match device {
            DeviceConfig::Manual(_) => riddim::plugins::ProcessMode::Offline,
            _ => riddim::plugins::ProcessMode::Realtime,
        };
        let config = EngineConfig {
            device,
            capture_dir: dir.to_path_buf(),
            media_dir: dir.to_path_buf(),
            // The CLI pumps the plugin host itself, on its main thread, so editors can open.
            main_thread: MainThreadMode::External,
            butler: Default::default(),
            midi_input,
            process_mode,
            render_threads,
            parallel_from: riddim::PARALLEL_FROM_STEPS,
            streaming: Default::default(),
            bridge_executable: None,
        };
        let mut engine = Engine::start(config).map_err(|e| e.to_string())?;
        let main = engine
            .take_main_thread()
            .expect("an external main thread was requested");
        let events = engine.subscribe();
        Ok(Running {
            engine,
            main,
            events,
            switch_buffer: None,
        })
    }

    /// One turn of the main loop: plugins, then whatever the engine reported.
    pub fn pump(&mut self, mut on_event: impl FnMut(&Event)) {
        self.main.pump();
        while let Ok(event) = self.events.try_recv() {
            match &event {
                Event::PluginLoaded {
                    slot,
                    latency,
                    parameters,
                    ..
                } => {
                    println!("plugin loaded in slot {}: {parameters} parameters, {latency} samples of latency", slot.0);
                }
                Event::PluginFailed { slot, error, .. } => {
                    eprintln!("error: the plugin in slot {} did not load: {error}", slot.0);
                }
                Event::MediaMissing { media, error } => {
                    eprintln!("warning: media {} is missing: {error}", media.0)
                }
                Event::Error { message } => eprintln!("error: {message}"),
                Event::RecordingOverflow {
                    track,
                    frames_dropped,
                } => {
                    eprintln!(
                        "error: track {} lost {frames_dropped} frames: the disk could not keep up",
                        track.0
                    );
                }
                _ => {}
            }
            on_event(&event);
        }
    }

    /// Pumps until `done` says so or `timeout` passes. Returns whether `done` was reached.
    pub fn pump_until(&mut self, timeout: Duration, mut done: impl FnMut(&Event) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut reached = false;
        while !reached && Instant::now() < deadline {
            self.pump(|event| reached |= done(event));
            if !reached {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        reached
    }

    /// Applies ops and keeps the plugin host turning until they (and any plugin loads they
    /// start) have settled.
    pub fn apply(&mut self, ops: Vec<Op>) -> Result<(), String> {
        let ticket = self.engine.apply(ops);
        let mut outcome = None;
        self.pump_until(Duration::from_secs(120), |event| {
            match event {
                Event::Applied { ticket: t, .. } if *t == ticket => outcome = Some(Ok(())),
                Event::Rejected {
                    ticket: t, error, ..
                } if *t == ticket => outcome = Some(Err(error.clone())),
                _ => {}
            }
            outcome.is_some()
        });
        outcome.unwrap_or_else(|| Err("the engine did not answer".into()))?;
        self.settle()
    }

    /// Waits for every plugin in the model to finish loading (or fail).
    pub fn settle(&mut self) -> Result<(), String> {
        let wanted: usize = self
            .engine
            .model()
            .all_tracks()
            .map(|t| t.plugins.len())
            .sum();
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut settled = self.main.loaded_slots().len();
        let mut failed = 0;
        while settled + failed < wanted {
            if Instant::now() > deadline {
                return Err("plugins are still loading after five minutes".into());
            }
            self.pump(|event| {
                if matches!(event, Event::PluginFailed { .. }) {
                    failed += 1;
                }
            });
            settled = self.main.loaded_slots().len();
            std::thread::sleep(Duration::from_millis(2));
        }
        // Let the plan that includes them reach the audio thread.
        let reply = Instant::now() + Duration::from_millis(50);
        while Instant::now() < reply {
            self.pump(|_| {});
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Stores every plugin's state in the model, pumping the host while it answers.
    pub fn capture_states(&mut self) -> Result<(), String> {
        let outcome = self.engine.request_plugin_states();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            self.pump(|_| {});
            match outcome.try_recv() {
                Ok(result) => return result.map(|_| ()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err("the engine shut down".into())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() > deadline {
                return Err("plugins did not report their state".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// A 64-bit FNV-1a hash over the exact bits of every sample: two renders are identical if and
/// only if their fingerprints are.
pub fn fingerprint(audio: &[Vec<f32>]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for channel in audio {
        for sample in channel {
            for byte in sample.to_bits().to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

/// Bounces `seconds` of a model offline to `out` and returns a description of the result.
#[allow(clippy::too_many_arguments)]
pub fn bounce_model(
    model: &Model,
    media: &Path,
    out: &Path,
    rate: f64,
    buffer: u32,
    seconds: f64,
    tail_seconds: f64,
    tracks: Option<Vec<TrackId>>,
    render_threads: usize,
) -> Result<(riddim::BounceResult, u64), String> {
    let request = riddim::BounceRequest {
        path: out.to_path_buf(),
        start: Position::START,
        end: Position::Time(riddim::time::samples_to_superclock(
            (seconds * rate) as i64,
            rate as u32,
        )),
        tail_seconds,
        sample_rate: rate as u32,
        block_frames: buffer,
        format: SampleFormat::Float32,
        media_dir: media.to_path_buf(),
        tracks,
        render_threads,
    };
    let result = riddim::bounce(model, &request)?;
    for missing in &result.missing_plugins {
        eprintln!("warning: rendered without a plugin: {missing}");
    }
    let audio = wav::read(out).map_err(|e| e.to_string())?;
    Ok((result, fingerprint(&audio.channels)))
}

fn write_wav(path: &Path, rate: f64, audio: Vec<Vec<f32>>) -> Result<(), String> {
    let data = AudioData {
        sample_rate: rate as u32,
        channels: audio,
    };
    wav::write(path, &data, SampleFormat::Float32).map_err(|e| format!("{}: {e}", path.display()))
}

pub struct RenderOptions {
    pub out: PathBuf,
    pub seconds: f64,
    pub rate: f64,
    pub buffer: u32,
    pub state: Option<PathBuf>,
    pub midi: Option<PathBuf>,
    /// Extra threads to render with.
    pub threads: usize,
}

/// `riddim render`: a plugin playing a MIDI file (or the test arpeggio), offline, to a WAV.
pub fn render(context: &Context, spec: &str, options: RenderOptions) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    let material = options
        .midi
        .clone()
        .map_or(Material::Arpeggio, Material::MidiFile);
    let looped = options.midi.is_none();
    let mut model = Model::new();
    model
        .apply_all(&instrument_ops(
            &descriptor,
            options.state.as_deref(),
            &material,
            looped,
        )?)
        .map_err(|(_, e)| e.to_string())?;
    let media = std::env::temp_dir();
    let (result, print) = bounce_model(
        &model,
        &media,
        &options.out,
        options.rate,
        options.buffer,
        options.seconds,
        0.0,
        None,
        options.threads,
    )?;
    println!(
        "rendered {:.2} s of {} to {} (peak {:.4}, fingerprint {:016x})",
        options.seconds,
        descriptor.name,
        options.out.display(),
        result.peak,
        print
    );
    Ok(result.peak > 0.0 && result.missing_plugins.is_empty())
}

pub struct PlayOptions {
    pub device: DeviceOptions,
    pub seconds: Option<f64>,
    pub sequence: bool,
    pub midi: Option<PathBuf>,
    pub editor: bool,
    pub state: Option<PathBuf>,
    pub save_state: Option<PathBuf>,
}

/// Runs until Enter is pressed, the time is up, or (if a window is open) it is closed.
fn interact(
    running: &mut Running,
    seconds: Option<f64>,
    mut window: Option<(crate::ui::EditorWindow, SlotId)>,
    mut on_event: impl FnMut(&Event),
) {
    let (quit_tx, quit_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            let _ = quit_tx.send(());
        }
    });
    match seconds {
        Some(seconds) => println!("running for {seconds} s (Enter to stop early)"),
        None => println!("running; press Enter to stop"),
    }
    let started = Instant::now();
    let mut plugin_events = Vec::new();
    // A change of buffer size, if one was asked for: three seconds in, or halfway if the run
    // is shorter than six.
    let switch_at = seconds.map_or(3.0, |s| (s / 2.0).min(3.0));
    loop {
        crate::ui::pump(Duration::from_millis(5));
        running.pump(&mut on_event);
        if running.switch_buffer.is_some() && started.elapsed().as_secs_f64() >= switch_at {
            let frames = running.switch_buffer.take().expect("just checked");
            match running.engine.set_buffer_size(frames) {
                Ok(info) => println!(
                    "buffer size changed to {} frames while running (asked for {frames}); output latency now {} frames",
                    info.max_frames, info.output_latency_frames
                ),
                Err(error) => eprintln!("error: the buffer size could not be changed: {error}"),
            }
        }
        if let Some((open, slot)) = window.as_mut() {
            let Some(instance) = running.main.instance_mut(*slot) else {
                break;
            };
            // Resize requests reach the window host through the instance's own event poll; the
            // engine has already taken the parameter changes it cares about.
            plugin_events.clear();
            instance.poll_events(&mut plugin_events);
            for event in &plugin_events {
                open.handle_event(instance, event);
            }
            if !open.sync(instance) {
                break;
            }
        }
        if quit_rx.try_recv().is_ok()
            || seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s)
        {
            break;
        }
    }
    if let Some((open, slot)) = window.take() {
        if let Some(instance) = running.main.instance_mut(slot) {
            open.close(instance);
        }
    }
}

fn open_editor(running: &mut Running, slot: SlotId) -> Option<(crate::ui::EditorWindow, SlotId)> {
    let instance = running.main.instance_mut(slot)?;
    if !instance.has_editor() {
        return None;
    }
    let title = instance.descriptor().name.clone();
    match crate::ui::open(instance, &title) {
        Ok(window) => Some((window, slot)),
        Err(why) => {
            eprintln!("note: no editor window: {why}");
            None
        }
    }
}

fn print_summary(running: &Running) -> bool {
    let status = running.engine.status();
    println!(
        "blocks {}  xruns {}  audio-thread heap operations {}  frames lost while recording {}",
        status.blocks, status.xruns, status.rt_violations, status.capture_dropped
    );
    println!(
        "peak dsp load {:.0}%  frames of streamed files not read in time {}  messages lost {}",
        status.dsp_load * 100.0,
        status.playback_starved,
        status.events_lost
    );
    status.rt_violations == 0 && status.capture_dropped == 0
}

/// `riddim play`: one plugin on an instrument track, live through the device.
pub fn play(context: &Context, spec: &str, options: PlayOptions) -> CommandResult {
    let descriptor = context.resolve(spec)?;
    crate::ui::init()?;
    let dir = std::env::temp_dir().join(format!("riddim-play-{}", std::process::id()));
    let mut running = Running::open(&options.device, false, &dir, !options.device.null_device)?;
    let info = running.engine.stream_info().clone();
    println!(
        "device: {} at {} Hz, {} frames, {} out; output latency {} frames",
        info.device_name,
        info.sample_rate,
        info.max_frames,
        info.output_channels,
        info.output_latency_frames
    );
    let material = match (&options.midi, options.sequence) {
        (Some(path), _) => Material::MidiFile(path.clone()),
        (None, true) => Material::Arpeggio,
        (None, false) => Material::Live,
    };
    let rolling = !matches!(material, Material::Live);
    running.apply(instrument_ops(
        &descriptor,
        options.state.as_deref(),
        &material,
        options.midi.is_none(),
    )?)?;
    if running.main.loaded_slots().is_empty() {
        return Err(format!("{} did not load", descriptor.name));
    }
    if options.state.is_some() {
        println!("restored state");
    }
    if rolling {
        running.engine.transport(TransportCommand::Play);
    }
    let window = if options.editor {
        open_editor(&mut running, INSTRUMENT_SLOT)
    } else {
        None
    };
    interact(&mut running, options.seconds, window, |_| {});
    running.engine.transport(TransportCommand::Stop);

    if let Some(path) = &options.save_state {
        running.capture_states()?;
        let model = running.engine.model();
        let bytes = model
            .track(INSTRUMENT_TRACK)
            .and_then(|t| t.plugin(INSTRUMENT_SLOT))
            .and_then(|s| s.state.clone())
            .ok_or("the plugin did not report a state")?;
        // The slot holds the same container the hosting tools read and write.
        StateBlob::from_bytes(&bytes).map_err(|e| e.to_string())?;
        std::fs::write(path, bytes.as_slice()).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("saved {} bytes of state to {}", bytes.len(), path.display());
    }
    let clean = print_summary(&running);
    drop(running);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(clean)
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

fn media_dir(project: &Path) -> PathBuf {
    let mut name = project.file_stem().unwrap_or_default().to_os_string();
    name.push(".media");
    project.with_file_name(name)
}

fn load_project(project: &Path) -> Result<Model, String> {
    let json =
        std::fs::read_to_string(project).map_err(|e| format!("{}: {e}", project.display()))?;
    Model::from_json(&json).map_err(|e| format!("{}: {e}", project.display()))
}

fn save_project(project: &Path, model: &Model) -> Result<(), String> {
    // Write beside the target and rename, so a crash never leaves half a project file.
    let temporary = project.with_extension("json.tmp");
    std::fs::write(&temporary, model.to_json())
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, project).map_err(|e| format!("{}: {e}", project.display()))
}

pub struct NewOptions {
    pub instrument: Option<String>,
    pub midi: Option<PathBuf>,
    pub sequence: bool,
    /// First device input channel (0-based) and channel count for the audio track.
    pub audio_input: Option<(u16, u8)>,
    /// How many armed audio tracks to create on that input.
    pub audio_tracks: u8,
    pub count_in: u8,
    /// Effects to insert on the instrument track, in order.
    pub inserts: Vec<String>,
    /// An effect to put on a bus of its own, fed by a send from the instrument track.
    pub send_effect: Option<String>,
    /// Automate the instrument's fader from silence to full over this many beats.
    pub fade_in: Option<u32>,
    /// Run every plugin of the project in a process of its own.
    pub bridged: bool,
}

/// `riddim session new`: writes a project with an instrument track and an armed audio track.
pub fn session_new(context: &Context, project: &Path, options: NewOptions) -> CommandResult {
    let mut model = Model::new();
    let apply = |model: &mut Model, ops: Vec<Op>| {
        model
            .apply_all(&ops)
            .map(|_| ())
            .map_err(|(_, e)| e.to_string())
    };
    if let Some(spec) = &options.instrument {
        let descriptor = context.resolve(spec)?;
        let material = match (&options.midi, options.sequence) {
            (Some(path), _) => Material::MidiFile(path.clone()),
            (None, true) => Material::Arpeggio,
            (None, false) => Material::Live,
        };
        let mut ops = instrument_ops(&descriptor, None, &material, false)?;
        for op in &mut ops {
            if let Op::InsertPlugin { slot, .. } = op {
                slot.bridged = options.bridged;
            }
        }
        apply(&mut model, ops)?;
    }
    let effect_slot = |id: SlotId, spec: &str| -> Result<PluginSlot, String> {
        Ok(PluginSlot {
            id,
            plugin: plugin_ref(&context.resolve(spec)?),
            state: None,
            bypass: false,
            outputs: Vec::new(),
            sidechain: None,
            bridged: options.bridged,
        })
    };
    let wants_mixing =
        !options.inserts.is_empty() || options.send_effect.is_some() || options.fade_in.is_some();
    if wants_mixing && options.instrument.is_none() {
        return Err("--insert, --send-effect and --fade-in need an --instrument track".into());
    }
    for (n, spec) in options.inserts.iter().enumerate() {
        apply(
            &mut model,
            vec![Op::InsertPlugin {
                track: INSTRUMENT_TRACK,
                index: n + 1,
                slot: effect_slot(SlotId(INSERT_SLOT.0 + n as u64), spec)?,
            }],
        )?;
    }
    if let Some(spec) = &options.send_effect {
        let index = model.tracks.len();
        apply(
            &mut model,
            vec![
                Op::AddTrack {
                    index,
                    track: Track::new(EFFECT_BUS, "Effect return", TrackKind::Bus),
                },
                Op::InsertPlugin {
                    track: EFFECT_BUS,
                    index: 0,
                    slot: effect_slot(EFFECT_SLOT, spec)?,
                },
                Op::AddSend {
                    track: INSTRUMENT_TRACK,
                    index: 0,
                    send: Send {
                        id: EFFECT_SEND,
                        target: EFFECT_BUS,
                        gain: 0.5,
                        pre_fader: false,
                        mute: false,
                    },
                },
            ],
        )?;
    }
    if let Some(beats) = options.fade_in.filter(|beats| *beats > 0) {
        let point = |beat: i64, value: f32| AutomationPoint {
            at: Ticks::from_beats(beat),
            value,
            shape: CurveShape::Linear,
        };
        apply(
            &mut model,
            vec![Op::AddLane {
                track: INSTRUMENT_TRACK,
                lane: AutomationLane {
                    id: FADE_LANE,
                    target: AutomationTarget::TrackGain,
                    mode: AutomationMode::Read,
                    points: std::sync::Arc::new(vec![point(0, 0.0), point(beats as i64, 1.0)]),
                },
            }],
        )?;
    }
    if let Some((first_channel, channels)) = options.audio_input {
        for n in 0..options.audio_tracks.max(1) as u64 {
            let id = TrackId(AUDIO_TRACK.0 + n * 10);
            let index = model.tracks.len();
            apply(
                &mut model,
                vec![
                    Op::AddTrack {
                        index,
                        track: Track::new(id, format!("Audio {}", n + 1), TrackKind::Audio),
                    },
                    Op::SetTrack {
                        id,
                        change: TrackChange::Input(TrackInput::Audio {
                            first_channel,
                            channels,
                        }),
                    },
                    Op::SetTrack {
                        id,
                        change: TrackChange::RecordArmed(true),
                    },
                ],
            )?;
        }
    }
    if options.count_in > 0 {
        apply(
            &mut model,
            vec![Op::SetMetronome {
                metronome: Metronome {
                    enabled: false,
                    gain: 0.5,
                    count_in_bars: options.count_in,
                },
            }],
        )?;
    }
    std::fs::create_dir_all(media_dir(project)).map_err(|e| e.to_string())?;
    save_project(project, &model)?;
    println!(
        "wrote {} ({} track(s))",
        project.display(),
        model.tracks.len()
    );
    Ok(true)
}

/// `riddim session apply`: edits a project file with ops given as JSON (an array of ops, as
/// they cross the C ABI). Anything the engine's model can express can be set up this way.
pub fn session_apply(project: &Path, ops: &Path) -> CommandResult {
    let mut model = load_project(project)?;
    let text = if ops.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())?
    } else {
        std::fs::read_to_string(ops).map_err(|e| format!("{}: {e}", ops.display()))?
    };
    let ops: Vec<Op> =
        serde_json::from_str(&text).map_err(|e| format!("the ops are not valid: {e}"))?;
    model
        .apply_all(&ops)
        .map_err(|(index, error)| format!("op {index} was refused: {error}"))?;
    save_project(project, &model)?;
    println!("applied {} op(s) to {}", ops.len(), project.display());
    Ok(true)
}

/// `riddim session info`.
pub fn session_info(project: &Path) -> CommandResult {
    let model = load_project(project)?;
    println!("{}", project.display());
    println!(
        "  tempo {:.2} bpm at the start",
        model.tempo_map.tempo_at(Ticks::ZERO).bpm()
    );
    let in_beats = |range: riddim::model::LoopRange| {
        let beats = |position| model.tempo_map.position_to_ticks(position).as_beats_f64();
        format!("beats {:.2} to {:.2}", beats(range.start), beats(range.end))
    };
    if let Some(range) = model.loop_range {
        let state = if model.loop_enabled { "on" } else { "off" };
        println!("  loop {} ({state})", in_beats(range));
    }
    if let Some(range) = model.punch_range {
        let state = if model.punch_enabled { "on" } else { "off" };
        println!("  punch {} ({state})", in_beats(range));
    }
    for track in model.all_tracks() {
        println!(
            "  track {:>3}  {:<10} {:<24} gain {:.2}{}{}",
            track.id.0,
            format!("{:?}", track.kind).to_lowercase(),
            track.name,
            track.gain,
            if track.record_armed { "  armed" } else { "" },
            if track.mute { "  muted" } else { "" }
        );
        if let Some(from) = track.events_from {
            println!(
                "      also plays the notes that come out of track {}",
                from.0
            );
        }
        for slot in &track.plugins {
            println!(
                "      plugin {} in slot {} ({}:{}{}){}",
                slot.plugin.name,
                slot.id.0,
                slot.plugin.format,
                slot.plugin.uid,
                if slot.bridged {
                    ", in a process of its own"
                } else {
                    ""
                },
                slot.state
                    .as_ref()
                    .map_or(String::new(), |s| format!(", {} bytes of state", s.len()))
            );
            for output in &slot.outputs {
                println!(
                    "          output {} to track {}",
                    output.bus + 1,
                    output.target.0
                );
            }
            if let Some(sidechain) = slot.sidechain {
                println!(
                    "          sidechain from track {} ({}-fader)",
                    sidechain.source.0,
                    if sidechain.pre_fader { "pre" } else { "post" }
                );
            }
        }
        for send in &track.sends {
            println!(
                "      send {} to track {} at gain {:.2} ({}-fader){}",
                send.id.0,
                send.target.0,
                send.gain,
                if send.pre_fader { "pre" } else { "post" },
                if send.mute { ", muted" } else { "" }
            );
        }
        for lane in &track.automation {
            println!(
                "      automation {} of {:?}: {} point(s), {:?}",
                lane.id.0,
                lane.target,
                lane.points.len(),
                lane.mode
            );
        }
        for clip in &track.clips {
            let mut notes_on_it = String::new();
            if let Some(take) = clip.take {
                notes_on_it.push_str(&format!(", take {}", take.number));
            }
            if clip.muted {
                notes_on_it.push_str(", muted");
            }
            match &clip.content {
                ClipContent::Audio { media, length, .. } => {
                    let path = model.media(*media).map_or("?", |m| m.path.as_str());
                    println!(
                        "      audio clip {} from {path} ({length} frames{notes_on_it})",
                        clip.id.0
                    );
                }
                ClipContent::Midi { notes, .. } => {
                    println!(
                        "      midi clip {} ({} notes{notes_on_it})",
                        clip.id.0,
                        notes.len()
                    )
                }
            }
        }
    }
    Ok(true)
}

/// `riddim session play`.
pub fn session_play(
    project: &Path,
    device: DeviceOptions,
    seconds: Option<f64>,
    editor: bool,
    save: bool,
) -> CommandResult {
    let model = load_project(project)?;
    crate::ui::init()?;
    let mut running = Running::open(&device, false, &media_dir(project), !device.null_device)?;
    running.engine.load(model)?;
    running.settle()?;
    running
        .engine
        .transport(TransportCommand::Locate(Position::START));
    running.engine.transport(TransportCommand::Play);
    let window = if editor {
        open_editor(&mut running, INSTRUMENT_SLOT)
    } else {
        None
    };
    interact(&mut running, seconds, window, |_| {});
    running.engine.transport(TransportCommand::Stop);
    if save {
        // What was changed in the plugins' editors lives only in the plugins until now.
        running.capture_states()?;
        save_project(project, &running.engine.model())?;
        println!(
            "saved {} with the plugins' current settings",
            project.display()
        );
    }
    Ok(print_summary(&running))
}

/// `riddim session record`: plays the project, records every armed track, saves the result.
/// A stretch of the timeline given on the command line as `START:END` in beats.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatRange {
    pub start: f64,
    pub end: f64,
}

impl std::str::FromStr for BeatRange {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("'{text}' is not a range of beats like \"4:8\"");
        let (start, end) = text.split_once(':').ok_or_else(bad)?;
        let range = BeatRange {
            start: start.trim().parse().map_err(|_| bad())?,
            end: end.trim().parse().map_err(|_| bad())?,
        };
        if range.start < 0.0 || range.end <= range.start {
            return Err(bad());
        }
        Ok(range)
    }
}

impl BeatRange {
    fn range(self) -> riddim::model::LoopRange {
        let ticks =
            |beats: f64| Ticks((beats * riddim::time::TICKS_PER_BEAT as f64).round() as i64);
        riddim::model::LoopRange {
            start: Position::Beats(ticks(self.start)),
            end: Position::Beats(ticks(self.end)),
        }
    }
}

pub struct RecordOptions {
    pub seconds: Option<f64>,
    pub editor: bool,
    /// Loop this stretch while recording: every time round becomes a take.
    pub loop_range: Option<BeatRange>,
    /// Record only inside this stretch.
    pub punch: Option<BeatRange>,
}

pub fn session_record(
    project: &Path,
    device: DeviceOptions,
    options: RecordOptions,
) -> CommandResult {
    let RecordOptions {
        seconds,
        editor,
        loop_range,
        punch,
    } = options;
    let mut model = load_project(project)?;
    let mut ranges = Vec::new();
    if let Some(range) = loop_range {
        ranges.push(Op::SetLoop {
            range: Some(range.range()),
            enabled: true,
        });
    }
    if let Some(range) = punch {
        ranges.push(Op::SetPunch {
            range: Some(range.range()),
            enabled: true,
        });
    }
    model
        .apply_all(&ranges)
        .map_err(|(_, error)| error.to_string())?;
    crate::ui::init()?;
    let mut device = device;
    device.capture = true;
    let mut running = Running::open(&device, false, &media_dir(project), !device.null_device)?;
    let info = running.engine.stream_info().clone();
    println!(
        "device: {} at {} Hz, {} frames, {} in / {} out; latency {} in + {} out frames",
        info.device_name,
        info.sample_rate,
        info.max_frames,
        info.input_channels,
        info.output_channels,
        info.input_latency_frames,
        info.output_latency_frames
    );
    running.engine.load(model)?;
    running.settle()?;
    running
        .engine
        .transport(TransportCommand::Locate(Position::START));
    running.engine.transport(TransportCommand::Record);
    let window = if editor {
        open_editor(&mut running, INSTRUMENT_SLOT)
    } else {
        None
    };

    // What the engine reports about the recording, whenever it reports it: a punch-in ends
    // by itself while the transport is still playing, a loop gives several takes at once.
    let started = std::cell::Cell::new(false);
    let takes = std::cell::Cell::new(0usize);
    let tracks_done = std::cell::RefCell::new(std::collections::BTreeSet::new());
    let note = |event: &Event| match event {
        Event::RecordingStarted {
            tracks,
            position_samples,
        } => {
            started.set(true);
            println!(
                "recording {} track(s) from sample {position_samples}",
                tracks.len()
            );
        }
        Event::RecordingFinished {
            track,
            frames,
            media,
            take,
            ..
        } => {
            takes.set(takes.get() + 1);
            tracks_done.borrow_mut().insert(*track);
            let unit = if media.is_some() { "frames" } else { "notes" };
            match take {
                Some(number) => println!("take {number} on track {}: {frames} {unit}", track.0),
                None => println!("take on track {}: {frames} {unit}", track.0),
            }
        }
        _ => {}
    };
    interact(&mut running, seconds, window, note);
    if !started.get() {
        running.engine.transport(TransportCommand::Stop);
        return Err("recording did not start (is a track armed with an input?)".into());
    }

    // Stop, then wait for every armed track's takes to be written and placed (unless a
    // punch-out has already seen to that).
    let armed = running
        .engine
        .model()
        .tracks
        .iter()
        .filter(|t| t.record_armed)
        .count();
    running.engine.transport(TransportCommand::Stop);
    running.pump_until(Duration::from_secs(60), |event| {
        note(event);
        tracks_done.borrow().len() >= armed
    });
    // The takes of one recording arrive together; make sure none is still in the post.
    running.pump_until(Duration::from_millis(250), |event| {
        note(event);
        false
    });
    let takes = takes.get();
    running.capture_states()?;
    save_project(project, &running.engine.model())?;
    println!("saved {} with {takes} new take(s)", project.display());
    Ok(print_summary(&running) && takes > 0)
}

pub struct SessionRenderOptions {
    pub out: PathBuf,
    pub seconds: f64,
    pub rate: f64,
    pub buffer: u32,
    /// Extra time after the end, for reverb and release tails.
    pub tail: f64,
    /// Render only these tracks (a stem). Empty: the whole mix.
    pub stem: Vec<u64>,
    /// Extra threads to render with.
    pub threads: usize,
}

/// `riddim session render`: the project, offline, to a WAV, with a fingerprint of the audio.
pub fn session_render(project: &Path, options: SessionRenderOptions) -> CommandResult {
    let model = load_project(project)?;
    for id in &options.stem {
        if model.track(TrackId(*id)).is_none() {
            return Err(format!("the project has no track {id}"));
        }
    }
    let tracks = (!options.stem.is_empty()).then(|| {
        options
            .stem
            .iter()
            .map(|id| TrackId(*id))
            .collect::<Vec<_>>()
    });
    let what = match &tracks {
        Some(tracks) => format!("a stem of {} track(s)", tracks.len()),
        None => "the mix".to_string(),
    };
    let (result, print) = bounce_model(
        &model,
        &media_dir(project),
        &options.out,
        options.rate,
        options.buffer,
        options.seconds,
        options.tail,
        tracks,
        options.threads,
    )?;
    println!(
        "rendered {:.2} s of {what} to {} (peak {:.4}, fingerprint {print:016x})",
        result.frames as f64 / options.rate,
        options.out.display(),
        result.peak
    );
    Ok(result.missing_plugins.is_empty())
}

/// `riddim loopback`: measures where recorded audio lands relative to where it was played.
///
/// Plays a train of single-sample clicks from one track while recording the device's input on
/// another, then finds each click in the take. With a cable from an output to an input, and a
/// device that reports its latency truthfully, every click lands on the sample it was played at:
/// the offset printed is the engine's placement error on this hardware.
pub fn loopback(device: DeviceOptions, input: (u16, u8), seconds: f64) -> CommandResult {
    const CLICK_TRACK: TrackId = TrackId(1);
    const RECORD_TRACK: TrackId = TrackId(2);
    crate::ui::init()?;
    let dir = std::env::temp_dir().join(format!("riddim-loopback-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut device = device;
    device.capture = true;
    let mut running = Running::open(&device, false, &dir, false)?;
    let info = running.engine.stream_info().clone();
    let rate = info.sample_rate as u32;
    println!(
        "device: {} at {} Hz, {} frames; reported latency {} in + {} out = {} frames",
        info.device_name,
        info.sample_rate,
        info.max_frames,
        info.input_latency_frames,
        info.output_latency_frames,
        info.input_latency_frames + info.output_latency_frames
    );

    // One click every quarter second, starting half a second in.
    let spacing = rate as usize / 4;
    let total = (seconds * rate as f64) as usize;
    let mut signal = vec![0.0f32; total];
    let clicks: Vec<usize> = (0..)
        .map(|n| rate as usize / 2 + n * spacing)
        .take_while(|&at| at + spacing < total)
        .collect();
    for &at in &clicks {
        signal[at] = 0.5;
    }
    let click_path = dir.join("clicks.wav");
    write_wav(&click_path, rate as f64, vec![signal])?;
    let media = riddim::import::audio_file(&click_path, riddim::model::MediaId(10))
        .map_err(|e| e.to_string())?;
    let clip = riddim::import::clip_for_media(&media, ClipId(11), Position::START);
    running.apply(vec![
        Op::AddMedia { media },
        Op::AddTrack {
            index: 0,
            track: Track::new(CLICK_TRACK, "clicks", TrackKind::Audio),
        },
        Op::AddClip {
            track: CLICK_TRACK,
            clip,
        },
        Op::AddTrack {
            index: 1,
            track: Track::new(RECORD_TRACK, "return", TrackKind::Audio),
        },
        Op::SetTrack {
            id: RECORD_TRACK,
            change: TrackChange::Input(TrackInput::Audio {
                first_channel: input.0,
                channels: input.1,
            }),
        },
        Op::SetTrack {
            id: RECORD_TRACK,
            change: TrackChange::RecordArmed(true),
        },
    ])?;

    running.engine.transport(TransportCommand::Record);
    let mut finished = None;
    running.pump_until(Duration::from_secs_f64(seconds), |_| false);
    running.engine.transport(TransportCommand::Stop);
    running.pump_until(Duration::from_secs(30), |event| {
        if let Event::RecordingFinished {
            track,
            media: Some(media),
            ..
        } = event
        {
            if *track == RECORD_TRACK {
                finished = Some(*media);
            }
        }
        finished.is_some()
    });
    let Some(media) = finished else {
        return Err("no take was recorded".into());
    };
    let model = running.engine.model();
    let take_path = PathBuf::from(
        &model
            .media(media)
            .ok_or("the take is missing from the model")?
            .path,
    );
    let take = wav::read(&take_path).map_err(|e| e.to_string())?;
    let status = running.engine.status();
    drop(running);

    // For each click, the loudest sample within a window around where it should be.
    let channel = &take.channels[0];
    let window = spacing as i64 / 2;
    let mut offsets = Vec::new();
    for &at in &clicks {
        let from = (at as i64 - window).max(0) as usize;
        let to = ((at as i64 + window) as usize).min(channel.len());
        if from >= to {
            continue;
        }
        let (index, level) =
            channel[from..to]
                .iter()
                .enumerate()
                .fold((0, 0.0f32), |best, (i, s)| {
                    if s.abs() > best.1 {
                        (i, s.abs())
                    } else {
                        best
                    }
                });
        if level > 0.01 {
            offsets.push((from + index) as i64 - at as i64);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if offsets.is_empty() {
        println!(
            "no clicks came back: is an output connected to input {}?",
            input.0 + 1
        );
        return Ok(false);
    }
    let (min, max) = (
        *offsets.iter().min().unwrap(),
        *offsets.iter().max().unwrap(),
    );
    println!(
        "{} of {} clicks found; offset from where they were played: {min} to {max} samples",
        offsets.len(),
        clicks.len()
    );
    println!(
        "(positive means the take is late: the device's real round trip is longer than it reports)"
    );
    println!(
        "frames lost while recording {}  audio-thread heap operations {}",
        status.capture_dropped, status.rt_violations
    );
    Ok(min.abs() <= 1 && max.abs() <= 1)
}
