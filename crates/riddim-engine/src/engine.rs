//! The engine handle: what a client holds.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use riddim_disk::capture::{Butler, ButlerConfig};
use riddim_io::midi::{MidiInput, MidiMessage};
use riddim_io::null::{ManualStream, NullConfig, NullStream};
use riddim_io::{AudioStream, StreamInfo};
use riddim_model::{Model, Op, SlotId, TrackId};
use riddim_plugin::EventBody;
use riddim_rt::garbage::garbage_channel;
use riddim_rt::spsc::{self, Producer};

use crate::audio::AudioEngine;
use crate::control::{Applied, Control, ControlMsg, ControlSetup, Rejected, TransportCommand};
use crate::events::{Event, TransportState};
use crate::host::{HostReply, HostRequest, MainThread};
use crate::shared::Shared;

/// Which audio device the engine runs on.
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceConfig {
    /// No hardware and no thread: audio advances only when the client calls
    /// [`Engine::run`]. Deterministic; for tests and offline work.
    Manual(NullConfig),
    /// No hardware; a timer thread calls the engine at roughly real-time pace.
    Null(NullConfig),
    /// The system's audio hardware (CoreAudio on macOS).
    System(riddim_io::StreamConfig),
}

/// Who pumps the plugin host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainThreadMode {
    /// The engine runs a thread for it. Fine for headless use; plugin editors cannot be shown.
    Internal,
    /// The client takes the [`MainThread`] with [`Engine::take_main_thread`] and pumps it from
    /// its own main thread. Required to show plugin editors.
    External,
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub device: DeviceConfig,
    /// Where recordings are written.
    pub capture_dir: PathBuf,
    /// What relative media paths in the model are relative to.
    pub media_dir: PathBuf,
    pub main_thread: MainThreadMode,
    pub butler: ButlerConfig,
    /// Connect to the system's MIDI inputs.
    pub midi_input: bool,
    /// What plugins are told about how they are being driven. `Offline` lets samplers wait for
    /// their own disk streaming instead of dropping voices; use it for bounces.
    pub process_mode: riddim_plugin::ProcessMode,
    /// Threads that help the audio thread render. `None` picks for the device: one fewer than
    /// the machine has cores (at most 7) on audio hardware, where they join the device's audio
    /// workgroup; none on the deviceless backends. The result is the same, bit for bit,
    /// whatever the number.
    pub render_threads: Option<usize>,
    /// Plans with fewer steps than this are rendered by the audio thread alone even when
    /// there are render threads: sharing out a handful of steps costs more than it saves.
    pub parallel_from: usize,
    /// How long files are played from disk.
    pub streaming: crate::stream::StreamingConfig,
    /// The `riddim-bridge` executable, for plugin slots marked to run in a process of their
    /// own. `None` looks in `$RIDDIM_BRIDGE` and then beside the running program.
    pub bridge_executable: Option<PathBuf>,
}

/// The default for [`EngineConfig::parallel_from`].
pub const PARALLEL_FROM_STEPS: usize = 24;

/// The most render threads the engine will start.
pub const MAX_RENDER_THREADS: usize = 31;

impl EngineConfig {
    /// A deterministic, deviceless configuration at 48 kHz with 128-frame blocks, writing
    /// recordings under `dir`.
    pub fn manual(dir: impl Into<PathBuf>) -> EngineConfig {
        let dir = dir.into();
        EngineConfig {
            device: DeviceConfig::Manual(NullConfig::default()),
            capture_dir: dir.clone(),
            media_dir: dir,
            main_thread: MainThreadMode::Internal,
            butler: ButlerConfig::default(),
            midi_input: false,
            process_mode: riddim_plugin::ProcessMode::Realtime,
            render_threads: None,
            parallel_from: PARALLEL_FROM_STEPS,
            streaming: Default::default(),
            bridge_executable: None,
        }
    }
}

/// The smallest and largest buffer sizes the engine will run at.
pub const MIN_BUFFER_FRAMES: u32 = 16;
pub const MAX_BUFFER_FRAMES: u32 = 4096;

#[derive(Debug)]
#[non_exhaustive]
pub enum EngineError {
    Device(String),
    Io(std::io::Error),
    /// The request cannot be carried out in the engine's present state.
    Refused(&'static str),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Device(why) => write!(f, "audio device: {why}"),
            EngineError::Io(e) => write!(f, "{e}"),
            EngineError::Refused(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// The channel ends and the bridge path a [`MainThread`] is made from.
type MainThreadParts = (Receiver<HostRequest>, Sender<HostReply>, Option<PathBuf>);

/// A snapshot of what the engine is doing.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub struct Status {
    pub transport: TransportState,
    pub position_samples: i64,
    pub sample_rate: u32,
    pub block_frames: u32,
    pub model_revision: u64,
    /// Blocks processed since the engine started.
    pub blocks: u64,
    pub xruns: u64,
    /// Heap operations observed on the audio thread. Only measured when the process installed
    /// `riddim_rt::guard::RtCheckAllocator`.
    pub rt_violations: u64,
    /// Recorded frames lost to a full capture ring.
    pub capture_dropped: u64,
    /// Recorded frames waiting in memory for the disk (the track furthest behind). Climbing
    /// towards the ring size means the disk is not keeping up.
    pub capture_backlog: u64,
    /// Samples by which the output lags the timeline (plugin latency compensation).
    pub plan_latency: u32,
    pub input_latency: u32,
    pub output_latency: u32,
    /// The largest share of a block's duration that rendering one took over the last second
    /// or so (1.0 is all of it). The same figure as [`Event::Load`].
    pub dsp_load: f32,
    /// Messages from the audio thread that were lost because nobody was taking them fast
    /// enough. Should stay at zero.
    pub events_lost: u64,
    /// Frames of files played from disk that were not read in time and played as silence.
    pub playback_starved: u64,
    /// Pages of files played from disk that are in memory right now.
    pub stream_pages: u64,
}

enum Stream {
    Manual(Box<ManualStream>),
    Running(Box<dyn AudioStream + Send>),
}

/// A running engine.
///
/// Commands are sent to the engine's control thread and return at once; results arrive as
/// [`Event`]s, or through the `_wait` variants for callers that prefer to block.
pub struct Engine {
    control: Sender<ControlMsg>,
    control_thread: Option<JoinHandle<()>>,
    host_thread: Option<JoinHandle<()>>,
    /// What the main-thread half is made from, until it is taken.
    main_thread: Option<MainThreadParts>,
    stream: Option<Stream>,
    info: StreamInfo,
    shared: Arc<Shared>,
    snapshot: Arc<Mutex<Arc<Model>>>,
    next_ticket: AtomicU64,
    /// A stand-in MIDI keyboard for when no system MIDI is connected (tests, scripting).
    midi_feed: Option<Producer<MidiMessage>>,
    _midi: Option<MidiInput>,
}

impl Engine {
    /// Starts the engine: opens the device, and starts the control and disk threads.
    pub fn start(config: EngineConfig) -> Result<Engine, EngineError> {
        std::fs::create_dir_all(&config.capture_dir).map_err(EngineError::Io)?;
        let shared = Arc::new(Shared::default());
        let (to_audio, from_control) = spsc::channel(4096);
        let (to_control, from_audio) = spsc::channel(16_384);
        let (chute, collector) = garbage_channel(4096);
        let (midi_tx, midi_rx) = spsc::channel::<MidiMessage>(4096);

        let (rate, block) = match &config.device {
            DeviceConfig::Manual(c) | DeviceConfig::Null(c) => (c.sample_rate, c.buffer_frames),
            DeviceConfig::System(c) => (c.sample_rate, c.buffer_frames),
        };
        // The plan never runs a block larger than this; bigger device blocks are split.
        let max_block = block.max(MIN_BUFFER_FRAMES) as usize;

        let render_threads = config.render_threads.unwrap_or(match &config.device {
            DeviceConfig::System(_) => std::thread::available_parallelism()
                .map_or(0, |cores| cores.get().saturating_sub(1))
                .min(7),
            DeviceConfig::Manual(_) | DeviceConfig::Null(_) => 0,
        });
        let workgroup = crate::workgroup::Slot::default();
        let pool = if render_threads > 0 {
            let workgroup = workgroup.clone();
            Some(
                crate::exec::Pool::new(render_threads.min(MAX_RENDER_THREADS), move || {
                    workgroup.join_when_known()
                })
                .map_err(EngineError::Io)?,
            )
        } else {
            None
        };
        let executor = crate::exec::Executor::new(pool, config.parallel_from);
        let audio = AudioEngine::new(
            from_control,
            to_control,
            chute,
            midi_rx,
            shared.clone(),
            rate as u32,
            max_block,
            executor,
            matches!(config.device, DeviceConfig::Manual(_)),
        );

        let opened = match config.device.clone() {
            DeviceConfig::Manual(c) => {
                let stream = ManualStream::new(c, audio);
                let info = stream.info().clone();
                Ok((Stream::Manual(Box::new(stream)), info))
            }
            DeviceConfig::Null(c) => {
                let mut stream = NullStream::new(c, audio);
                let info = stream.info().clone();
                stream
                    .start()
                    .map(|()| (Stream::Running(Box::new(stream) as Box<_>), info))
                    .map_err(|e| EngineError::Device(e.to_string()))
            }
            DeviceConfig::System(c) => open_system_stream(&c, audio, &workgroup),
        };
        // Whatever happened, the render threads must not be left waiting to hear about a
        // workgroup.
        workgroup.close();
        let (stream, info) = opened?;

        let (midi_feed, midi) = if config.midi_input {
            let input = MidiInput::open_all("Riddim", midi_tx).map_err(EngineError::Device)?;
            (None, Some(input))
        } else {
            (Some(midi_tx), None)
        };

        let (control_tx, control_rx) = mpsc::channel();
        let (host_tx, host_rx) = mpsc::channel();
        let (host_reply_tx, host_reply_rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Arc::new(Model::new())));
        let setup = ControlSetup {
            inbox: control_rx,
            to_audio,
            from_audio,
            collector,
            host_requests: host_tx,
            host_replies: host_reply_rx,
            shared: shared.clone(),
            snapshot: snapshot.clone(),
            stream: info.clone(),
            max_block,
            capture_dir: config.capture_dir.clone(),
            media_dir: config.media_dir.clone(),
            butler: Butler::spawn(config.butler),
            process_mode: config.process_mode,
            xruns: match &stream {
                Stream::Running(stream) => Some(stream.xrun_counter()),
                Stream::Manual(_) => None,
            },
            streaming: config.streaming,
        };
        let control_thread = std::thread::Builder::new()
            .name("riddim-control".into())
            .spawn(move || Control::new(setup).run())
            .map_err(EngineError::Io)?;

        let (main_thread, host_thread) = match config.main_thread {
            // Only what the main-thread half is made from is kept here, and the half itself is
            // made where it is taken: it holds plugin instances, which must never change
            // thread, and an `Engine` holding one could not be handed to another thread or
            // kept in an application's shared state.
            MainThreadMode::External => (
                Some((host_rx, host_reply_tx, config.bridge_executable.clone())),
                None,
            ),
            MainThreadMode::Internal => {
                let bridge = config.bridge_executable.clone();
                let thread = std::thread::Builder::new()
                    .name("riddim-plugin-host".into())
                    .spawn(move || {
                        // Plugin instances are created on, and never leave, this thread.
                        let mut main = MainThread::new(host_rx, host_reply_tx, bridge);
                        while main.pump() {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    })
                    .map_err(EngineError::Io)?;
                (None, Some(thread))
            }
        };

        Ok(Engine {
            control: control_tx,
            control_thread: Some(control_thread),
            host_thread,
            main_thread,
            stream: Some(stream),
            info,
            shared,
            snapshot,
            next_ticket: AtomicU64::new(1),
            midi_feed,
            _midi: midi,
        })
    }

    fn send(&self, message: ControlMsg) {
        // A send only fails once the control thread has gone, at shutdown.
        let _ = self.control.send(message);
    }

    /// Submits a batch of ops. Returns a ticket; the outcome arrives as [`Event::Applied`] or
    /// [`Event::Rejected`] carrying the same ticket.
    pub fn apply(&self, ops: Vec<Op>) -> u64 {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        self.send(ControlMsg::Apply {
            ticket,
            ops,
            reply: None,
        });
        ticket
    }

    /// Submits a batch of ops and waits for the outcome.
    pub fn apply_wait(&self, ops: Vec<Op>) -> Result<Applied, Rejected> {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        let (reply, outcome) = mpsc::channel();
        self.send(ControlMsg::Apply {
            ticket,
            ops,
            reply: Some(reply),
        });
        outcome
            .recv()
            .expect("the control thread answers every apply")
    }

    pub fn transport(&self, command: TransportCommand) {
        self.send(ControlMsg::Transport(command));
    }

    /// Sets a plugin parameter (normalised 0..=1).
    pub fn set_param(&self, slot: SlotId, id: u32, value: f64) {
        self.send(ControlMsg::SetParam { slot, id, value });
    }

    /// Tells the engine that a control is being held (`down`) or was released. This is what
    /// drives touch and latch automation for faders, pans and sends; plugin editors report their
    /// own gestures.
    pub fn touch(&self, track: TrackId, target: riddim_model::AutomationTarget, down: bool) {
        self.send(ControlMsg::Touch {
            track,
            target,
            down,
        });
    }

    /// Plays a note or controller into a track's instrument, as an on-screen keyboard would.
    pub fn play_note(&self, track: TrackId, body: EventBody) {
        self.send(ControlMsg::PlayNote { track, body });
    }

    /// Asks every loaded plugin for its current state and stores it in the model, ready to be
    /// saved with the project. Blocks until done.
    ///
    /// With an external main thread this must not be called on that thread (the plugins answer
    /// from it): use [`request_plugin_states`](Self::request_plugin_states) there instead.
    pub fn capture_plugin_states(&self) -> Result<Applied, String> {
        self.request_plugin_states()
            .recv()
            .map_err(|_| "the engine has shut down".to_string())?
    }

    /// As [`capture_plugin_states`](Self::capture_plugin_states), without waiting: the result
    /// arrives on the returned channel once every plugin has answered.
    pub fn request_plugin_states(&self) -> Receiver<Result<Applied, String>> {
        let (reply, outcome) = mpsc::channel();
        self.send(ControlMsg::CapturePluginStates(reply));
        outcome
    }

    /// True if the engine runs on the manual device and is advanced with [`run`](Self::run).
    pub fn is_manual(&self) -> bool {
        matches!(self.stream, Some(Stream::Manual(_)))
    }

    /// Replaces the whole model, as when opening a project. Returns the new revision.
    pub fn load(&self, model: Model) -> Result<u64, String> {
        let (reply, outcome) = mpsc::channel();
        self.send(ControlMsg::Load {
            model: Box::new(model),
            reply,
        });
        outcome
            .recv()
            .map_err(|_| "the engine has shut down".to_string())?
    }

    /// A stream of engine events from now on.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (subscriber, events) = mpsc::channel();
        self.send(ControlMsg::Subscribe(subscriber));
        events
    }

    /// Waits until the control thread has handled everything sent so far, including plugins
    /// finishing loading. (With an external main thread, something must be pumping it.)
    pub fn sync(&self) {
        let (reply, done) = mpsc::channel();
        self.send(ControlMsg::Sync(reply));
        let _ = done.recv();
    }

    /// The current model.
    pub fn model(&self) -> Arc<Model> {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    /// Changes the audio buffer size while the engine runs, without reopening the project.
    ///
    /// The device pauses for the change. The engine then runs at the size the device settled
    /// on (which may be a clamped value: see the returned [`StreamInfo`]); every plugin is
    /// told the new maximum block, which takes each out of the mix for a moment
    /// ([`Event::PluginRestarted`] when it is back). Plugin settings, the transport position
    /// and the model are untouched. Not allowed while recording.
    pub fn set_buffer_size(&mut self, frames: u32) -> Result<StreamInfo, EngineError> {
        if !(MIN_BUFFER_FRAMES..=MAX_BUFFER_FRAMES).contains(&frames) {
            return Err(EngineError::Refused(
                "the buffer size must be 16 to 4096 frames",
            ));
        }
        if self.shared.recording.load(Ordering::Acquire) {
            return Err(EngineError::Refused(
                "the buffer size cannot change while recording",
            ));
        }
        let result = match self.stream.as_mut() {
            Some(Stream::Manual(stream)) => {
                stream.set_buffer_frames(frames);
                Ok(())
            }
            Some(Stream::Running(stream)) => stream.set_buffer_frames(frames),
            None => return Err(EngineError::Refused("the engine has no audio stream")),
        };
        // Whatever the device did, follow it: on an error it may or may not have changed.
        self.info = match self.stream.as_ref() {
            Some(Stream::Manual(stream)) => stream.info().clone(),
            Some(Stream::Running(stream)) => stream.info().clone(),
            None => self.info.clone(),
        };
        self.send(ControlMsg::StreamChanged {
            info: self.info.clone(),
            max_block: (self.info.max_frames as usize).max(MIN_BUFFER_FRAMES as usize),
        });
        result
            .map(|()| self.info.clone())
            .map_err(|e| EngineError::Device(e.to_string()))
    }

    pub fn status(&self) -> Status {
        let playing = self.shared.playing.load(Ordering::Acquire);
        let recording = self.shared.recording.load(Ordering::Acquire);
        Status {
            transport: match (playing, recording) {
                (true, true) => TransportState::Recording,
                (true, false) => TransportState::Playing,
                _ => TransportState::Stopped,
            },
            position_samples: self.shared.position.load(Ordering::Acquire),
            sample_rate: self.info.sample_rate as u32,
            block_frames: self.info.max_frames,
            model_revision: self.shared.model_revision.load(Ordering::Acquire),
            blocks: self.shared.blocks.load(Ordering::Relaxed),
            xruns: match &self.stream {
                Some(Stream::Running(stream)) => stream.xruns(),
                _ => 0,
            },
            rt_violations: self.shared.rt_violations.load(Ordering::Relaxed),
            capture_dropped: self.shared.capture_dropped.load(Ordering::Relaxed),
            capture_backlog: self.shared.capture_backlog.load(Ordering::Relaxed),
            plan_latency: self.shared.plan_latency.load(Ordering::Acquire),
            input_latency: self.info.input_latency_frames,
            output_latency: self.info.output_latency_frames,
            dsp_load: self
                .shared
                .dsp_load_reported
                .load(Ordering::Relaxed)
                .max(self.shared.dsp_load.load(Ordering::Relaxed)),
            events_lost: self.shared.events_lost.load(Ordering::Relaxed),
            playback_starved: self.shared.stream.starved.load(Ordering::Relaxed),
            stream_pages: self.shared.stream.pages.load(Ordering::Relaxed),
        }
    }

    /// Advances a [`DeviceConfig::Manual`] engine by `frames` frames, in device-sized blocks,
    /// on the calling thread. Returns what the device "played": one `Vec` per output channel.
    ///
    /// Panics if the engine runs on a real or timer-driven device.
    pub fn run(&mut self, frames: usize) -> Vec<Vec<f32>> {
        let Some(Stream::Manual(stream)) = self.stream.as_mut() else {
            panic!("Engine::run needs a manual device");
        };
        let block = stream.info().max_frames as usize;
        let channels = stream.info().output_channels as usize;
        let mut output = vec![Vec::with_capacity(frames); channels];
        let mut done = 0;
        while done < frames {
            let now = block.min(frames - done);
            stream.run_block(now);
            for (channel, sink) in output.iter_mut().enumerate() {
                sink.extend_from_slice(&stream.last_output(channel)[..now]);
            }
            done += now;
        }
        output
    }

    /// Feeds raw MIDI bytes in as if a keyboard had sent them. Only available when the engine
    /// was started without system MIDI input.
    pub fn feed_midi(&mut self, bytes: &[u8]) -> bool {
        let Some(feed) = self.midi_feed.as_mut() else {
            return false;
        };
        let mut running = 0;
        let mut ok = true;
        riddim_io::midi::parse_stream(bytes, 0, &mut running, |message| {
            ok &= feed.push(message).is_ok();
        });
        ok
    }

    /// Takes the main-thread half of the engine. Only present with
    /// [`MainThreadMode::External`], and only once.
    ///
    /// Call this on the thread that will pump it, which on macOS must be the application's
    /// main thread: the half that comes back holds the plugin instances and cannot be sent
    /// to another thread. The `Engine` itself can be (it is `Send`), so an application can
    /// take this on its main thread and keep the engine wherever its commands are handled.
    pub fn take_main_thread(&mut self) -> Option<MainThread> {
        let (requests, replies, bridge) = self.main_thread.take()?;
        Some(MainThread::new(requests, replies, bridge))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop audio first so nothing is running while the rest is torn down.
        if let Some(Stream::Running(mut stream)) = self.stream.take() {
            stream.stop();
        }
        self.stream = None;
        let _ = self.control.send(ControlMsg::Shutdown);
        if let Some(thread) = self.control_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.host_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn open_system_stream(
    config: &riddim_io::StreamConfig,
    audio: AudioEngine,
    workgroup: &crate::workgroup::Slot,
) -> Result<(Stream, StreamInfo), EngineError> {
    let mut stream = riddim_io::coreaudio::CoreAudioStream::open(config, audio)
        .map_err(|e| EngineError::Device(e.to_string()))?;
    // The render threads join the device's audio workgroup, so the system schedules them
    // together with the device's own I/O thread.
    workgroup.set(
        stream.workgroup(),
        stream.info().max_frames,
        stream.info().sample_rate,
    );
    let info = stream.info().clone();
    stream
        .start()
        .map_err(|e| EngineError::Device(e.to_string()))?;
    Ok((Stream::Running(Box::new(stream)), info))
}

#[cfg(not(target_os = "macos"))]
fn open_system_stream(
    _config: &riddim_io::StreamConfig,
    _audio: AudioEngine,
    _workgroup: &crate::workgroup::Slot,
) -> Result<(Stream, StreamInfo), EngineError> {
    Err(EngineError::Device(
        "there is no audio hardware backend for this platform yet".into(),
    ))
}
