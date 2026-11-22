//! Hosting a plugin in a process of its own.
//!
//! A plugin is somebody else's code loaded into our address space; if it crashes in process,
//! the session goes with it. A bridged plugin runs in a child process (`riddim-bridge`), and
//! what the engine holds is a [`PluginInstance`] that forwards to it:
//!
//! * **Everything that is not real-time** (buses, parameters, state, activation) is a request
//!   and a reply, one JSON object per line, over the child's standard input and output.
//! * **Audio and events** are in memory shared between the two processes. For each block the
//!   audio thread writes its inputs there, sends one byte down a socket, and waits for one
//!   byte back; the child's audio thread does the plugin's `process` in between.
//!
//! The hand-off costs two system calls and two context switches per block, on the audio
//! thread, which is the price of isolation and the reason it is opt-in per plugin. If the
//! child dies or stops answering, the bridge goes quiet (silence out, [`PluginEvent::Crashed`]
//! to the host) and everything else carries on.
//!
//! One plugin per child for now; the vision's grouping by manufacturer is not built. A
//! bridged plugin has no editor yet. Unix only.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::descriptor::{BusDirection, BusLayout, PluginDescriptor};
use crate::error::{PluginError, Result};
use crate::event::{Event, EventBody, EventBuffer};
use crate::instance::{
    ActivateConfig, ParamFlags, ParamId, ParamInfo, PluginEvent, PluginInstance, PluginProcessor,
    ProcessMode, Tail,
};
use crate::process::{ProcessContext, ProcessStatus, TransportInfo};
use crate::state::StateBlob;

/// Environment variable naming the bridge executable.
pub const BRIDGE_ENV: &str = "RIDDIM_BRIDGE";

/// Events per block the shared memory has room for, each way.
const EVENT_CAPACITY: usize = 2048;

/// How long a request on the control channel may take. Loading a sampler's library is the
/// slow case.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);

/// The bridge executable: `$RIDDIM_BRIDGE`, or `riddim-bridge` beside the running program.
pub fn default_executable() -> PathBuf {
    if let Some(path) = std::env::var_os(BRIDGE_ENV) {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("riddim-bridge")))
        .unwrap_or_else(|| PathBuf::from("riddim-bridge"))
}

// ---------------------------------------------------------------------------------------------
// The control channel
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "call")]
enum Request {
    BusLayout,
    SetBusActive {
        input: bool,
        index: usize,
        active: bool,
    },
    Activate {
        sample_rate: f64,
        max_block: u32,
        offline: bool,
        memory: PathBuf,
        socket: PathBuf,
    },
    Deactivate,
    Params,
    ParamValue {
        id: u32,
    },
    SetParamValue {
        id: u32,
        value: f64,
    },
    ParamText {
        id: u32,
        value: f64,
    },
    SaveState,
    RestoreState {
        state: String,
    },
    Latency,
    PollEvents,
    Quit,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reply")]
enum Reply {
    Ready,
    Ok,
    Error { message: String },
    Layout { layout: BusLayout },
    Activated { latency: u32 },
    Params { params: Vec<WireParam> },
    Value { value: Option<f64> },
    Text { text: Option<String> },
    State { state: String },
    Latency { samples: u32 },
    Events { events: Vec<WireEvent> },
}

#[derive(Debug, Serialize, Deserialize)]
struct WireParam {
    id: u32,
    name: String,
    unit: String,
    default: f64,
    steps: u32,
    automatable: bool,
    read_only: bool,
    hidden: bool,
    bypass: bool,
}

/// A [`PluginEvent`] on the wire.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
enum WireEvent {
    GestureBegin { id: u32 },
    Changed { id: u32, value: f64 },
    GestureEnd { id: u32 },
    ValuesChanged,
    InfoChanged,
    LatencyChanged,
    IoChanged,
    RestartRequested,
    StateDirty,
}

impl WireEvent {
    fn from_event(event: &PluginEvent) -> Option<WireEvent> {
        Some(match *event {
            PluginEvent::ParamGestureBegin { id } => WireEvent::GestureBegin { id },
            PluginEvent::ParamChanged { id, value } => WireEvent::Changed { id, value },
            PluginEvent::ParamGestureEnd { id } => WireEvent::GestureEnd { id },
            PluginEvent::ParamValuesChanged => WireEvent::ValuesChanged,
            PluginEvent::ParamInfoChanged => WireEvent::InfoChanged,
            PluginEvent::LatencyChanged => WireEvent::LatencyChanged,
            PluginEvent::IoChanged => WireEvent::IoChanged,
            PluginEvent::RestartRequested => WireEvent::RestartRequested,
            PluginEvent::StateDirty => WireEvent::StateDirty,
            // There is no editor on the other side to resize, and a child does not report
            // its own death.
            PluginEvent::EditorResizeRequest { .. } | PluginEvent::Crashed => return None,
        })
    }

    fn into_event(self) -> PluginEvent {
        match self {
            WireEvent::GestureBegin { id } => PluginEvent::ParamGestureBegin { id },
            WireEvent::Changed { id, value } => PluginEvent::ParamChanged { id, value },
            WireEvent::GestureEnd { id } => PluginEvent::ParamGestureEnd { id },
            WireEvent::ValuesChanged => PluginEvent::ParamValuesChanged,
            WireEvent::InfoChanged => PluginEvent::ParamInfoChanged,
            WireEvent::LatencyChanged => PluginEvent::LatencyChanged,
            WireEvent::IoChanged => PluginEvent::IoChanged,
            WireEvent::RestartRequested => PluginEvent::RestartRequested,
            WireEvent::StateDirty => PluginEvent::StateDirty,
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The shared memory
// ---------------------------------------------------------------------------------------------

/// The fixed part of the shared block. Written by the host before a block, and by the child
/// (the last two fields) after it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    frames: u32,
    events_in: u32,
    playing: u32,
    recording: u32,
    looping: u32,
    time_sig_numerator: u32,
    time_sig_denominator: u32,
    _pad: u32,
    sample_rate: f64,
    position_samples: i64,
    continuous_samples: i64,
    position_quarters: f64,
    bar_start_quarters: f64,
    loop_start_quarters: f64,
    loop_end_quarters: f64,
    tempo_bpm: f64,
    // From the child:
    events_out: u32,
    failed: u32,
}

/// An [`Event`] as plain data.
#[repr(C)]
#[derive(Clone, Copy)]
struct WireAudioEvent {
    offset: u32,
    kind: u32,
    a: u32,
    b: u32,
    c: i32,
    _pad: u32,
    value: f64,
}

impl WireAudioEvent {
    fn pack(event: &Event) -> WireAudioEvent {
        let (kind, a, b, c, value) = match event.body {
            EventBody::NoteOn {
                channel,
                key,
                velocity,
                note_id,
            } => (0, channel as u32, key as u32, note_id, velocity as f64),
            EventBody::NoteOff {
                channel,
                key,
                velocity,
                note_id,
            } => (1, channel as u32, key as u32, note_id, velocity as f64),
            EventBody::Midi { data, len } => (
                2,
                u32::from_le_bytes([data[0], data[1], data[2], 0]),
                len as u32,
                0,
                0.0,
            ),
            EventBody::Param { id, value } => (3, id, 0, 0, value),
        };
        WireAudioEvent {
            offset: event.offset,
            kind,
            a,
            b,
            c,
            _pad: 0,
            value,
        }
    }

    fn unpack(&self) -> Option<Event> {
        let body = match self.kind {
            0 => EventBody::NoteOn {
                channel: self.a as u8,
                key: self.b as u8,
                velocity: self.value as f32,
                note_id: self.c,
            },
            1 => EventBody::NoteOff {
                channel: self.a as u8,
                key: self.b as u8,
                velocity: self.value as f32,
                note_id: self.c,
            },
            2 => {
                let bytes = self.a.to_le_bytes();
                EventBody::Midi {
                    data: [bytes[0], bytes[1], bytes[2]],
                    len: self.b.min(3) as u8,
                }
            }
            3 => EventBody::Param {
                id: self.a,
                value: self.value,
            },
            _ => return None,
        };
        Some(Event {
            offset: self.offset,
            body,
        })
    }
}

/// Where each part of the shared block is, for one activation.
#[derive(Clone, Copy)]
struct Layout {
    in_channels: usize,
    out_channels: usize,
    max_block: usize,
}

impl Layout {
    fn events_in(&self) -> usize {
        std::mem::size_of::<Header>().next_multiple_of(16)
    }

    fn events_out(&self) -> usize {
        self.events_in() + EVENT_CAPACITY * std::mem::size_of::<WireAudioEvent>()
    }

    fn audio_in(&self) -> usize {
        self.events_out() + EVENT_CAPACITY * std::mem::size_of::<WireAudioEvent>()
    }

    fn audio_out(&self) -> usize {
        self.audio_in() + self.in_channels * self.max_block * std::mem::size_of::<f32>()
    }

    fn size(&self) -> usize {
        self.audio_out() + self.out_channels * self.max_block * std::mem::size_of::<f32>()
    }
}

/// A file mapped into this process, shared with the process at the other end of the bridge.
struct Shared {
    base: *mut u8,
    len: usize,
    layout: Layout,
}

// SAFETY: the mapping is plain memory. The two processes take strict turns with it (the byte
// on the socket is the hand-off), and within a process only the audio side touches it.
unsafe impl Send for Shared {}

impl Shared {
    /// Maps `path`, creating it at the right size first if `create`.
    fn map(path: &Path, layout: Layout, create: bool) -> Result<Shared> {
        use std::os::fd::AsRawFd;
        let io = |e: std::io::Error| PluginError::Io(format!("{}: {e}", path.display()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(create)
            .open(path)
            .map_err(io)?;
        let len = layout.size();
        if create {
            file.set_len(len as u64).map_err(io)?;
        } else if file.metadata().map_err(io)?.len() < len as u64 {
            return Err(PluginError::Io(format!(
                "{} is smaller than the bridge needs",
                path.display()
            )));
        }
        // SAFETY: mapping `len` bytes of a file we have just opened read-write and checked the
        // size of. The mapping stays valid after the file handle is closed.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(Shared {
            base: base.cast(),
            len,
            layout,
        })
    }

    #[allow(clippy::mut_from_ref)]
    fn header(&self) -> &mut Header {
        // SAFETY: the mapping starts with a Header (page-aligned, large enough), and whoever
        // holds the turn is the only one looking.
        unsafe { &mut *self.base.cast::<Header>() }
    }

    #[allow(clippy::mut_from_ref)]
    fn events(&self, out: bool) -> &mut [WireAudioEvent] {
        let offset = if out {
            self.layout.events_out()
        } else {
            self.layout.events_in()
        };
        // SAFETY: inside the mapping by construction of `Layout`, aligned to 16.
        unsafe { std::slice::from_raw_parts_mut(self.base.add(offset).cast(), EVENT_CAPACITY) }
    }

    #[allow(clippy::mut_from_ref)]
    fn channel(&self, out: bool, channel: usize) -> &mut [f32] {
        let (start, channels) = if out {
            (self.layout.audio_out(), self.layout.out_channels)
        } else {
            (self.layout.audio_in(), self.layout.in_channels)
        };
        assert!(channel < channels);
        let offset = start + channel * self.layout.max_block * std::mem::size_of::<f32>();
        // SAFETY: inside the mapping by construction of `Layout`; f32 alignment holds because
        // every part before it is a multiple of 8 bytes long.
        unsafe {
            std::slice::from_raw_parts_mut(self.base.add(offset).cast(), self.layout.max_block)
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `map` mapped.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

// ---------------------------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------------------------

static NEXT_BRIDGE: AtomicU64 = AtomicU64::new(0);

/// A plugin running in a child process (control side).
pub struct BridgedInstance {
    descriptor: PluginDescriptor,
    child: Child,
    stdin: std::cell::RefCell<Option<ChildStdin>>,
    replies: Receiver<String>,
    layout: BusLayout,
    latency: u32,
    active: bool,
    /// Set by the audio side when the child stops answering.
    dead: Arc<AtomicBool>,
    reported_dead: bool,
    /// The shared-memory file and socket of the current activation, removed when it ends.
    files: Vec<PathBuf>,
}

/// Starts `executable` and has it load the described plugin.
pub fn instantiate(descriptor: &PluginDescriptor, executable: &Path) -> Result<BridgedInstance> {
    let arg = serde_json::to_string(descriptor).map_err(|e| PluginError::Load(e.to_string()))?;
    let mut child = Command::new(executable)
        .arg(arg)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PluginError::Load(format!("{}: {e}", executable.display())))?;
    let stdin = std::cell::RefCell::new(child.stdin.take());
    let stdout = child.stdout.take().expect("stdout was piped");
    // Replies are read on a thread of their own so that waiting for one can time out.
    let (tx, replies) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("riddim-bridge-replies".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| PluginError::Io(e.to_string()))?;

    let mut instance = BridgedInstance {
        descriptor: descriptor.clone(),
        child,
        stdin,
        replies,
        layout: BusLayout::default(),
        latency: 0,
        active: false,
        dead: Arc::new(AtomicBool::new(false)),
        reported_dead: false,
        files: Vec::new(),
    };
    match instance.read_reply()? {
        Reply::Ready => {}
        Reply::Error { message } => return Err(PluginError::Load(message)),
        other => return Err(PluginError::Load(format!("unexpected reply {other:?}"))),
    }
    instance.refresh_layout()?;
    Ok(instance)
}

impl BridgedInstance {
    fn gone(&self) -> PluginError {
        PluginError::Plugin("the plugin's process has gone".into())
    }

    fn read_reply(&self) -> Result<Reply> {
        match self.replies.recv_timeout(CONTROL_TIMEOUT) {
            Ok(line) => serde_json::from_str(&line)
                .map_err(|e| PluginError::Plugin(format!("bad reply from the bridge: {e}"))),
            Err(RecvTimeoutError::Timeout) => {
                self.dead.store(true, Ordering::Release);
                Err(PluginError::Plugin(
                    "the plugin's process stopped answering".into(),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.dead.store(true, Ordering::Release);
                Err(self.gone())
            }
        }
    }

    fn call(&self, request: &Request) -> Result<Reply> {
        if self.dead.load(Ordering::Acquire) {
            return Err(self.gone());
        }
        let line = serde_json::to_string(request).expect("requests always serialise");
        let sent = match self.stdin.borrow_mut().as_mut() {
            Some(stdin) => writeln!(stdin, "{line}")
                .and_then(|()| stdin.flush())
                .map_err(|_| self.gone()),
            None => Err(self.gone()),
        };
        if let Err(error) = sent {
            self.dead.store(true, Ordering::Release);
            return Err(error);
        }
        match self.read_reply()? {
            Reply::Error { message } => Err(PluginError::Plugin(message)),
            reply => Ok(reply),
        }
    }

    fn refresh_layout(&mut self) -> Result<()> {
        if let Reply::Layout { layout } = self.call(&Request::BusLayout)? {
            self.layout = layout;
        }
        Ok(())
    }

    fn remove_files(&mut self) {
        for path in self.files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl PluginInstance for BridgedInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    fn bus_layout(&self) -> BusLayout {
        self.layout.clone()
    }

    fn set_bus_active(
        &mut self,
        direction: BusDirection,
        index: usize,
        active: bool,
    ) -> Result<()> {
        self.call(&Request::SetBusActive {
            input: direction == BusDirection::Input,
            index,
            active,
        })?;
        self.refresh_layout()
    }

    fn activate(&mut self, config: &ActivateConfig) -> Result<Box<dyn PluginProcessor>> {
        if self.active {
            return Err(PluginError::InvalidState("already active"));
        }
        self.refresh_layout()?;
        let layout = Layout {
            in_channels: self.layout.active_input_channels(),
            out_channels: self.layout.active_output_channels(),
            max_block: config.max_block.max(1) as usize,
        };
        let stem = std::env::temp_dir().join(format!(
            "riddim-bridge-{}-{}",
            std::process::id(),
            NEXT_BRIDGE.fetch_add(1, Ordering::Relaxed)
        ));
        let memory = stem.with_extension("mem");
        let socket = stem.with_extension("sock");
        let shared = Shared::map(&memory, layout, true)?;
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)
            .map_err(|e| PluginError::Io(format!("{}: {e}", socket.display())))?;
        self.files = vec![memory.clone(), socket.clone()];

        let offline = config.mode == ProcessMode::Offline;
        let reply = self.call(&Request::Activate {
            sample_rate: config.sample_rate,
            max_block: config.max_block,
            offline,
            memory,
            socket,
        });
        let latency = match reply {
            Ok(Reply::Activated { latency }) => latency,
            Ok(other) => {
                self.remove_files();
                return Err(PluginError::Plugin(format!("unexpected reply {other:?}")));
            }
            Err(error) => {
                self.remove_files();
                return Err(error);
            }
        };
        // The child connected before it replied.
        listener
            .set_nonblocking(true)
            .map_err(|e| PluginError::Io(e.to_string()))?;
        let (stream, _) = listener
            .accept()
            .map_err(|e| PluginError::Plugin(format!("the bridge did not connect: {e}")))?;
        stream
            .set_nonblocking(false)
            .map_err(|e| PluginError::Io(e.to_string()))?;
        // How long the audio thread will wait for a block before giving the plugin up for
        // dead. Generous offline, where there is no deadline; a handful of blocks live, and
        // never less than a tenth of a second.
        let block = config.max_block as f64 / config.sample_rate.max(1.0);
        let patience = if offline {
            Duration::from_secs(30)
        } else {
            Duration::from_secs_f64((block * 8.0).max(0.1))
        };
        stream
            .set_read_timeout(Some(patience))
            .and_then(|()| stream.set_write_timeout(Some(patience)))
            .map_err(|e| PluginError::Io(e.to_string()))?;

        self.latency = latency;
        self.active = true;
        Ok(Box::new(BridgedProcessor {
            shared,
            stream,
            dead: self.dead.clone(),
            patience,
        }))
    }

    fn deactivate(&mut self, processor: Box<dyn PluginProcessor>) {
        // Closing the socket is what tells the child's audio thread to stop.
        drop(processor);
        if self.active {
            let _ = self.call(&Request::Deactivate);
            self.active = false;
        }
        self.remove_files();
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn params(&self) -> Vec<ParamInfo> {
        let Ok(Reply::Params { params }) = self.call(&Request::Params) else {
            return Vec::new();
        };
        params
            .into_iter()
            .map(|p| ParamInfo {
                id: p.id,
                name: p.name,
                unit: p.unit,
                default: p.default,
                steps: p.steps,
                flags: ParamFlags {
                    automatable: p.automatable,
                    read_only: p.read_only,
                    hidden: p.hidden,
                    bypass: p.bypass,
                },
            })
            .collect()
    }

    fn param_value(&self, id: ParamId) -> Option<f64> {
        match self.call(&Request::ParamValue { id }) {
            Ok(Reply::Value { value }) => value,
            _ => None,
        }
    }

    fn set_param_value(&mut self, id: ParamId, value: f64) {
        let _ = self.call(&Request::SetParamValue { id, value });
    }

    fn param_text(&self, id: ParamId, value: f64) -> Option<String> {
        match self.call(&Request::ParamText { id, value }) {
            Ok(Reply::Text { text }) => text,
            _ => None,
        }
    }

    fn save_state(&mut self) -> Result<StateBlob> {
        match self.call(&Request::SaveState)? {
            Reply::State { state } => {
                let bytes = from_hex(&state).ok_or_else(|| {
                    PluginError::State("the bridge sent a malformed state".into())
                })?;
                StateBlob::from_bytes(&bytes)
            }
            other => Err(PluginError::Plugin(format!("unexpected reply {other:?}"))),
        }
    }

    fn restore_state(&mut self, state: &StateBlob) -> Result<()> {
        self.call(&Request::RestoreState {
            state: to_hex(&state.to_bytes()),
        })
        .map(|_| ())
    }

    fn latency_samples(&self) -> u32 {
        self.latency
    }

    fn tail(&self) -> Tail {
        Tail::Samples(0)
    }

    fn poll_events(&mut self, out: &mut Vec<PluginEvent>) {
        // A child that has exited is as dead as one that stopped answering.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            self.dead.store(true, Ordering::Release);
        }
        if self.dead.load(Ordering::Acquire) {
            if !self.reported_dead {
                self.reported_dead = true;
                out.push(PluginEvent::Crashed);
            }
            return;
        }
        if let Ok(Reply::Events { events }) = self.call(&Request::PollEvents) {
            for event in events {
                let event = event.into_event();
                if event == PluginEvent::LatencyChanged {
                    if let Ok(Reply::Latency { samples }) = self.call(&Request::Latency) {
                        self.latency = samples;
                    }
                }
                out.push(event);
            }
        }
    }
}

impl Drop for BridgedInstance {
    fn drop(&mut self) {
        if !self.dead.load(Ordering::Acquire) {
            if let Some(stdin) = self.stdin.borrow_mut().as_mut() {
                let line = serde_json::to_string(&Request::Quit).expect("requests serialise");
                let _ = writeln!(stdin, "{line}");
                let _ = stdin.flush();
            }
        }
        // Closing its input ends a child that is listening; one that is not gets no say.
        drop(self.stdin.borrow_mut().take());
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        self.remove_files();
    }
}

/// The audio side of a bridged plugin.
struct BridgedProcessor {
    shared: Shared,
    stream: UnixStream,
    dead: Arc<AtomicBool>,
    /// How long a block is waited for.
    patience: Duration,
}

/// How long a plugin's first block is waited for when nothing is waiting on it: as long as
/// a block of an offline render.
const FIRST_BLOCK_PATIENCE: Duration = Duration::from_secs(30);

impl BridgedProcessor {
    /// Hands the block to the child and waits for it to come back. False if it did not.
    fn exchange(&mut self) -> bool {
        // Two system calls on the audio thread, one of which waits: this is what a bridge is.
        riddim_rt::guard::permit(|| {
            let mut byte = [1u8];
            self.stream.write_all(&byte).is_ok() && matches!(self.stream.read(&mut byte), Ok(1))
        })
    }
}

impl PluginProcessor for BridgedProcessor {
    fn first_block(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus {
        // A plugin may take far longer over its first block than over any other, and live
        // that would look like a hang. Nothing is waiting this time.
        let wait = |stream: &UnixStream, patience: Duration| {
            let _ = stream.set_read_timeout(Some(patience));
            let _ = stream.set_write_timeout(Some(patience));
        };
        wait(&self.stream, self.patience.max(FIRST_BLOCK_PATIENCE));
        let status = self.process(context);
        wait(&self.stream, self.patience);
        status
    }

    fn process(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus {
        let frames = context.frames.min(self.shared.layout.max_block);
        let silence = |context: &mut ProcessContext<'_>| {
            for output in context.outputs.iter_mut() {
                output[..frames].fill(0.0);
            }
        };
        if self.dead.load(Ordering::Relaxed) {
            silence(context);
            return ProcessStatus::Failed;
        }

        let info: &TransportInfo = context.transport;
        let header = self.shared.header();
        *header = Header {
            frames: frames as u32,
            events_in: context.events_in.len().min(EVENT_CAPACITY) as u32,
            playing: info.playing as u32,
            recording: info.recording as u32,
            looping: info.looping as u32,
            time_sig_numerator: info.time_sig_numerator as u32,
            time_sig_denominator: info.time_sig_denominator as u32,
            _pad: 0,
            sample_rate: info.sample_rate,
            position_samples: info.position_samples,
            continuous_samples: info.continuous_samples,
            position_quarters: info.position_quarters,
            bar_start_quarters: info.bar_start_quarters,
            loop_start_quarters: info.loop_start_quarters,
            loop_end_quarters: info.loop_end_quarters,
            tempo_bpm: info.tempo_bpm,
            events_out: 0,
            failed: 0,
        };
        let events = self.shared.events(false);
        for (slot, event) in events.iter_mut().zip(context.events_in) {
            *slot = WireAudioEvent::pack(event);
        }
        for (channel, input) in context
            .inputs
            .iter()
            .enumerate()
            .take(self.shared.layout.in_channels)
        {
            self.shared.channel(false, channel)[..frames].copy_from_slice(&input[..frames]);
        }

        if !self.exchange() {
            // Dead or hung; either way it has had its chance. Say so once, and stay quiet.
            self.dead.store(true, Ordering::Release);
            silence(context);
            return ProcessStatus::Failed;
        }

        let header = *self.shared.header();
        for (channel, output) in context.outputs.iter_mut().enumerate() {
            if channel < self.shared.layout.out_channels {
                output[..frames].copy_from_slice(&self.shared.channel(true, channel)[..frames]);
            } else {
                output[..frames].fill(0.0);
            }
        }
        let count = (header.events_out as usize).min(EVENT_CAPACITY);
        for event in &self.shared.events(true)[..count] {
            if let Some(event) = event.unpack() {
                context.events_out.push(event);
            }
        }
        if header.failed != 0 {
            ProcessStatus::Failed
        } else {
            ProcessStatus::Ok
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------------------------

/// The child's audio thread: one block per byte received, until the host hangs up.
fn serve_audio(
    mut processor: Box<dyn PluginProcessor>,
    shared: Shared,
    mut stream: UnixStream,
) -> Box<dyn PluginProcessor> {
    let layout = shared.layout;
    let mut events_in = EventBuffer::with_capacity(EVENT_CAPACITY);
    let mut events_out = EventBuffer::with_capacity(EVENT_CAPACITY);
    let mut scratch =
        riddim_rt::planar::PlanarScratch::new(layout.in_channels, layout.out_channels);
    let _denormals = riddim_rt::denormal::ScopedNoDenormals::new();
    let mut byte = [0u8];
    while matches!(stream.read(&mut byte), Ok(1)) {
        let header = *shared.header();
        let frames = (header.frames as usize).min(layout.max_block);
        events_in.clear();
        for event in &shared.events(false)[..(header.events_in as usize).min(EVENT_CAPACITY)] {
            if let Some(event) = event.unpack() {
                events_in.push(event);
            }
        }
        events_out.clear();
        let transport = TransportInfo {
            playing: header.playing != 0,
            recording: header.recording != 0,
            looping: header.looping != 0,
            sample_rate: header.sample_rate,
            position_samples: header.position_samples,
            continuous_samples: header.continuous_samples,
            position_quarters: header.position_quarters,
            bar_start_quarters: header.bar_start_quarters,
            loop_start_quarters: header.loop_start_quarters,
            loop_end_quarters: header.loop_end_quarters,
            tempo_bpm: header.tempo_bpm,
            time_sig_numerator: header.time_sig_numerator as u16,
            time_sig_denominator: header.time_sig_denominator as u16,
        };
        let status = {
            let (inputs, outputs) = scratch.fill(
                (0..layout.in_channels).map(|c| &*shared.channel(false, c)),
                (0..layout.out_channels).map(|c| shared.channel(true, c)),
                frames,
            );
            processor.process(&mut ProcessContext {
                frames,
                inputs,
                outputs,
                events_in: events_in.as_slice(),
                events_out: &mut events_out,
                transport: &transport,
            })
        };
        let out = shared.events(true);
        let produced = events_out.as_slice();
        for (slot, event) in out.iter_mut().zip(produced) {
            *slot = WireAudioEvent::pack(event);
        }
        let header = shared.header();
        header.events_out = produced.len().min(EVENT_CAPACITY) as u32;
        header.failed = (status == ProcessStatus::Failed) as u32;
        if stream.write_all(&byte).is_err() {
            break;
        }
    }
    processor
}

/// The bridge executable's whole job: load the plugin described by `descriptor_json`, then
/// answer requests on `input` until told to quit or the host goes away. Returns the exit code.
pub fn serve(descriptor_json: &str, input: impl BufRead, mut output: impl Write) -> i32 {
    let mut send = |reply: &Reply| {
        let line = serde_json::to_string(reply).expect("replies always serialise");
        let _ = writeln!(output, "{line}");
        let _ = output.flush();
    };
    let descriptor: PluginDescriptor = match serde_json::from_str(descriptor_json) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            send(&Reply::Error {
                message: format!("bad plugin description: {error}"),
            });
            return 2;
        }
    };
    let mut instance = match crate::host::instantiate(&descriptor) {
        Ok(instance) => instance,
        Err(error) => {
            send(&Reply::Error {
                message: error.to_string(),
            });
            return 1;
        }
    };
    send(&Reply::Ready);

    let mut audio: Option<std::thread::JoinHandle<Box<dyn PluginProcessor>>> = None;
    let mut pending = Vec::new();
    let fail = |error: PluginError| Reply::Error {
        message: error.to_string(),
    };
    for line in input.lines() {
        let Ok(line) = line else { break };
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                send(&Reply::Error {
                    message: format!("bad request: {error}"),
                });
                continue;
            }
        };
        let reply = match request {
            Request::BusLayout => Reply::Layout {
                layout: instance.bus_layout(),
            },
            Request::SetBusActive {
                input,
                index,
                active,
            } => {
                let direction = if input {
                    BusDirection::Input
                } else {
                    BusDirection::Output
                };
                match instance.set_bus_active(direction, index, active) {
                    Ok(()) => Reply::Ok,
                    Err(error) => fail(error),
                }
            }
            Request::Activate {
                sample_rate,
                max_block,
                offline,
                memory,
                socket,
            } => {
                let config = ActivateConfig {
                    sample_rate,
                    max_block,
                    mode: if offline {
                        ProcessMode::Offline
                    } else {
                        ProcessMode::Realtime
                    },
                };
                let layout = instance.bus_layout();
                let layout = Layout {
                    in_channels: layout.active_input_channels(),
                    out_channels: layout.active_output_channels(),
                    max_block: max_block.max(1) as usize,
                };
                let prepared = Shared::map(&memory, layout, false).and_then(|shared| {
                    let stream = UnixStream::connect(&socket)
                        .map_err(|e| PluginError::Io(format!("{}: {e}", socket.display())))?;
                    let processor = instance.activate(&config)?;
                    Ok((shared, stream, processor))
                });
                match prepared {
                    Ok((shared, stream, processor)) => {
                        let thread = std::thread::Builder::new()
                            .name("riddim-bridge-audio".into())
                            .spawn(move || serve_audio(processor, shared, stream));
                        match thread {
                            Ok(thread) => {
                                audio = Some(thread);
                                Reply::Activated {
                                    latency: instance.latency_samples(),
                                }
                            }
                            Err(error) => Reply::Error {
                                message: error.to_string(),
                            },
                        }
                    }
                    Err(error) => fail(error),
                }
            }
            Request::Deactivate => {
                // The host has closed the socket, so the audio thread is on its way out.
                if let Some(thread) = audio.take() {
                    if let Ok(processor) = thread.join() {
                        instance.deactivate(processor);
                    }
                }
                Reply::Ok
            }
            Request::Params => Reply::Params {
                params: instance
                    .params()
                    .into_iter()
                    .map(|p| WireParam {
                        id: p.id,
                        name: p.name,
                        unit: p.unit,
                        default: p.default,
                        steps: p.steps,
                        automatable: p.flags.automatable,
                        read_only: p.flags.read_only,
                        hidden: p.flags.hidden,
                        bypass: p.flags.bypass,
                    })
                    .collect(),
            },
            Request::ParamValue { id } => Reply::Value {
                value: instance.param_value(id),
            },
            Request::SetParamValue { id, value } => {
                instance.set_param_value(id, value);
                Reply::Ok
            }
            Request::ParamText { id, value } => Reply::Text {
                text: instance.param_text(id, value),
            },
            Request::SaveState => match instance.save_state() {
                Ok(state) => Reply::State {
                    state: to_hex(&state.to_bytes()),
                },
                Err(error) => fail(error),
            },
            Request::RestoreState { state } => {
                let restored = from_hex(&state)
                    .ok_or_else(|| PluginError::State("malformed state".into()))
                    .and_then(|bytes| StateBlob::from_bytes(&bytes))
                    .and_then(|blob| instance.restore_state(&blob));
                match restored {
                    Ok(()) => Reply::Ok,
                    Err(error) => fail(error),
                }
            }
            Request::Latency => Reply::Latency {
                samples: instance.latency_samples(),
            },
            Request::PollEvents => {
                pending.clear();
                instance.poll_events(&mut pending);
                Reply::Events {
                    events: pending.iter().filter_map(WireEvent::from_event).collect(),
                }
            }
            Request::Quit => {
                send(&Reply::Ok);
                break;
            }
        };
        send(&reply);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }

    #[test]
    fn audio_events_survive_the_shared_memory_form() {
        for event in [
            Event::note_on(7, 3, 60, 0.5),
            Event::note_off(9, 3, 60),
            Event::param(11, 0x7E57_0001, 0.625),
            Event::from_midi(13, &[0xB2, 7, 100]).unwrap(),
            Event::from_midi(14, &[0xC1, 5]).unwrap(),
        ] {
            assert_eq!(WireAudioEvent::pack(&event).unpack(), Some(event));
        }
    }

    #[test]
    fn the_shared_block_is_laid_out_without_overlap() {
        let layout = Layout {
            in_channels: 4,
            out_channels: 6,
            max_block: 512,
        };
        assert!(layout.events_in() >= std::mem::size_of::<Header>());
        assert_eq!(layout.events_in() % 16, 0);
        assert_eq!(layout.audio_in() % 8, 0);
        assert_eq!(layout.size() - layout.audio_out(), 6 * 512 * 4);
        assert_eq!(layout.audio_out() - layout.audio_in(), 4 * 512 * 4);
    }
}
