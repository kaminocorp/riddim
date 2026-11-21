//! The control side of the engine.
//!
//! One thread owns the model. It applies ops, compiles a plan for every change, hands plans to
//! the audio thread, takes retired ones back to free them, asks the main thread to load and
//! unload plugins, runs recordings, and turns what the audio thread reports into events.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use riddim_disk::capture::{Butler, CaptureId};
use riddim_disk::wav::{SampleFormat, WavSpec};
use riddim_graph::compile;
use riddim_graph::model_graph::{self, NodeKind, PluginPorts};
use riddim_io::StreamInfo;
use riddim_model::{
    AutomationLane, AutomationMode, AutomationTarget, Clip, ClipContent, ClipId, CurveShape, Media,
    MediaId, Model, Note, Op, OpError, SlotId, Track, TrackId, TrackInput, TrackKind, TrackOutput,
};
use riddim_plugin::{ActivateConfig, EventBody, PluginEvent, ProcessMode};
use riddim_rt::garbage::GarbageCollector;
use riddim_rt::planar::PlanarScratch;
use riddim_rt::spsc::{Consumer, Producer, PushError};
use riddim_time::{samples_to_superclock, Position, TempoMap, Ticks};

use crate::audio::{CaptureRt, FromAudio, ToAudio};
use crate::dsp::{Click, DelayLine, StereoGain};
use crate::events::{Event, TransportState};
use crate::host::{BusRequest, HostReply, HostRequest};
use crate::media::MediaCache;
use crate::plan::{
    Buffers, DelayCell, GainRt, MidiSourceRt, NodeCell, NodeRt, PluginRt, RtAudioClip, RtCurve,
    RtInput, RtPlan, RtPoint, RtSource, RtStep, StepParams, StripRt, TimedEvent,
    AUTOMATED_CAPACITY, EVENT_CAPACITY,
};
use crate::shared::{MeterCell, Shared};

/// A transport request from the client.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransportCommand {
    Play,
    Stop,
    /// Move the playhead.
    Locate(Position),
    /// Start recording on every armed track, with a count-in if the metronome asks for one.
    Record,
}

/// The result of a successfully applied batch.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    pub revision: u64,
    /// Undo the batch by applying these first to last.
    pub inverses: Vec<Op>,
}

/// Why a batch was refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Rejected {
    /// The op in the batch that failed.
    pub index: usize,
    pub error: OpError,
}

pub(crate) enum ControlMsg {
    Apply {
        ticket: u64,
        ops: Vec<Op>,
        reply: Option<Sender<Result<Applied, Rejected>>>,
    },
    Transport(TransportCommand),
    SetParam {
        slot: SlotId,
        id: u32,
        value: f64,
    },
    PlayNote {
        track: TrackId,
        body: EventBody,
    },
    Subscribe(Sender<Event>),
    /// Answered once everything sent before it has been dealt with.
    Sync(Sender<()>),
    /// Replace the whole model (opening a project).
    Load {
        model: Box<Model>,
        reply: Sender<Result<u64, String>>,
    },
    /// A control was grabbed or released (for touch and latch automation).
    Touch {
        track: TrackId,
        target: AutomationTarget,
        down: bool,
    },
    /// Ask every loaded plugin for its state and store it in the model.
    CapturePluginStates(Sender<Result<Applied, String>>),
    /// The device was re-opened with another buffer size: run at that size from now on.
    StreamChanged {
        info: StreamInfo,
        max_block: usize,
    },
    Shutdown,
}

enum PluginStatus {
    Loading,
    Loaded(PluginPorts),
    Failed,
}

struct PluginEntry {
    track: TrackId,
    status: PluginStatus,
    /// Names the instance to the plugin host (a slot can be emptied and filled again before
    /// the host has let go of what was in it).
    serial: u64,
    /// The buses and block size the plugin was last asked to run with.
    buses: BusRequest,
    max_block: u32,
}

/// The optional buses a slot's routing calls for.
fn wanted_buses(slot: &riddim_model::PluginSlot) -> BusRequest {
    BusRequest {
        extra_outputs: slot.outputs.iter().map(|o| o.bus).max().unwrap_or(0),
        sidechain: slot.sidechain.is_some(),
    }
}

struct AudioTake {
    track: TrackId,
    capture: CaptureId,
    stats: Arc<riddim_disk::capture::CaptureStats>,
    path: PathBuf,
    channels: u16,
}

struct RecordingSession {
    /// Timeline sample where the takes begin. Known once the audio thread has started.
    start: i64,
    tracks: Vec<TrackId>,
    /// Output-side latency (device plus plan) in force when recording started.
    output_latency: i64,
    audio: Vec<AudioTake>,
    midi_tracks: Vec<TrackId>,
    /// Notes played live: (track, frames rolled since the take's start, what).
    midi_events: Vec<(TrackId, i64, EventBody)>,
    /// Loop wraps since the take began: where the playhead left, and where it landed.
    wraps: Vec<(i64, i64)>,
    /// The punch range in samples, for a punch recording.
    punch: Option<(i64, i64)>,
}

/// One time through the recorded stretch. A recording without a loop has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pass {
    /// Where on the timeline this pass began.
    timeline_start: i64,
    /// The frames of the take it covers, counted from the take's start through every wrap.
    rolled_start: i64,
    rolled_end: i64,
}

/// Splits a recording into its passes: one per time round the loop.
fn passes_of(start: i64, wraps: &[(i64, i64)], rolled_total: i64) -> Vec<Pass> {
    let mut passes = Vec::new();
    let (mut at, mut rolled) = (start, 0i64);
    for &(left, landed) in wraps {
        let length = left - at;
        if length > 0 {
            passes.push(Pass {
                timeline_start: at,
                rolled_start: rolled,
                rolled_end: rolled + length,
            });
            rolled += length;
        }
        at = landed;
    }
    if rolled_total > rolled {
        passes.push(Pass {
            timeline_start: at,
            rolled_start: rolled,
            rolled_end: rolled_total,
        });
    }
    passes
}

/// Which take should be the one that is heard: the newest that is not a fragment. (Stopping
/// usually cuts the last time round short, and a few beats of it are not what anyone wants
/// to hear in place of a whole take.)
fn audible_take(lengths: &[i64]) -> usize {
    let longest = lengths.iter().copied().max().unwrap_or(0);
    lengths
        .iter()
        .rposition(|&length| length * 2 >= longest)
        .unwrap_or(0)
}

/// One automation lane being written: the control is held (touch), was touched (latch), or the
/// transport is running over a lane in write mode.
struct Writing {
    track: TrackId,
    lane: riddim_model::LaneId,
    target: AutomationTarget,
    start: Ticks,
    points: Vec<riddim_model::AutomationPoint>,
}

struct PendingSave {
    waiting: HashSet<SlotId>,
    states: Vec<(TrackId, SlotId, Vec<u8>)>,
    reply: Sender<Result<Applied, String>>,
}

type DelayKey = (NodeKind, usize, NodeKind, usize);

/// What a source node plays, cached per track so an unrelated edit does not rebuild it.
struct SourceCache {
    track: Arc<Track>,
    map: Arc<TempoMap>,
    audio: Option<Arc<Vec<RtAudioClip>>>,
    midi: Option<Arc<Vec<TimedEvent>>>,
}

pub(crate) struct ControlSetup {
    pub inbox: Receiver<ControlMsg>,
    pub to_audio: Producer<ToAudio>,
    pub from_audio: Consumer<FromAudio>,
    pub collector: GarbageCollector,
    pub host_requests: Sender<HostRequest>,
    pub host_replies: Receiver<HostReply>,
    pub shared: Arc<Shared>,
    pub snapshot: Arc<Mutex<Arc<Model>>>,
    pub stream: StreamInfo,
    pub max_block: usize,
    pub capture_dir: PathBuf,
    pub media_dir: PathBuf,
    pub butler: Butler,
    pub process_mode: ProcessMode,
}

pub(crate) struct Control {
    inbox: Receiver<ControlMsg>,
    to_audio: Producer<ToAudio>,
    from_audio: Consumer<FromAudio>,
    collector: GarbageCollector,
    host_requests: Sender<HostRequest>,
    host_replies: Receiver<HostReply>,
    shared: Arc<Shared>,
    snapshot: Arc<Mutex<Arc<Model>>>,
    stream: StreamInfo,
    rate: u32,
    max_block: usize,
    capture_dir: PathBuf,
    butler: Butler,
    process_mode: ProcessMode,

    model: Model,
    revision: u64,
    map: Arc<TempoMap>,
    subscribers: Vec<Sender<Event>>,

    cells: HashMap<NodeKind, Arc<NodeCell>>,
    delays: HashMap<DelayKey, Arc<DelayCell>>,
    /// Plugin cells that have left the graph and wait for the audio thread to let go of them.
    retired: Vec<(u64, Arc<NodeCell>)>,
    next_serial: u64,
    /// Plugins taken out of the plan to be activated again with other buses or another block
    /// size, waiting for the audio thread to let go of them.
    restarting: Vec<(SlotId, u64, Arc<NodeCell>)>,
    plugins: HashMap<SlotId, PluginEntry>,
    meters: HashMap<TrackId, Arc<MeterCell>>,
    media: MediaCache,
    sources: HashMap<TrackId, SourceCache>,

    plan_serial: u64,
    /// (audio, scratch, event) buffer counts the audio thread currently has.
    installed: (usize, usize, usize),
    /// A plan that did not fit in the queue yet. Newer plans replace it.
    unsent: Option<ToAudio>,
    /// Other messages that did not fit in the queue yet, in order.
    backlog: Vec<ToAudio>,

    recording: Option<RecordingSession>,
    /// Automation being written right now.
    writes: Vec<Writing>,
    /// Stands in for the playhead while a jump is being accounted for (see `playhead_jumped`).
    write_clock: Option<Ticks>,
    take_counter: u32,
    pending_save: Option<PendingSave>,
    pending_syncs: Vec<Sender<()>>,
    last_state: TransportState,
    last_report: Instant,
}

/// What mutes and solos leave to be heard.
#[derive(Default)]
struct Audibility {
    /// Tracks whose strip is silenced.
    muted_strips: HashSet<TrackId>,
    /// Tracks whose own output is closed while a post-fader send from them stays open.
    closed_outputs: HashSet<TrackId>,
    /// Sends closed by a solo elsewhere.
    closed_sends: HashSet<(TrackId, riddim_model::SendId)>,
}

/// Works out what is heard, given mutes and solos.
///
/// With no solo anywhere, everything unmuted is heard. With one, solo follows the signal: a
/// path from a track to the master is heard if it passes through a soloed track. So soloing a
/// track keeps the buses it plays through and the returns it sends to; soloing a bus keeps
/// what feeds it, but only on the way into that bus (soloing an effect return gives the
/// effect alone, not the dry tracks that send to it).
///
/// This is decided per connection: one from `T` into `X` is open if `T` carries soloed signal
/// (it is soloed, or something soloed plays into it) or `X` leads to a soloed track. One case
/// is approximate: a bus that both carries a soloed track's signal and feeds a soloed bus
/// lets its other, unsoloed inputs through to its other destinations as well.
fn audibility(model: &Model) -> Audibility {
    let mut result = Audibility::default();
    result
        .muted_strips
        .extend(model.tracks.iter().filter(|t| t.mute).map(|t| t.id));
    if model.master.mute {
        result.muted_strips.insert(TrackId::MASTER);
    }
    if !model.tracks.iter().any(|t| t.solo) {
        return result;
    }

    // Where a track's signal goes next: its output, then its (unmuted) sends.
    let targets = |track: &Track| -> Vec<TrackId> {
        let main = match track.output {
            TrackOutput::Bus(bus) => bus,
            TrackOutput::Master => TrackId::MASTER,
        };
        std::iter::once(main)
            .chain(track.sends.iter().filter(|s| !s.mute).map(|s| s.target))
            .collect()
    };
    // Tracks carrying soloed signal: the soloed ones and everything downstream of them.
    let mut carries: HashSet<TrackId> = HashSet::new();
    let mut pending: Vec<TrackId> = model
        .tracks
        .iter()
        .filter(|t| t.solo)
        .map(|t| t.id)
        .collect();
    while let Some(id) = pending.pop() {
        if carries.insert(id) {
            if let Some(track) = model.track(id) {
                pending.extend(targets(track));
            }
        }
    }
    // Tracks leading to a soloed one: the soloed ones and everything upstream of them. The
    // routing has no cycles, so repeating until nothing changes ends after at most one pass
    // per track.
    let mut leads: HashSet<TrackId> = model
        .tracks
        .iter()
        .filter(|t| t.solo)
        .map(|t| t.id)
        .collect();
    loop {
        let before = leads.len();
        for track in &model.tracks {
            if targets(track).iter().any(|target| leads.contains(target)) {
                leads.insert(track.id);
            }
        }
        if leads.len() == before {
            break;
        }
    }

    for track in &model.tracks {
        let open = |target: TrackId| carries.contains(&track.id) || leads.contains(&target);
        let main_open = open(match track.output {
            TrackOutput::Bus(bus) => bus,
            TrackOutput::Master => TrackId::MASTER,
        });
        let mut post_fader_open = false;
        for send in track.sends.iter().filter(|s| !s.mute) {
            if !open(send.target) {
                result.closed_sends.insert((track.id, send.id));
            } else if !send.pre_fader {
                post_fader_open = true;
            }
        }
        if !main_open {
            if post_fader_open {
                // The strip has to keep running for the send; shut the output after it.
                result.closed_outputs.insert(track.id);
            } else {
                result.muted_strips.insert(track.id);
            }
        }
    }
    result
}

impl Control {
    pub fn new(setup: ControlSetup) -> Control {
        let rate = setup.stream.sample_rate as u32;
        let model = Model::new();
        Control {
            inbox: setup.inbox,
            to_audio: setup.to_audio,
            from_audio: setup.from_audio,
            collector: setup.collector,
            host_requests: setup.host_requests,
            host_replies: setup.host_replies,
            shared: setup.shared,
            snapshot: setup.snapshot,
            rate,
            max_block: setup.max_block,
            capture_dir: setup.capture_dir,
            butler: setup.butler,
            process_mode: setup.process_mode,
            map: Arc::new(model.tempo_map.clone()),
            model,
            revision: 0,
            subscribers: Vec::new(),
            cells: HashMap::new(),
            delays: HashMap::new(),
            retired: Vec::new(),
            next_serial: 1,
            restarting: Vec::new(),
            plugins: HashMap::new(),
            meters: HashMap::new(),
            media: MediaCache::new(setup.media_dir, rate),
            sources: HashMap::new(),
            plan_serial: 0,
            installed: (0, 0, 0),
            unsent: None,
            backlog: Vec::new(),
            recording: None,
            writes: Vec::new(),
            take_counter: 0,
            pending_save: None,
            pending_syncs: Vec::new(),
            write_clock: None,
            last_state: TransportState::Stopped,
            last_report: Instant::now(),
            stream: setup.stream,
        }
    }

    /// The control thread's main loop. Returns when the engine shuts down.
    pub fn run(mut self) {
        self.rebuild_plan();
        loop {
            match self.inbox.recv_timeout(Duration::from_millis(2)) {
                Ok(message) => {
                    if !self.handle(message) {
                        break;
                    }
                    // Deal with whatever else is already waiting before publishing.
                    let mut alive = true;
                    while let Ok(message) = self.inbox.try_recv() {
                        alive = self.handle(message);
                        if !alive {
                            break;
                        }
                    }
                    if !alive {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.flush_to_audio();
            self.poll_audio();
            self.poll_host();
            self.collect();
            self.report();
            self.answer_syncs();
        }
        let _ = self.host_requests.send(HostRequest::Shutdown);
    }

    fn emit(&mut self, event: Event) {
        self.subscribers
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }

    fn send(&mut self, message: ToAudio) {
        if !self.backlog.is_empty() {
            self.backlog.push(message);
            return;
        }
        if let Err(PushError::Full(message)) = self.to_audio.push(message) {
            self.backlog.push(message);
        }
    }

    /// Retries whatever did not fit in the audio thread's queue.
    fn flush_to_audio(&mut self) {
        if let Some(plan) = self.unsent.take() {
            if let Err(PushError::Full(plan)) = self.to_audio.push(plan) {
                self.unsent = Some(plan);
                return;
            }
        }
        let pending = std::mem::take(&mut self.backlog);
        let mut pending = pending.into_iter();
        while let Some(message) = pending.next() {
            if let Err(PushError::Full(message)) = self.to_audio.push(message) {
                self.backlog.push(message);
                self.backlog.extend(pending);
                break;
            }
        }
    }

    fn handle(&mut self, message: ControlMsg) -> bool {
        match message {
            ControlMsg::Apply { ticket, ops, reply } => {
                let outcome = self.apply(&ops);
                if outcome.is_ok() {
                    // Moving a fader is an op; if its lane is being written, it is also a
                    // point on that lane.
                    for op in &ops {
                        match op {
                            Op::SetTrack {
                                id,
                                change: riddim_model::TrackChange::Gain(gain),
                            } => {
                                self.automation_value(*id, AutomationTarget::TrackGain, *gain);
                            }
                            Op::SetTrack {
                                id,
                                change: riddim_model::TrackChange::Pan(pan),
                            } => {
                                self.automation_value(*id, AutomationTarget::TrackPan, *pan);
                            }
                            Op::SetSend { track, send } => {
                                let target = AutomationTarget::SendGain { send: send.id };
                                self.automation_value(*track, target, send.gain);
                            }
                            _ => {}
                        }
                    }
                }
                match &outcome {
                    Ok(applied) => self.emit(Event::Applied {
                        ticket,
                        revision: applied.revision,
                        inverses: applied.inverses.clone(),
                    }),
                    Err(rejected) => self.emit(Event::Rejected {
                        ticket,
                        index: rejected.index,
                        error: rejected.error.to_string(),
                    }),
                }
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                }
            }
            ControlMsg::Transport(command) => self.transport(command),
            ControlMsg::SetParam { slot, id, value } => {
                self.send(ToAudio::Param { slot, id, value });
                let _ = self
                    .host_requests
                    .send(HostRequest::NotifyParam { slot, id, value });
                if let Some(track) = self.plugins.get(&slot).map(|entry| entry.track) {
                    let target = AutomationTarget::PluginParam { slot, param: id };
                    self.automation_value(track, target, value as f32);
                }
            }
            ControlMsg::Touch {
                track,
                target,
                down,
            } => self.touch(track, target, down),
            ControlMsg::PlayNote { track, body } => self.send(ToAudio::Midi { track, body }),
            ControlMsg::Subscribe(subscriber) => self.subscribers.push(subscriber),
            ControlMsg::Sync(reply) => self.pending_syncs.push(reply),
            ControlMsg::Load { model, reply } => {
                let _ = reply.send(self.load(*model));
            }
            ControlMsg::CapturePluginStates(reply) => self.capture_plugin_states(reply),
            ControlMsg::StreamChanged { info, max_block } => {
                self.stream = info;
                self.max_block = max_block;
                // New buffers of the new size go to the audio thread with the next plan, and
                // every plugin is restarted to be told the new maximum block.
                self.installed = (0, 0, 0);
                self.sync_plugins();
                self.rebuild_plan();
                self.emit(Event::StreamChanged {
                    sample_rate: self.stream.sample_rate,
                    buffer_frames: self.stream.max_frames,
                    input_latency_frames: self.stream.input_latency_frames,
                    output_latency_frames: self.stream.output_latency_frames,
                });
            }
            ControlMsg::Shutdown => return false,
        }
        true
    }

    // -----------------------------------------------------------------------------------------
    // Model
    // -----------------------------------------------------------------------------------------

    fn apply(&mut self, ops: &[Op]) -> Result<Applied, Rejected> {
        let inverses = self
            .model
            .apply_all(ops)
            .map_err(|(index, error)| Rejected { index, error })?;
        self.revision += 1;
        self.shared
            .model_revision
            .store(self.revision, Ordering::Release);
        *self.snapshot.lock().unwrap() = Arc::new(self.model.clone());
        if *self.map != self.model.tempo_map {
            self.map = Arc::new(self.model.tempo_map.clone());
        }
        self.sync_plugins();
        self.rebuild_plan();
        Ok(Applied {
            revision: self.revision,
            inverses,
        })
    }

    /// Replaces the model wholesale. Not an op: there is nothing to undo back to.
    fn load(&mut self, model: Model) -> Result<u64, String> {
        if self.recording.is_some() {
            return Err("cannot open a project while recording".into());
        }
        model.validate()?;
        self.model = model;
        self.revision += 1;
        self.shared
            .model_revision
            .store(self.revision, Ordering::Release);
        *self.snapshot.lock().unwrap() = Arc::new(self.model.clone());
        self.map = Arc::new(self.model.tempo_map.clone());
        // Every plugin of the old project goes, even one whose slot id the new project reuses:
        // the new slot may hold a different plugin or different state.
        let stale: Vec<SlotId> = self.plugins.keys().copied().collect();
        for slot in stale {
            let entry = self.plugins.remove(&slot).expect("just listed");
            // (One with no cell is still loading or being restarted: it is let go of when it
            // reports back and turns out not to be wanted.)
            if let Some(cell) = self.cells.remove(&NodeKind::Plugin(entry.track, slot)) {
                self.retired.push((entry.serial, cell));
            }
        }
        self.sync_plugins();
        self.rebuild_plan();
        Ok(self.revision)
    }

    /// Applies ops the engine itself decided on, and tells the client as if it had made them.
    fn apply_internal(&mut self, ops: Vec<Op>) -> bool {
        match self.apply(&ops) {
            Ok(applied) => {
                self.emit(Event::ModelChanged {
                    revision: applied.revision,
                    ops,
                    inverses: applied.inverses,
                });
                true
            }
            Err(rejected) => {
                self.emit(Event::Error {
                    message: format!("the engine could not update the model: {}", rejected.error),
                });
                false
            }
        }
    }

    /// Starts loading plugins that appeared in the model and retires ones that left it.
    fn sync_plugins(&mut self) {
        let mut wanted: HashMap<SlotId, TrackId> = HashMap::new();
        for track in self.model.all_tracks() {
            for slot in &track.plugins {
                wanted.insert(slot.id, track.id);
            }
        }
        let gone: Vec<SlotId> = self
            .plugins
            .keys()
            .filter(|slot| !wanted.contains_key(slot))
            .copied()
            .collect();
        for slot in gone {
            let entry = self.plugins.remove(&slot).expect("just listed");
            // The audio thread may still be running a plan that uses the processor; it is
            // unloaded once the cell is ours alone. (One with no cell is still loading or
            // being restarted: it is let go of when it reports back and is not wanted.)
            if let Some(cell) = self.cells.remove(&NodeKind::Plugin(entry.track, slot)) {
                self.retired.push((entry.serial, cell));
            }
        }
        let config = ActivateConfig {
            sample_rate: self.rate as f64,
            max_block: self.max_block as u32,
            mode: self.process_mode,
        };
        for track in self.model.all_tracks() {
            for slot in &track.plugins {
                let buses = wanted_buses(slot);
                if let Some(entry) = self.plugins.get_mut(&slot.id) {
                    // Loaded, but for other buses or another block size than are wanted now:
                    // take it out of the plan and have it activated again. (One that is still
                    // loading is looked at again when it arrives.)
                    let stale = entry.buses != buses || entry.max_block != config.max_block;
                    if stale && matches!(entry.status, PluginStatus::Loaded(_)) {
                        entry.status = PluginStatus::Loading;
                        if let Some(cell) =
                            self.cells.remove(&NodeKind::Plugin(entry.track, slot.id))
                        {
                            self.restarting.push((slot.id, entry.serial, cell));
                        }
                    }
                    continue;
                }
                let serial = self.next_serial;
                self.next_serial += 1;
                self.plugins.insert(
                    slot.id,
                    PluginEntry {
                        track: track.id,
                        status: PluginStatus::Loading,
                        serial,
                        buses,
                        max_block: config.max_block,
                    },
                );
                let _ = self.host_requests.send(HostRequest::Load {
                    slot: slot.id,
                    serial,
                    plugin: slot.plugin.clone(),
                    state: slot.state.clone(),
                    config,
                    buses,
                });
            }
        }
    }

    /// What a slot should be running with right now, if it is still in the model.
    fn wanted_config(&self, slot: SlotId) -> Option<(ActivateConfig, BusRequest)> {
        let config = ActivateConfig {
            sample_rate: self.rate as f64,
            max_block: self.max_block as u32,
            mode: self.process_mode,
        };
        self.model
            .all_tracks()
            .flat_map(|t| t.plugins.iter())
            .find(|s| s.id == slot)
            .map(|s| (config, wanted_buses(s)))
    }

    fn capture_plugin_states(&mut self, reply: Sender<Result<Applied, String>>) {
        if self.pending_save.is_some() {
            let _ = reply.send(Err("a state capture is already in progress".into()));
            return;
        }
        let waiting: HashSet<SlotId> = self
            .plugins
            .iter()
            .filter(|(_, entry)| matches!(entry.status, PluginStatus::Loaded(_)))
            .map(|(slot, _)| *slot)
            .collect();
        for slot in &waiting {
            let _ = self
                .host_requests
                .send(HostRequest::SaveState { slot: *slot });
        }
        self.pending_save = Some(PendingSave {
            waiting,
            states: Vec::new(),
            reply,
        });
        self.finish_save_if_ready();
    }

    fn finish_save_if_ready(&mut self) {
        if !self
            .pending_save
            .as_ref()
            .is_some_and(|save| save.waiting.is_empty())
        {
            return;
        }
        let save = self.pending_save.take().expect("checked above");
        let ops: Vec<Op> = save
            .states
            .into_iter()
            .map(|(track, slot, bytes)| Op::SetPluginState {
                track,
                slot,
                state: Some(Arc::new(bytes)),
            })
            .collect();
        let outcome = match self.apply(&ops) {
            Ok(applied) => {
                self.emit(Event::ModelChanged {
                    revision: applied.revision,
                    ops,
                    inverses: applied.inverses.clone(),
                });
                Ok(applied)
            }
            Err(rejected) => Err(rejected.error.to_string()),
        };
        let _ = save.reply.send(outcome);
    }

    // -----------------------------------------------------------------------------------------
    // Plan building
    // -----------------------------------------------------------------------------------------

    fn audio_clips(&mut self, track: &Track) -> Arc<Vec<RtAudioClip>> {
        let mut clips = Vec::new();
        let mut missing = Vec::new();
        for clip in track.clips.iter().filter(|c| !c.muted) {
            let ClipContent::Audio {
                media,
                offset,
                length,
                gain,
                fade_in,
                fade_out,
            } = &clip.content
            else {
                continue;
            };
            let Some(media) = self.model.media(*media).cloned() else {
                continue;
            };
            let (loaded, first_time) = self.media.get(&media);
            let audio = match loaded {
                Ok(audio) => audio,
                Err(error) => {
                    if first_time {
                        missing.push(Event::MediaMissing {
                            media: media.id,
                            error,
                        });
                    }
                    continue;
                }
            };
            let to_session = |frames: u64| self.media.to_session_frames(frames, media.sample_rate);
            let offset = to_session(*offset);
            let length = to_session(*length)
                .min(audio.frames() as i64 - offset)
                .max(0);
            clips.push(RtAudioClip {
                start: self.map.position_to_samples(clip.start, self.rate),
                length,
                offset,
                gain: *gain,
                fade_in: to_session(*fade_in).min(length),
                fade_out: to_session(*fade_out).min(length),
                media: audio,
            });
        }
        for event in missing {
            self.emit(event);
        }
        Arc::new(clips)
    }

    fn midi_events(&self, track: &Track) -> Arc<Vec<TimedEvent>> {
        let mut events: Vec<(i64, u8, EventBody)> = Vec::new();
        for clip in track.clips.iter().filter(|c| !c.muted) {
            let ClipContent::Midi {
                length,
                notes,
                events: others,
            } = &clip.content
            else {
                continue;
            };
            let clip_start = self.map.position_to_ticks(clip.start);
            let clip_end = clip_start + *length;
            let sample = |tick: Ticks| self.map.ticks_to_samples(tick, self.rate);
            for note in notes.iter() {
                let start = clip_start + note.start;
                if start >= clip_end {
                    continue;
                }
                // Notes are cut at the end of their clip.
                let end = (start + note.length).min(clip_end);
                let on = sample(start);
                let off = sample(end).max(on + 1);
                let velocity = note.velocity as f32 / 127.0;
                events.push((
                    on,
                    1,
                    EventBody::NoteOn {
                        channel: note.channel,
                        key: note.key,
                        velocity,
                        note_id: -1,
                    },
                ));
                events.push((
                    off,
                    0,
                    EventBody::NoteOff {
                        channel: note.channel,
                        key: note.key,
                        velocity: 0.0,
                        note_id: -1,
                    },
                ));
            }
            for other in others.iter() {
                let at = clip_start + other.at;
                if at < clip_end {
                    events.push((
                        sample(at),
                        1,
                        EventBody::Midi {
                            data: other.data,
                            len: other.len,
                        },
                    ));
                }
            }
        }
        // Releases before attacks at the same sample, so a repeated note retriggers cleanly.
        events.sort_by_key(|(sample, rank, _)| (*sample, *rank));
        Arc::new(
            events
                .into_iter()
                .map(|(sample, _, body)| TimedEvent { sample, body })
                .collect(),
        )
    }

    fn source_params(&mut self, track: &Arc<Track>) -> StepParams {
        let fresh = self.sources.get(&track.id).is_some_and(|cache| {
            Arc::ptr_eq(&cache.track, track) && Arc::ptr_eq(&cache.map, &self.map)
        });
        if !fresh {
            let (audio, midi) = match track.kind {
                TrackKind::Audio => (Some(self.audio_clips(track)), None),
                _ => (None, Some(self.midi_events(track))),
            };
            self.sources.insert(
                track.id,
                SourceCache {
                    track: track.clone(),
                    map: self.map.clone(),
                    audio,
                    midi,
                },
            );
        }
        let cache = &self.sources[&track.id];
        match track.kind {
            TrackKind::Audio => StepParams::AudioSource {
                clips: cache.audio.clone().expect("audio tracks cache audio clips"),
                monitor_input: match (track.monitor, track.input) {
                    (
                        true,
                        TrackInput::Audio {
                            first_channel,
                            channels,
                        },
                    ) => Some((first_channel as usize, channels as usize)),
                    _ => None,
                },
            },
            _ => StepParams::MidiSource {
                track: track.id,
                events: cache
                    .midi
                    .clone()
                    .expect("instrument tracks cache MIDI events"),
                live_input: track.input == TrackInput::Midi
                    && (track.monitor || track.record_armed),
                record: track.record_armed,
            },
        }
    }

    /// A lane as the audio thread reads it, or `None` if the lane is off, empty, or currently
    /// being written (in which case the control's live value applies instead).
    fn curve(&self, track: TrackId, lane: &AutomationLane) -> Option<Arc<RtCurve>> {
        if lane.mode == AutomationMode::Off || lane.points.is_empty() {
            return None;
        }
        if self
            .writes
            .iter()
            .any(|w| w.track == track && w.lane == lane.id)
        {
            return None;
        }
        let points = lane
            .points
            .iter()
            .map(|p| RtPoint {
                sample: self.map.ticks_to_samples(p.at, self.rate),
                value: p.value,
                hold: p.shape == CurveShape::Hold,
            })
            .collect();
        Some(Arc::new(RtCurve { points }))
    }

    fn new_cell(&mut self, kind: NodeKind) -> Option<Arc<NodeCell>> {
        let rt = match kind {
            NodeKind::Source(track) => match self.model.track(track).map(|t| t.kind) {
                Some(TrackKind::Instrument) => NodeRt::MidiSource(MidiSourceRt::new()),
                _ => NodeRt::Stateless,
            },
            NodeKind::BusInput(_) | NodeKind::Output => NodeRt::Stateless,
            NodeKind::Strip(track) => NodeRt::Strip(StripRt {
                // A 5 ms ramp: long enough not to click, short enough to feel immediate.
                gain: StereoGain::new(self.rate / 200),
                primed: false,
                meter: self.meters.entry(track).or_default().clone(),
            }),
            NodeKind::Send(..) | NodeKind::MainOut(..) => NodeRt::Gain(GainRt {
                gain: StereoGain::new(self.rate / 200),
                primed: false,
            }),
            NodeKind::Metronome => NodeRt::Metronome(Click::new()),
            // Plugin cells are created when the plugin finishes loading, never here.
            NodeKind::Plugin(..) => return None,
        };
        Some(NodeCell::new(rt))
    }

    /// Compiles the current model into a plan and sends it to the audio thread.
    fn rebuild_plan(&mut self) {
        let ports: HashMap<SlotId, PluginPorts> = self
            .plugins
            .iter()
            .filter_map(|(slot, entry)| match &entry.status {
                PluginStatus::Loaded(ports) => Some((*slot, ports.clone())),
                _ => None,
            })
            .collect();
        let graph = model_graph::build(&self.model, &ports);
        let plan = match compile(&graph.spec) {
            Ok(plan) => plan,
            Err(error) => {
                self.emit(Event::Error {
                    message: format!("the routing could not be compiled: {error}"),
                });
                return;
            }
        };
        let heard = audibility(&self.model);
        let model = self.model.clone();

        let mut steps = Vec::with_capacity(plan.steps.len());
        let mut scratch_count = 0;
        let mut event_buffer_of: Vec<Option<usize>> = Vec::with_capacity(plan.steps.len());
        let mut event_buffer_count = 0;
        let mut live_kinds: HashSet<NodeKind> = HashSet::new();
        let mut live_delays: HashSet<DelayKey> = HashSet::new();
        let mut output_step = 0;
        let mut plugin_steps = Vec::new();

        for (position, step) in plan.steps.iter().enumerate() {
            let kind = graph.kinds[step.node];
            live_kinds.insert(kind);
            let cell = match self.cells.get(&kind) {
                Some(cell) => cell.clone(),
                None => {
                    let Some(cell) = self.new_cell(kind) else {
                        // A plugin node without a processor cannot be in the graph: the ports
                        // table above only lists loaded plugins.
                        self.emit(Event::Error {
                            message: format!("no processor for {kind:?}"),
                        });
                        return;
                    };
                    self.cells.insert(kind, cell.clone());
                    cell
                }
            };

            let params = match kind {
                NodeKind::Source(track) => match model.track(track) {
                    Some(track) => self.source_params(track),
                    None => StepParams::BusInput,
                },
                NodeKind::BusInput(_) => StepParams::BusInput,
                NodeKind::Plugin(track, slot) => {
                    plugin_steps.push((slot, position));
                    let bypass = model
                        .track(track)
                        .and_then(|t| t.plugin(slot))
                        .is_some_and(|s| s.bypass);
                    let curves = model.track(track).map_or_else(Vec::new, |t| {
                        t.automation
                            .iter()
                            .filter_map(|lane| match lane.target {
                                AutomationTarget::PluginParam { slot: s, param } if s == slot => {
                                    self.curve(track, lane).map(|curve| (param, curve))
                                }
                                _ => None,
                            })
                            .collect()
                    });
                    StepParams::Plugin {
                        bypass,
                        curves: Arc::new(curves),
                    }
                }
                NodeKind::Strip(track) => {
                    let strip = model.track(track);
                    let curve_for = |target| {
                        strip
                            .and_then(|t| t.lane_for(target))
                            .and_then(|lane| self.curve(track, lane))
                    };
                    StepParams::Strip {
                        gain: strip.map_or(1.0, |t| t.gain),
                        pan: strip.map_or(0.0, |t| t.pan),
                        mute: heard.muted_strips.contains(&track),
                        invert: strip.is_some_and(|t| t.invert),
                        gain_curve: curve_for(AutomationTarget::TrackGain),
                        pan_curve: curve_for(AutomationTarget::TrackPan),
                    }
                }
                NodeKind::Send(track, send) => {
                    let owner = model.track(track);
                    let settings = owner.and_then(|t| t.send(send));
                    StepParams::Send {
                        gain: settings.map_or(0.0, |s| s.gain),
                        mute: settings.is_none_or(|s| s.mute)
                            || heard.closed_sends.contains(&(track, send)),
                        curve: owner
                            .and_then(|t| t.lane_for(AutomationTarget::SendGain { send }))
                            .and_then(|lane| self.curve(track, lane)),
                    }
                }
                NodeKind::MainOut(track) => StepParams::Send {
                    gain: 1.0,
                    mute: heard.closed_outputs.contains(&track),
                    curve: None,
                },
                NodeKind::Metronome => StepParams::Metronome {
                    enabled: model.metronome.enabled,
                    gain: model.metronome.gain,
                },
                NodeKind::Output => {
                    output_step = position;
                    StepParams::Output
                }
            };

            let first_scratch = scratch_count;
            let mut inputs = Vec::with_capacity(step.inputs.len());
            for input in &step.inputs {
                let mut sources = Vec::with_capacity(input.sources.len());
                for source in &input.sources {
                    let delay = (source.delay > 0).then(|| {
                        let key = (
                            graph.kinds[source.edge.from],
                            source.edge.from_port,
                            kind,
                            source.edge.to_port,
                        );
                        live_delays.insert(key);
                        let channels = input.channels as usize;
                        let reusable = self
                            .delays
                            .get(&key)
                            .filter(|cell| {
                                cell.samples == source.delay && cell.channels == channels
                            })
                            .cloned();
                        reusable.unwrap_or_else(|| {
                            let cell = DelayCell::new(channels, source.delay);
                            self.delays.insert(key, cell.clone());
                            cell
                        })
                    });
                    sources.push(RtSource {
                        first_buffer: source.first_buffer,
                        channels: source.channels as usize,
                        delay,
                    });
                }
                inputs.push(RtInput {
                    first_scratch: scratch_count,
                    channels: input.channels as usize,
                    sources,
                });
                scratch_count += input.channels as usize;
            }

            let event_output = graph.spec.nodes[step.node].event_output.then(|| {
                event_buffer_count += 1;
                event_buffer_count - 1
            });
            event_buffer_of.push(event_output);
            let event_sources = step
                .event_sources
                .iter()
                .filter_map(|&source| event_buffer_of[source])
                .collect();

            steps.push(RtStep {
                cell,
                params,
                inputs,
                first_scratch,
                input_latency: step.input_latency as i64,
                in_channels: scratch_count - first_scratch,
                first_buffer: step.outputs.first().map_or(0, |o| o.first_buffer),
                out_channels: step.outputs.iter().map(|o| o.channels as usize).sum(),
                event_sources,
                event_output,
                dependencies: step.dependencies,
                dependents: step.dependents.clone(),
            });
        }

        // Forget state for nodes and delays that are no longer part of the graph. (Plugin
        // cells are retired separately, in `sync_plugins`, because they need unloading.)
        self.cells
            .retain(|kind, _| live_kinds.contains(kind) || matches!(kind, NodeKind::Plugin(..)));
        self.delays.retain(|key, _| live_delays.contains(key));
        self.meters.retain(|track, _| model.track(*track).is_some());
        self.sources
            .retain(|track, _| model.track(*track).is_some());
        self.media.retain(|id| model.media(id).is_some());

        plugin_steps.sort_by_key(|(slot, _)| *slot);
        let loop_range = match (model.loop_enabled, model.loop_range) {
            (true, Some(range)) => {
                let start = self.map.position_to_samples(range.start, self.rate);
                let end = self.map.position_to_samples(range.end, self.rate);
                (end > start).then_some((start, end))
            }
            _ => None,
        };
        self.plan_serial += 1;
        let schedule = crate::exec::Schedule::new(steps.len());
        let rt_plan = Box::new(RtPlan {
            schedule,
            steps,
            output_step,
            plugins: plugin_steps,
            buffer_count: plan.buffer_count,
            scratch_count,
            event_buffer_count,
            tempo_map: self.map.clone(),
            loop_range,
            latency: plan.max_latency,
            serial: self.plan_serial,
        });

        let needed = (
            rt_plan.buffer_count,
            rt_plan.scratch_count,
            rt_plan.event_buffer_count,
        );
        let mut buffers = None;
        if needed.0 > self.installed.0 || needed.1 > self.installed.1 || needed.2 > self.installed.2
        {
            // Grow with headroom so adding one track does not reallocate every time.
            let grow = |need: usize, have: usize| {
                if need > have {
                    need + need / 2 + 8
                } else {
                    have
                }
            };
            self.installed = (
                grow(needed.0, self.installed.0),
                grow(needed.1, self.installed.1),
                grow(needed.2, self.installed.2),
            );
            buffers = Some(Box::new(Buffers::new(
                self.installed.0,
                self.installed.1,
                self.installed.2,
                self.max_block,
            )));
        }

        // If an earlier plan is still waiting for room in the queue, this one supersedes it. Its
        // buffers must not be lost, though: keep whichever set is larger (the newer).
        let message = match (self.unsent.take(), buffers) {
            (Some(ToAudio::SwapPlan { buffers: old, .. }), None) => ToAudio::SwapPlan {
                plan: rt_plan,
                buffers: old,
            },
            (_, buffers) => ToAudio::SwapPlan {
                plan: rt_plan,
                buffers,
            },
        };
        if let Err(PushError::Full(message)) = self.to_audio.push(message) {
            self.unsent = Some(message);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Transport and recording
    // -----------------------------------------------------------------------------------------

    // -----------------------------------------------------------------------------------------
    // Writing automation
    // -----------------------------------------------------------------------------------------

    fn now_ticks(&self) -> Ticks {
        if let Some(at) = self.write_clock {
            return at;
        }
        self.map
            .samples_to_ticks(self.shared.position.load(Ordering::Acquire), self.rate)
    }

    /// The control's own (un-automated) value, where the model knows it.
    fn control_value(&self, track: TrackId, target: AutomationTarget) -> Option<f32> {
        let track = self.model.track(track)?;
        match target {
            AutomationTarget::TrackGain => Some(track.gain),
            AutomationTarget::TrackPan => Some(track.pan),
            AutomationTarget::SendGain { send } => track.send(send).map(|s| s.gain),
            // A plugin's parameter values live in the plugin.
            AutomationTarget::PluginParam { .. } => None,
        }
    }

    /// A control was grabbed (`down`) or let go.
    fn touch(&mut self, track: TrackId, target: AutomationTarget, down: bool) {
        let Some(lane) = self
            .model
            .track(track)
            .and_then(|t| t.lane_for(target))
            .cloned()
        else {
            return;
        };
        if down {
            let rolling = self.shared.playing.load(Ordering::Acquire);
            if rolling && matches!(lane.mode, AutomationMode::Touch | AutomationMode::Latch) {
                self.begin_write(track, &lane);
            }
        } else if lane.mode == AutomationMode::Touch {
            // Touch stops writing on release; latch carries on until the transport stops.
            self.end_write(track, lane.id);
        }
    }

    fn begin_write(&mut self, track: TrackId, lane: &AutomationLane) {
        if self
            .writes
            .iter()
            .any(|w| w.track == track && w.lane == lane.id)
        {
            return;
        }
        let start = self.now_ticks();
        let mut points = Vec::new();
        if let Some(value) = self.control_value(track, lane.target) {
            points.push(riddim_model::AutomationPoint {
                at: start,
                value,
                shape: CurveShape::Linear,
            });
        }
        self.writes.push(Writing {
            track,
            lane: lane.id,
            target: lane.target,
            start,
            points,
        });
        // From now on the control's live value applies, not the lane.
        self.rebuild_plan();
    }

    /// A control produced a value: record it if its lane is being written, and start writing
    /// if the lane latches on the first movement.
    fn automation_value(&mut self, track: TrackId, target: AutomationTarget, value: f32) {
        // Hear about any jump of the playhead first, so this value lands in the right pass.
        self.poll_audio();
        if !self.shared.playing.load(Ordering::Acquire) {
            return;
        }
        if !self
            .writes
            .iter()
            .any(|w| w.track == track && w.target == target)
        {
            let lane = self
                .model
                .track(track)
                .and_then(|t| t.lane_for(target))
                .cloned();
            match lane {
                Some(lane) if lane.mode == AutomationMode::Latch => self.begin_write(track, &lane),
                _ => return,
            }
        }
        let now = self.now_ticks();
        let (low, high) = target.range();
        let value = value.clamp(low, high);
        // A control that sat still and then moved was heard as a flat line and then the move,
        // not as a slow slide between the two: past this gap, hold the old value up to the
        // new one. Closer together, values are the samples of one gesture and are joined.
        let gesture_gap = self.rate as i64 / 10;
        let (map, rate) = (self.map.clone(), self.rate);
        if let Some(write) = self
            .writes
            .iter_mut()
            .find(|w| w.track == track && w.target == target)
        {
            if let Some(last) = write.points.last().copied() {
                let apart = map.ticks_to_samples(now, rate) - map.ticks_to_samples(last.at, rate);
                if apart > gesture_gap && last.value != value && now.0 - 1 > last.at.0 {
                    write.points.push(riddim_model::AutomationPoint {
                        at: Ticks(now.0 - 1),
                        value: last.value,
                        shape: CurveShape::Linear,
                    });
                }
            }
            // Points must move forwards in time; values arriving within one tick stack up.
            let at = match write.points.last() {
                Some(last) if now <= last.at => Ticks(last.at.0 + 1),
                _ => now,
            };
            write.points.push(riddim_model::AutomationPoint {
                at,
                value,
                shape: CurveShape::Linear,
            });
        }
    }

    /// Commits what was written to the lane as one undoable edit.
    fn end_write(&mut self, track: TrackId, lane: riddim_model::LaneId) {
        let Some(index) = self
            .writes
            .iter()
            .position(|w| w.track == track && w.lane == lane)
        else {
            return;
        };
        let write = self.writes.remove(index);
        let Some(old) = self.model.track(track).and_then(|t| t.lane(lane)).cloned() else {
            self.rebuild_plan();
            return;
        };
        if write.points.is_empty() {
            // Held but never moved, with no value of its own to record: nothing changes.
            self.rebuild_plan();
            return;
        }
        let end = write
            .points
            .last()
            .map_or(write.start, |p| p.at)
            .max(self.now_ticks());
        let point = |at: Ticks| {
            old.value_at(at).map(|value| riddim_model::AutomationPoint {
                at,
                value,
                shape: CurveShape::Linear,
            })
        };
        // What was there before the pass and after it stays; an anchor on each side keeps the
        // old curve's shape right up to the edges of what was written.
        let mut points: Vec<riddim_model::AutomationPoint> = old
            .points
            .iter()
            .copied()
            .filter(|p| p.at < write.start)
            .collect();
        if write.start > Ticks::ZERO {
            if let Some(anchor) = point(Ticks(write.start.0 - 1)) {
                if points.last().is_none_or(|p| p.at < anchor.at) {
                    points.push(anchor);
                }
            }
        }
        for recorded in write.points {
            // Leave out points that repeat the value on both sides of them.
            let flat = points.len() >= 2
                && points[points.len() - 1].value == recorded.value
                && points[points.len() - 2].value == recorded.value
                && points[points.len() - 1].at >= write.start;
            if flat {
                points.pop();
            }
            if points.last().is_none_or(|p| p.at < recorded.at) {
                points.push(recorded);
            }
        }
        let last_written = points.last().map_or(end, |p| p.at).max(end);
        if points.last().is_some_and(|p| p.at < last_written) {
            let value = points.last().expect("checked").value;
            points.push(riddim_model::AutomationPoint {
                at: last_written,
                value,
                shape: CurveShape::Linear,
            });
        }
        if let Some(anchor) = point(Ticks(last_written.0 + 1)) {
            if old.points.iter().any(|p| p.at > last_written) {
                points.push(anchor);
            }
        }
        points.extend(
            old.points
                .iter()
                .copied()
                .filter(|p| p.at > Ticks(last_written.0 + 1)),
        );

        let lane = AutomationLane {
            points: Arc::new(points),
            ..(*old).clone()
        };
        self.apply_internal(vec![Op::ReplaceLane { track, lane }]);
    }

    /// The playhead jumped while rolling (a loop wrapped, or a locate). Automation passes do
    /// not span a jump: what was being written ends where the playhead left, and the same
    /// lanes carry on as new passes from where it landed.
    fn playhead_jumped(&mut self, from: i64, to: i64) {
        let open: Vec<(TrackId, riddim_model::LaneId)> =
            self.writes.iter().map(|w| (w.track, w.lane)).collect();
        if open.is_empty() {
            return;
        }
        self.write_clock = Some(self.map.samples_to_ticks(from, self.rate));
        self.finish_all_writes();
        self.write_clock = Some(self.map.samples_to_ticks(to, self.rate));
        for (track, lane) in open {
            let lane = self.model.track(track).and_then(|t| t.lane(lane)).cloned();
            if let Some(lane) = lane {
                self.begin_write(track, &lane);
            }
        }
        self.write_clock = None;
    }

    /// The transport started: lanes in write mode record from here.
    fn start_write_lanes(&mut self) {
        let lanes: Vec<(TrackId, Arc<AutomationLane>)> = self
            .model
            .all_tracks()
            .flat_map(|t| t.automation.iter().map(|lane| (t.id, lane.clone())))
            .filter(|(_, lane)| lane.mode == AutomationMode::Write)
            .collect();
        for (track, lane) in lanes {
            self.begin_write(track, &lane);
        }
    }

    /// The transport stopped: everything being written is committed.
    fn finish_all_writes(&mut self) {
        let open: Vec<(TrackId, riddim_model::LaneId)> =
            self.writes.iter().map(|w| (w.track, w.lane)).collect();
        for (track, lane) in open {
            self.end_write(track, lane);
        }
    }

    fn transport(&mut self, command: TransportCommand) {
        match command {
            TransportCommand::Play => self.send(ToAudio::Play),
            TransportCommand::Stop => self.send(ToAudio::Stop),
            TransportCommand::Locate(position) => {
                if self.recording.is_some() {
                    self.emit(Event::Error {
                        message: "the transport cannot be moved while recording".into(),
                    });
                    return;
                }
                let sample = self.map.position_to_samples(position, self.rate);
                self.send(ToAudio::Locate(sample));
            }
            TransportCommand::Record => self.start_recording(),
        }
    }

    fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }
        let armed: Vec<Arc<Track>> = self
            .model
            .tracks
            .iter()
            .filter(|t| t.record_armed)
            .cloned()
            .collect();
        if armed.is_empty() {
            self.emit(Event::Error {
                message: "no track is armed for recording".into(),
            });
            return;
        }
        let output_latency = self.stream.output_latency_frames as i64
            + self.shared.plan_latency.load(Ordering::Acquire) as i64;
        let round_trip = output_latency + self.stream.input_latency_frames as i64;
        let punch = match (self.model.punch_enabled, self.model.punch_range) {
            (true, Some(range)) => Some((
                self.map.position_to_samples(range.start, self.rate),
                self.map.position_to_samples(range.end, self.rate),
            )),
            _ => None,
        };
        let mut captures = Vec::new();
        let mut audio = Vec::new();
        let mut midi_tracks = Vec::new();
        for track in &armed {
            match (track.kind, track.input) {
                (
                    TrackKind::Audio,
                    TrackInput::Audio {
                        first_channel,
                        channels,
                    },
                ) => {
                    self.take_counter += 1;
                    let path = self.capture_dir.join(format!(
                        "take-{:04}-track-{}.wav",
                        self.take_counter, track.id.0
                    ));
                    let spec = WavSpec {
                        sample_rate: self.rate,
                        channels: channels as u16,
                        format: SampleFormat::Float32,
                    };
                    match self.butler.start_capture(&path, spec) {
                        Ok((capture, sink)) => {
                            let stats = sink.stats().clone();
                            captures.push(CaptureRt {
                                sink,
                                first_channel: first_channel as usize,
                                channels: channels as usize,
                            });
                            audio.push(AudioTake {
                                track: track.id,
                                capture,
                                stats,
                                path,
                                channels: channels as u16,
                            });
                        }
                        Err(error) => self.emit(Event::Error {
                            message: format!(
                                "could not start recording to {}: {error}",
                                path.display()
                            ),
                        }),
                    }
                }
                (TrackKind::Instrument, TrackInput::Midi) => midi_tracks.push(track.id),
                _ => {}
            }
        }
        if audio.is_empty() && midi_tracks.is_empty() {
            self.emit(Event::Error {
                message: "no armed track has an input to record from".into(),
            });
            return;
        }

        let tracks: Vec<TrackId> = armed.iter().map(|t| t.id).collect();
        self.recording = Some(RecordingSession {
            start: 0,
            tracks,
            output_latency,
            audio,
            midi_tracks,
            midi_events: Vec::new(),
            wraps: Vec::new(),
            punch,
        });
        // The audio thread fixes the exact start (it alone knows the position to the sample)
        // and reports it back; a little ahead of now if the transport is already rolling.
        self.send(ToAudio::Record {
            captures,
            round_trip,
            count_in_bars: self.model.metronome.count_in_bars as u32,
            punch_ahead: 2 * self.max_block as i64,
            punch,
            play: true,
        });
    }

    /// The audio thread would not start the recording: put away what was prepared for it.
    fn recording_refused(&mut self, captures: Vec<CaptureRt>) {
        drop(captures);
        if let Some(session) = self.recording.take() {
            for take in &session.audio {
                let _ = self
                    .butler
                    .finish_capture(take.capture)
                    .recv_timeout(Duration::from_secs(30));
                let _ = std::fs::remove_file(&take.path);
            }
        }
        self.emit(Event::Error {
            message: "the playhead is past the punch range: there is nothing to record".into(),
        });
    }

    fn finish_recording(&mut self, captures: Vec<CaptureRt>, rolled: i64) {
        // Dropping the sinks tells nobody anything; the butler is told explicitly below.
        drop(captures);
        let Some(session) = self.recording.take() else {
            return;
        };
        let passes = passes_of(session.start, &session.wraps, rolled);
        let mut ops = Vec::new();
        let mut finished = Vec::new();
        let mut next_id = self.model.fresh_id();
        let mut fresh = || {
            next_id += 1;
            next_id - 1
        };

        for take in &session.audio {
            let result = self
                .butler
                .finish_capture(take.capture)
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| Err("the disk thread did not answer".into()));
            let result = match result {
                Err(error) => {
                    self.emit(Event::Error {
                        message: format!("recording failed: {error}"),
                    });
                    continue;
                }
                Ok(result) => result,
            };
            if result.frames_dropped > 0 {
                self.emit(Event::RecordingOverflow {
                    track: take.track,
                    frames_dropped: result.frames_dropped,
                });
            }
            // What each pass has of the file. The file's first frame is the take's first
            // sample, and it runs on through every wrap; the tail of the last pass that had
            // not come back through the input when recording stopped is not in it.
            let frames = result.frames as i64;
            let pieces: Vec<(Pass, i64)> = passes
                .iter()
                .map(|pass| (*pass, pass.rolled_end.min(frames) - pass.rolled_start))
                .filter(|(_, length)| *length > 0)
                .collect();
            if pieces.is_empty() {
                let _ = std::fs::remove_file(&take.path);
                continue;
            }
            let media = MediaId(fresh());
            ops.push(Op::AddMedia {
                media: Media {
                    id: media,
                    path: take.path.to_string_lossy().into_owned(),
                    sample_rate: self.rate,
                    channels: take.channels,
                    frames: result.frames,
                },
            });
            // A punch replaces what was on the track where it lands.
            if session.punch.is_some() {
                let (pass, length) = pieces[0];
                if let Some(track) = self.model.track(take.track) {
                    ops.extend(self.make_room(
                        track,
                        pass.timeline_start,
                        pass.timeline_start + length,
                        &mut fresh,
                    ));
                }
            }
            let group = (pieces.len() > 1).then(&mut fresh);
            let lengths: Vec<i64> = pieces.iter().map(|(_, length)| *length).collect();
            let heard = audible_take(&lengths);
            let name = take
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for (number, (pass, length)) in pieces.iter().enumerate() {
                let clip = ClipId(fresh());
                let take_mark = group.map(|group| riddim_model::Take {
                    group,
                    number: number as u32 + 1,
                });
                ops.push(Op::AddClip {
                    track: take.track,
                    clip: Clip {
                        id: clip,
                        name: match take_mark {
                            Some(mark) => format!("{name} take {}", mark.number),
                            None => name.clone(),
                        },
                        // Recorded audio is anchored to audio time: it does not move if the
                        // tempo is changed later.
                        start: Position::Time(samples_to_superclock(
                            pass.timeline_start,
                            self.rate,
                        )),
                        muted: number != heard,
                        take: take_mark,
                        content: ClipContent::Audio {
                            media,
                            offset: pass.rolled_start as u64,
                            length: *length as u64,
                            gain: 1.0,
                            fade_in: 0,
                            fade_out: 0,
                        },
                    },
                });
                finished.push((
                    take.track,
                    clip,
                    Some(media),
                    *length as u64,
                    take_mark.map(|mark| mark.number),
                ));
            }
        }

        // Notes: the player heard the timeline `output_latency` late, so what they played
        // belongs that much earlier than the block it arrived in, and the last pass ends that
        // much before the transport stopped (or at the punch-out).
        let mut midi_passes = passes.clone();
        if let Some(last) = midi_passes.last_mut() {
            last.rolled_end -= session.output_latency;
            if let Some((punch_in, punch_out)) = session.punch {
                last.rolled_end = last.rolled_end.min(punch_out - punch_in.max(session.start));
            }
        }
        midi_passes.retain(|pass| pass.rolled_end > pass.rolled_start);
        for track in &session.midi_tracks {
            let clips: Vec<Clip> = midi_passes
                .iter()
                .filter_map(|pass| self.midi_take(*track, &session, pass, ClipId(fresh())))
                .collect();
            let group = (clips.len() > 1).then(&mut fresh);
            let lengths: Vec<i64> = clips
                .iter()
                .map(|clip| match &clip.content {
                    ClipContent::Midi { length, .. } => length.0,
                    _ => 0,
                })
                .collect();
            let heard = audible_take(&lengths);
            for (number, mut clip) in clips.into_iter().enumerate() {
                let take_mark = group.map(|group| riddim_model::Take {
                    group,
                    number: number as u32 + 1,
                });
                clip.take = take_mark;
                clip.muted = number != heard;
                let notes = match &clip.content {
                    ClipContent::Midi { notes, .. } => notes.len() as u64,
                    _ => 0,
                };
                finished.push((
                    *track,
                    clip.id,
                    None,
                    notes,
                    take_mark.map(|mark| mark.number),
                ));
                self.write_midi_take(*track, &clip);
                ops.push(Op::AddClip {
                    track: *track,
                    clip,
                });
            }
        }

        if !ops.is_empty() && self.apply_internal(ops) {
            for (track, clip, media, frames, take) in finished {
                self.emit(Event::RecordingFinished {
                    track,
                    clip,
                    media,
                    frames,
                    take,
                });
            }
        }
    }

    /// The edits that clear the stretch `from..to` (timeline samples) of `track` of audio
    /// clips, for a punched-in take to go there: clips are trimmed back to the edges, split
    /// around the stretch, or removed if they lie wholly inside it. The media is not touched,
    /// and the edits are ordinary ops, undone with the take.
    fn make_room(
        &self,
        track: &Track,
        from: i64,
        to: i64,
        fresh: &mut impl FnMut() -> u64,
    ) -> Vec<Op> {
        let rate = self.rate as i64;
        let mut ops = Vec::new();
        for clip in &track.clips {
            let ClipContent::Audio {
                media,
                offset,
                length,
                gain,
                fade_in,
                fade_out,
            } = clip.content
            else {
                continue;
            };
            let media_rate = self
                .model
                .media(media)
                .map_or(rate, |m| m.sample_rate.max(1) as i64);
            let start = self.map.position_to_samples(clip.start, self.rate);
            // Timeline samples <-> frames of this clip's media.
            let to_media = |samples: i64| riddim_time::mul_div_floor(samples, media_rate, rate);
            let to_media_up =
                |samples: i64| -riddim_time::mul_div_floor(-samples, media_rate, rate);
            let end = start - riddim_time::mul_div_floor(-(length as i64), rate, media_rate);
            if end <= from || start >= to {
                continue;
            }
            let audio =
                |offset: u64, length: u64, fade_in: u64, fade_out: u64| ClipContent::Audio {
                    media,
                    offset,
                    length,
                    gain,
                    fade_in: fade_in.min(length),
                    fade_out: fade_out.min(length),
                };
            // What is left before the stretch keeps the clip's id; what is left after it is
            // a clip of its own unless there is nothing before.
            let before = (start < from)
                .then(|| to_media(from - start))
                .filter(|kept| *kept > 0);
            let after = (end > to).then(|| {
                // A clip anchored to beats stays anchored to beats, on the first tick at or
                // after the stretch's end.
                let position = match clip.start {
                    Position::Time(_) => Position::Time(samples_to_superclock(to, self.rate)),
                    Position::Beats(_) => Position::Beats(
                        self.map
                            .superclock_to_ticks_ceil(samples_to_superclock(to, self.rate)),
                    ),
                };
                let resumes = self.map.position_to_samples(position, self.rate).max(to);
                let skipped = to_media_up(resumes - start).min(length as i64);
                (position, skipped as u64, length - skipped as u64)
            });
            let after = after.filter(|(_, _, kept)| *kept > 0);
            match (before, after) {
                (None, None) => ops.push(Op::RemoveClip {
                    track: track.id,
                    clip: clip.id,
                }),
                (Some(kept), after) => {
                    ops.push(Op::ReplaceClip {
                        track: track.id,
                        clip: Clip {
                            content: audio(offset, kept as u64, fade_in, 0),
                            ..(**clip).clone()
                        },
                    });
                    if let Some((position, skipped, kept)) = after {
                        ops.push(Op::AddClip {
                            track: track.id,
                            clip: Clip {
                                id: ClipId(fresh()),
                                start: position,
                                content: audio(offset + skipped, kept, 0, fade_out),
                                ..(**clip).clone()
                            },
                        });
                    }
                }
                (None, Some((position, skipped, kept))) => ops.push(Op::ReplaceClip {
                    track: track.id,
                    clip: Clip {
                        start: position,
                        content: audio(offset + skipped, kept, 0, fade_out),
                        ..(**clip).clone()
                    },
                }),
            }
        }
        ops
    }

    /// Writes a MIDI take beside the audio takes as a Standard MIDI File, so the performance
    /// exists on disk in a form other tools can read, not only inside the project.
    fn write_midi_take(&mut self, track: TrackId, clip: &Clip) {
        let ClipContent::Midi {
            length,
            notes,
            events,
        } = &clip.content
        else {
            return;
        };
        self.take_counter += 1;
        let path = self.capture_dir.join(format!(
            "take-{:04}-track-{}.mid",
            self.take_counter, track.0
        ));
        let data = riddim_disk::smf::SmfData {
            notes: notes
                .iter()
                .map(|n| riddim_disk::smf::SmfNote {
                    start: n.start,
                    length: n.length,
                    key: n.key,
                    velocity: n.velocity,
                    channel: n.channel,
                })
                .collect(),
            events: events
                .iter()
                .map(|e| riddim_disk::smf::SmfEvent {
                    at: e.at,
                    data: e.data,
                    len: e.len,
                })
                .collect(),
            tempos: vec![(
                Ticks::ZERO,
                self.map.tempo_at(self.map.position_to_ticks(clip.start)),
            )],
            meters: Vec::new(),
            length: *length,
        };
        if let Err(error) = std::fs::write(&path, riddim_disk::smf::write(&data)) {
            self.emit(Event::Error {
                message: format!("could not write {}: {error}", path.display()),
            });
        }
    }

    /// Builds a MIDI clip from what was played on `track` during one pass of the session.
    fn midi_take(
        &self,
        track: TrackId,
        session: &RecordingSession,
        pass: &Pass,
        id: ClipId,
    ) -> Option<Clip> {
        let start_tick = self.map.samples_to_ticks(pass.timeline_start, self.rate);
        let pass_length = pass.rolled_end - pass.rolled_start;
        let end_tick = self
            .map
            .samples_to_ticks(pass.timeline_start + pass_length.max(1), self.rate);
        // Where in the pass a note belongs, given how far the take had rolled when it arrived.
        let tick_of = |into_pass: i64| {
            let sample = pass.timeline_start + into_pass.clamp(0, pass_length);
            Ticks(self.map.samples_to_ticks(sample, self.rate).0 - start_tick.0)
        };
        let mut held: HashMap<(u8, u8), (Ticks, u8)> = HashMap::new();
        let mut notes = Vec::new();
        let mut close =
            |held: &mut HashMap<(u8, u8), (Ticks, u8)>, channel: u8, key: u8, at: Ticks| {
                if let Some((start, velocity)) = held.remove(&(channel, key)) {
                    notes.push(Note {
                        start,
                        length: Ticks((at.0 - start.0).max(1)),
                        key,
                        velocity,
                        channel,
                    });
                }
            };
        let mut others = Vec::new();
        let first_pass = pass.rolled_start == 0;
        for (_, rolled, body) in session.midi_events.iter().filter(|(t, _, _)| *t == track) {
            // Played during the count-in or the run-up to a punch-in: not part of the take.
            if *rolled < 0 {
                continue;
            }
            // (Notes played in the first moments, before the player could have heard the
            // take's start, are kept at the start rather than thrown away.)
            let heard = rolled - session.output_latency;
            let heard = if first_pass { heard.max(0) } else { heard };
            if heard < pass.rolled_start || heard >= pass.rolled_end {
                // A note still held when its pass ends is closed there, below.
                continue;
            }
            let at = tick_of(heard - pass.rolled_start);
            match *body {
                EventBody::NoteOn {
                    channel,
                    key,
                    velocity,
                    ..
                } => {
                    close(&mut held, channel, key, at);
                    let velocity = ((velocity * 127.0).round() as u8).clamp(1, 127);
                    held.insert((channel, key), (at, velocity));
                }
                EventBody::NoteOff { channel, key, .. } => close(&mut held, channel, key, at),
                EventBody::Midi { data, len } => {
                    others.push(riddim_model::MidiEvent { at, data, len });
                }
                EventBody::Param { .. } => {}
            }
        }
        let length = Ticks((end_tick.0 - start_tick.0).max(1));
        let dangling: Vec<(u8, u8)> = held.keys().copied().collect();
        for (channel, key) in dangling {
            close(&mut held, channel, key, length);
        }
        if notes.is_empty() && others.is_empty() {
            return None;
        }
        notes.sort_by_key(|n| (n.start, n.channel, n.key));
        Some(Clip {
            id,
            name: "MIDI take".into(),
            start: Position::Beats(start_tick),
            muted: false,
            take: None,
            content: ClipContent::Midi {
                length,
                notes: Arc::new(notes),
                events: Arc::new(others),
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------------------------

    fn poll_audio(&mut self) {
        while let Ok(message) = self.from_audio.pop() {
            match message {
                FromAudio::RecordingStarted { start } => {
                    if let Some(session) = self.recording.as_mut() {
                        session.start = start;
                        let tracks = session.tracks.clone();
                        self.emit(Event::RecordingStarted {
                            tracks,
                            position_samples: start,
                        });
                    }
                }
                FromAudio::RecordingEnded { captures, rolled } => {
                    self.finish_recording(captures, rolled);
                }
                FromAudio::RecordingRefused { captures } => self.recording_refused(captures),
                FromAudio::RecordedMidi {
                    track,
                    rolled,
                    body,
                } => {
                    if let Some(session) = self.recording.as_mut() {
                        session.midi_events.push((track, rolled, body));
                    }
                }
                FromAudio::PluginParam { slot, id, value } => {
                    let _ = self
                        .host_requests
                        .send(HostRequest::NotifyParam { slot, id, value });
                    self.emit(Event::PluginParam { slot, id, value });
                }
                FromAudio::Jumped { from, to } => {
                    // While recording, the only jumps are loop wraps: each starts a new pass.
                    if let Some(session) = self.recording.as_mut() {
                        session.wraps.push((from, to));
                    }
                    self.playhead_jumped(from, to);
                }
            }
        }
    }

    fn poll_host(&mut self) {
        while let Ok(reply) = self.host_replies.try_recv() {
            match reply {
                HostReply::Loaded {
                    slot,
                    serial,
                    result,
                    restarted,
                } => {
                    let current = self
                        .plugins
                        .get(&slot)
                        .filter(|entry| entry.serial == serial)
                        .map(|entry| entry.track);
                    let Some(track) = current else {
                        // Removed from the model while it was loading (even if the slot has
                        // been filled again since: that is another instance).
                        let processor = result.ok().map(|loaded| loaded.processor);
                        let _ = self
                            .host_requests
                            .send(HostRequest::Unload { serial, processor });
                        continue;
                    };
                    match result {
                        Ok(loaded) => {
                            let inputs: usize =
                                loaded.ports.inputs.iter().map(|c| *c as usize).sum();
                            let outputs: usize =
                                loaded.ports.outputs.iter().map(|c| *c as usize).sum();
                            let cell = NodeCell::new(NodeRt::Plugin(Box::new(PluginRt {
                                slot,
                                processor: loaded.processor,
                                scratch: PlanarScratch::new(inputs, outputs),
                                events_in: riddim_plugin::EventBuffer::with_capacity(
                                    EVENT_CAPACITY,
                                ),
                                events_out: riddim_plugin::EventBuffer::with_capacity(
                                    EVENT_CAPACITY,
                                ),
                                pending_params: riddim_plugin::EventBuffer::with_capacity(
                                    EVENT_CAPACITY,
                                ),
                                automated: Vec::with_capacity(AUTOMATED_CAPACITY),
                                bypass_delay: (loaded.ports.latency > 0).then(|| {
                                    Box::new(DelayLine::new(outputs, loaded.ports.latency as usize))
                                }),
                                was_bypassed: false,
                                main_in: loaded.ports.inputs.first().copied().unwrap_or(0) as usize,
                                main_out: loaded.ports.outputs.first().copied().unwrap_or(0)
                                    as usize,
                            })));
                            self.cells.insert(NodeKind::Plugin(track, slot), cell);
                            let latency = loaded.ports.latency;
                            self.plugins.get_mut(&slot).expect("checked above").status =
                                PluginStatus::Loaded(loaded.ports);
                            // The routing or the block size may have moved on while it loaded.
                            self.sync_plugins();
                            self.rebuild_plan();
                            self.emit(if restarted {
                                Event::PluginRestarted {
                                    track,
                                    slot,
                                    latency,
                                }
                            } else {
                                Event::PluginLoaded {
                                    track,
                                    slot,
                                    latency,
                                    parameters: loaded.parameters,
                                }
                            });
                        }
                        Err(error) => {
                            self.plugins.get_mut(&slot).expect("checked above").status =
                                PluginStatus::Failed;
                            self.emit(Event::PluginFailed { track, slot, error });
                        }
                    }
                }
                HostReply::State { slot, result } => {
                    let track = self.plugins.get(&slot).map(|entry| entry.track);
                    if let Some(save) = self.pending_save.as_mut() {
                        save.waiting.remove(&slot);
                        if let (Ok(bytes), Some(track)) = (result, track) {
                            save.states.push((track, slot, bytes));
                        }
                    }
                    self.finish_save_if_ready();
                }
                HostReply::Event { slot, event } => {
                    let track = self.plugins.get(&slot).map(|entry| entry.track);
                    let target = |param| AutomationTarget::PluginParam { slot, param };
                    match (event, track) {
                        (PluginEvent::ParamChanged { id, value }, _) => {
                            // An edit in the plugin's own editor: its processor has to be told.
                            self.send(ToAudio::Param { slot, id, value });
                            self.emit(Event::PluginParam { slot, id, value });
                            if let Some(track) = track {
                                self.automation_value(track, target(id), value as f32);
                            }
                        }
                        // Grabbing and releasing a knob in the editor is a touch.
                        (PluginEvent::ParamGestureBegin { id }, Some(track)) => {
                            self.touch(track, target(id), true);
                        }
                        (PluginEvent::ParamGestureEnd { id }, Some(track)) => {
                            self.touch(track, target(id), false);
                        }
                        _ => {}
                    }
                }
                HostReply::Latency { slot, latency } => {
                    if let Some(PluginEntry {
                        status: PluginStatus::Loaded(ports),
                        ..
                    }) = self.plugins.get_mut(&slot)
                    {
                        if ports.latency != latency {
                            ports.latency = latency;
                            let channels: usize = ports.outputs.iter().map(|c| *c as usize).sum();
                            let line = (latency > 0)
                                .then(|| Box::new(DelayLine::new(channels, latency as usize)));
                            self.send(ToAudio::BypassDelay { slot, line });
                            self.rebuild_plan();
                            self.emit(Event::PluginLatencyChanged { slot, latency });
                        }
                    }
                }
                HostReply::Unloaded => {}
            }
        }
    }

    /// Frees what the audio thread has retired, and unloads plugins nothing references any more.
    fn collect(&mut self) {
        self.collector.collect();
        let mut waiting = Vec::new();
        for (serial, cell) in std::mem::take(&mut self.retired) {
            match Arc::try_unwrap(cell) {
                Ok(cell) => {
                    let processor = match cell.into_inner() {
                        NodeRt::Plugin(plugin) => Some(plugin.processor),
                        _ => None,
                    };
                    let _ = self
                        .host_requests
                        .send(HostRequest::Unload { serial, processor });
                }
                Err(cell) => waiting.push((serial, cell)),
            }
        }
        self.retired = waiting;

        let mut waiting = Vec::new();
        for (slot, serial, cell) in std::mem::take(&mut self.restarting) {
            match Arc::try_unwrap(cell) {
                Ok(cell) => {
                    let processor = match cell.into_inner() {
                        NodeRt::Plugin(plugin) => Some(plugin.processor),
                        _ => None,
                    };
                    let still_wanted = self.plugins.get(&slot).is_some_and(|e| e.serial == serial);
                    match self.wanted_config(slot) {
                        Some((config, buses)) if still_wanted => {
                            let entry = self.plugins.get_mut(&slot).expect("just checked");
                            entry.buses = buses;
                            entry.max_block = config.max_block;
                            let _ = self.host_requests.send(HostRequest::Restart {
                                slot,
                                serial,
                                processor,
                                config,
                                buses,
                            });
                        }
                        // Removed from the model in the meantime.
                        _ => {
                            let _ = self
                                .host_requests
                                .send(HostRequest::Unload { serial, processor });
                        }
                    }
                }
                Err(cell) => waiting.push((slot, serial, cell)),
            }
        }
        self.restarting = waiting;
    }

    /// Transport changes at once; position and meters about thirty times a second.
    fn report(&mut self) {
        // How far the disk is behind the slowest take: the figure behind a "disk load" meter.
        let backlog = self.recording.as_ref().map_or(0, |session| {
            session
                .audio
                .iter()
                .map(|take| {
                    let pushed = take.stats.frames_pushed.load(Ordering::Relaxed);
                    pushed.saturating_sub(take.stats.frames_written.load(Ordering::Relaxed))
                })
                .max()
                .unwrap_or(0)
        });
        self.shared
            .capture_backlog
            .store(backlog, Ordering::Relaxed);

        let playing = self.shared.playing.load(Ordering::Acquire);
        let recording = self.shared.recording.load(Ordering::Acquire);
        let state = match (playing, recording) {
            (true, true) => TransportState::Recording,
            (true, false) => TransportState::Playing,
            _ => TransportState::Stopped,
        };
        let position = self.shared.position.load(Ordering::Acquire);
        if state != self.last_state {
            let was_rolling = self.last_state != TransportState::Stopped;
            let is_rolling = state != TransportState::Stopped;
            if is_rolling && !was_rolling {
                self.start_write_lanes();
            } else if was_rolling && !is_rolling {
                self.finish_all_writes();
            }
            self.last_state = state;
            self.emit(Event::Transport {
                state,
                position_samples: position,
            });
        }
        if self.subscribers.is_empty() || self.last_report.elapsed() < Duration::from_millis(33) {
            return;
        }
        self.last_report = Instant::now();
        if playing {
            self.emit(Event::Position { samples: position });
        }
        let mut tracks: Vec<(TrackId, [f32; 2])> = self
            .meters
            .iter()
            .map(|(track, meter)| (*track, meter.take()))
            .collect();
        tracks.sort_by_key(|(track, _)| *track);
        self.emit(Event::Meters { tracks });
    }

    fn answer_syncs(&mut self) {
        if self.pending_syncs.is_empty() {
            return;
        }
        let loading = self
            .plugins
            .values()
            .any(|entry| matches!(entry.status, PluginStatus::Loading));
        if loading || self.pending_save.is_some() {
            return;
        }
        for reply in self.pending_syncs.drain(..) {
            let _ = reply.send(());
        }
    }
}
