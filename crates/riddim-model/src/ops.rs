//! Edit operations: the only way the model changes.

use std::fmt;
use std::sync::Arc;

use riddim_time::{Position, TempoMap, Ticks};
use serde::{Deserialize, Serialize};

use crate::model::{
    AutomationLane, AutomationTarget, Clip, ClipContent, ClipId, LaneId, LoopRange, Media, MediaId,
    Metronome, Model, PluginOutput, PluginSlot, Send, SendId, Sidechain, SlotId, Track, TrackId,
    TrackInput, TrackKind, TrackOutput, MAX_PLUGIN_OUTPUTS,
};

/// One property of a track's strip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "field", content = "value")]
pub enum TrackChange {
    Name(String),
    Gain(f32),
    Pan(f32),
    Mute(bool),
    Solo(bool),
    RecordArmed(bool),
    Monitor(bool),
    /// Flip the polarity of the track's output.
    Invert(bool),
    Input(TrackInput),
    Output(TrackOutput),
    /// For an instrument track: also play the notes another track's plugins put out.
    EventsFrom(Option<TrackId>),
}

/// An edit to the model.
///
/// Every op is validated against the model it is applied to, applies completely or not at all,
/// and yields the op that undoes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum Op {
    /// Inserts a track (with whatever clips and plugins it carries) at `index` in mixer order.
    AddTrack {
        index: usize,
        track: Track,
    },
    RemoveTrack {
        id: TrackId,
    },
    MoveTrack {
        id: TrackId,
        index: usize,
    },
    SetTrack {
        id: TrackId,
        change: TrackChange,
    },

    AddClip {
        track: TrackId,
        clip: Clip,
    },
    RemoveClip {
        track: TrackId,
        clip: ClipId,
    },
    /// Replaces the clip that has the same id as `clip`. Moves, trims, gain changes and note
    /// edits are all this.
    ReplaceClip {
        track: TrackId,
        clip: Clip,
    },

    AddMedia {
        media: Media,
    },
    RemoveMedia {
        id: MediaId,
    },

    InsertPlugin {
        track: TrackId,
        index: usize,
        slot: PluginSlot,
    },
    RemovePlugin {
        track: TrackId,
        slot: SlotId,
    },
    MovePlugin {
        track: TrackId,
        slot: SlotId,
        index: usize,
    },
    /// Stores a plugin's opaque state in its slot (what a project save does).
    SetPluginState {
        track: TrackId,
        slot: SlotId,
        #[serde(default, with = "crate::base64::optional_bytes")]
        state: Option<Arc<Vec<u8>>>,
    },
    SetPluginBypass {
        track: TrackId,
        slot: SlotId,
        bypass: bool,
    },
    /// Sets where a plugin's extra outputs go and what feeds its sidechain. Both at once,
    /// because both change which of the plugin's buses are switched on.
    SetPluginRouting {
        track: TrackId,
        slot: SlotId,
        #[serde(default)]
        outputs: Vec<PluginOutput>,
        #[serde(default)]
        sidechain: Option<Sidechain>,
    },

    /// Adds a send at `index` in the track's list of sends.
    AddSend {
        track: TrackId,
        index: usize,
        send: Send,
    },
    RemoveSend {
        track: TrackId,
        send: SendId,
    },
    /// Replaces the send that has the same id as `send` (gain, target, pre/post, mute).
    SetSend {
        track: TrackId,
        send: Send,
    },

    AddLane {
        track: TrackId,
        lane: AutomationLane,
    },
    RemoveLane {
        track: TrackId,
        lane: LaneId,
    },
    /// Replaces the lane that has the same id as `lane`: its points, its mode, or both. The
    /// target cannot change.
    ReplaceLane {
        track: TrackId,
        lane: AutomationLane,
    },

    SetTempoMap {
        map: TempoMap,
    },
    SetLoop {
        range: Option<LoopRange>,
        enabled: bool,
    },
    /// The stretch recording is confined to, and whether that applies.
    SetPunch {
        range: Option<LoopRange>,
        enabled: bool,
    },
    SetMetronome {
        metronome: Metronome,
    },
}

/// Why an op was refused. The model is unchanged when one of these comes back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpError {
    UnknownTrack(TrackId),
    UnknownClip(ClipId),
    UnknownMedia(MediaId),
    UnknownSlot(SlotId),
    DuplicateTrack(TrackId),
    DuplicateClip(ClipId),
    DuplicateMedia(MediaId),
    DuplicateSlot(SlotId),
    IndexOutOfRange {
        index: usize,
        len: usize,
    },
    /// The master track cannot be added, removed, moved or re-routed.
    MasterIsFixed,
    /// Media cannot be removed while a clip uses it.
    MediaInUse {
        media: MediaId,
        clip: ClipId,
    },
    /// A track cannot be removed while another track outputs or sends to it, routes a plugin
    /// output to it, listens to it on a sidechain or takes notes from it.
    BusInUse {
        bus: TrackId,
        by: TrackId,
    },
    /// An output must name a bus track.
    NotABus(TrackId),
    /// The routing would feed a bus back into itself.
    RoutingCycle(TrackId),
    /// The clip's content does not suit the track (audio on an instrument track, say).
    WrongClipKind {
        track: TrackId,
        clip: ClipId,
    },
    UnknownSend(SendId),
    UnknownLane(LaneId),
    DuplicateSend(SendId),
    DuplicateLane(LaneId),
    /// The track already has a lane for that control.
    TargetAlreadyAutomated(LaneId),
    /// The plugin slot or send cannot be removed while a lane automates it.
    Automated(LaneId),
    Invalid(&'static str),
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::UnknownTrack(id) => write!(f, "there is no track {}", id.0),
            OpError::UnknownClip(id) => write!(f, "there is no clip {}", id.0),
            OpError::UnknownMedia(id) => write!(f, "there is no media {}", id.0),
            OpError::UnknownSlot(id) => write!(f, "there is no plugin slot {}", id.0),
            OpError::DuplicateTrack(id) => write!(f, "track id {} is already in use", id.0),
            OpError::DuplicateClip(id) => write!(f, "clip id {} is already in use", id.0),
            OpError::DuplicateMedia(id) => write!(f, "media id {} is already in use", id.0),
            OpError::DuplicateSlot(id) => write!(f, "plugin slot id {} is already in use", id.0),
            OpError::IndexOutOfRange { index, len } => {
                write!(f, "index {index} is out of range (length {len})")
            }
            OpError::MasterIsFixed => write!(f, "the master track cannot be changed that way"),
            OpError::MediaInUse { media, clip } => {
                write!(f, "media {} is used by clip {}", media.0, clip.0)
            }
            OpError::BusInUse { bus, by } => {
                write!(f, "bus {} still receives track {}", bus.0, by.0)
            }
            OpError::NotABus(id) => write!(f, "track {} is not a bus", id.0),
            OpError::RoutingCycle(id) => {
                write!(
                    f,
                    "routing track {} there would create a feedback loop",
                    id.0
                )
            }
            OpError::WrongClipKind { track, clip } => {
                write!(f, "clip {} is the wrong kind for track {}", clip.0, track.0)
            }
            OpError::UnknownSend(id) => write!(f, "there is no send {}", id.0),
            OpError::UnknownLane(id) => write!(f, "there is no automation lane {}", id.0),
            OpError::DuplicateSend(id) => write!(f, "send id {} is already in use", id.0),
            OpError::DuplicateLane(id) => write!(f, "lane id {} is already in use", id.0),
            OpError::TargetAlreadyAutomated(id) => {
                write!(f, "lane {} already automates that control", id.0)
            }
            OpError::Automated(id) => {
                write!(f, "automation lane {} still refers to it", id.0)
            }
            OpError::Invalid(why) => write!(f, "invalid value: {why}"),
        }
    }
}

impl std::error::Error for OpError {}

pub(crate) fn check_strip(gain: f32, pan: f32) -> Result<(), OpError> {
    if !gain.is_finite() || !(0.0..=4.0).contains(&gain) {
        return Err(OpError::Invalid("track gain must be between 0 and 4"));
    }
    if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
        return Err(OpError::Invalid("pan must be between -1 and 1"));
    }
    Ok(())
}

fn check_position(position: Position) -> Result<(), OpError> {
    let negative = match position {
        Position::Beats(ticks) => ticks < Ticks::ZERO,
        Position::Time(time) => time.0 < 0,
    };
    if negative {
        Err(OpError::Invalid(
            "positions on the timeline cannot be negative",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn check_clip(model: &Model, track: &Track, clip: &Clip) -> Result<(), OpError> {
    check_position(clip.start)?;
    let wrong_kind = || OpError::WrongClipKind {
        track: track.id,
        clip: clip.id,
    };
    match &clip.content {
        ClipContent::Audio {
            media,
            offset,
            length,
            gain,
            fade_in,
            fade_out,
        } => {
            if track.kind != TrackKind::Audio {
                return Err(wrong_kind());
            }
            let media = model.media(*media).ok_or(OpError::UnknownMedia(*media))?;
            if *length == 0 {
                return Err(OpError::Invalid("an audio clip must have a length"));
            }
            if offset
                .checked_add(*length)
                .is_none_or(|end| end > media.frames)
            {
                return Err(OpError::Invalid(
                    "the clip reaches past the end of its media",
                ));
            }
            if !gain.is_finite() || *gain < 0.0 || *gain > 16.0 {
                return Err(OpError::Invalid("clip gain must be between 0 and 16"));
            }
            if fade_in
                .checked_add(*fade_out)
                .is_none_or(|fades| fades > *length)
            {
                return Err(OpError::Invalid("fades are longer than the clip"));
            }
        }
        ClipContent::Midi {
            length,
            notes,
            events,
        } => {
            if track.kind != TrackKind::Instrument {
                return Err(wrong_kind());
            }
            if *length <= Ticks::ZERO {
                return Err(OpError::Invalid("a MIDI clip must have a length"));
            }
            for note in notes.iter() {
                if note.start < Ticks::ZERO
                    || note.length <= Ticks::ZERO
                    || note.key > 127
                    || note.velocity == 0
                    || note.velocity > 127
                    || note.channel > 15
                {
                    return Err(OpError::Invalid("a note is out of range"));
                }
            }
            for event in events.iter() {
                let status_ok = (0xA0..0xF0).contains(&event.data[0]);
                if event.at < Ticks::ZERO || !status_ok || !(2..=3).contains(&event.len) {
                    return Err(OpError::Invalid("a MIDI event is malformed"));
                }
            }
        }
    }
    Ok(())
}

/// Checks that `track` may output to `output` without creating a loop.
pub(crate) fn check_output(
    model: &Model,
    track: TrackId,
    output: TrackOutput,
) -> Result<(), OpError> {
    let TrackOutput::Bus(mut bus) = output else {
        return Ok(());
    };
    // Follow the chain of buses downstream; arriving back at `track` is a loop. The walk is
    // bounded because a model can only ever hold a finite chain.
    for _ in 0..=model.tracks.len() {
        if bus == track {
            return Err(OpError::RoutingCycle(track));
        }
        let target = model.track(bus).ok_or(OpError::UnknownTrack(bus))?;
        if target.kind != TrackKind::Bus {
            return Err(OpError::NotABus(bus));
        }
        match target.output {
            TrackOutput::Master => return Ok(()),
            TrackOutput::Bus(next) => bus = next,
        }
    }
    Err(OpError::RoutingCycle(track))
}

/// Checks one send of `track`: it goes to a bus, other than the track itself, at a sane gain.
pub(crate) fn check_send(model: &Model, track: TrackId, send: &Send) -> Result<(), OpError> {
    if !send.gain.is_finite() || !(0.0..=4.0).contains(&send.gain) {
        return Err(OpError::Invalid("send gain must be between 0 and 4"));
    }
    if send.target == track {
        return Err(OpError::RoutingCycle(track));
    }
    let target = model
        .track(send.target)
        .ok_or(OpError::UnknownTrack(send.target))?;
    if target.kind != TrackKind::Bus {
        return Err(OpError::NotABus(send.target));
    }
    Ok(())
}

/// Checks a lane against the track that carries it.
pub(crate) fn check_lane(track: &Track, lane: &AutomationLane) -> Result<(), OpError> {
    match lane.target {
        AutomationTarget::TrackGain | AutomationTarget::TrackPan => {}
        AutomationTarget::SendGain { send } => {
            track.send(send).ok_or(OpError::UnknownSend(send))?;
        }
        AutomationTarget::PluginParam { slot, .. } => {
            track.plugin(slot).ok_or(OpError::UnknownSlot(slot))?;
        }
    }
    let (low, high) = lane.target.range();
    for point in lane.points.iter() {
        if !point.value.is_finite() || point.value < low || point.value > high {
            return Err(OpError::Invalid(
                "an automation value is out of range for its control",
            ));
        }
        if point.at < Ticks::ZERO {
            return Err(OpError::Invalid(
                "automation cannot start before the timeline",
            ));
        }
    }
    if !lane.points.windows(2).all(|w| w[0].at < w[1].at) {
        return Err(OpError::Invalid(
            "automation points must be in strictly increasing order",
        ));
    }
    Ok(())
}

/// Every track that `track`'s signal (audio or notes) goes into directly.
pub(crate) fn feeds(model: &Model, track: &Track) -> Vec<TrackId> {
    let mut next = Vec::new();
    if let (TrackOutput::Bus(bus), true) = (track.output, track.kind != TrackKind::Master) {
        next.push(bus);
    }
    next.extend(track.sends.iter().map(|s| s.target));
    next.extend(
        track
            .plugins
            .iter()
            .flat_map(|slot| slot.outputs.iter().map(|o| o.target)),
    );
    // Listeners: tracks with a plugin sidechained from this one, or taking its notes.
    for other in model.all_tracks() {
        let listens = other.events_from == Some(track.id)
            || other
                .plugins
                .iter()
                .any(|slot| slot.sidechain.is_some_and(|sc| sc.source == track.id));
        if listens {
            next.push(other.id);
        }
    }
    next
}

/// Checks that nothing can travel from a track back into itself: through outputs, sends,
/// plugin outputs, sidechains or note routing.
pub(crate) fn check_routing(model: &Model) -> Result<(), OpError> {
    // Depth-first search over "feeds into" edges, with the usual three colours.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    fn visit(
        model: &Model,
        track: &Track,
        marks: &mut std::collections::HashMap<TrackId, Mark>,
    ) -> Result<(), OpError> {
        match marks.get(&track.id).copied().unwrap_or(Mark::New) {
            Mark::Done => return Ok(()),
            Mark::Open => return Err(OpError::RoutingCycle(track.id)),
            Mark::New => {}
        }
        marks.insert(track.id, Mark::Open);
        for next in feeds(model, track) {
            if let Some(next) = model.track(next) {
                visit(model, next, marks)?;
            }
        }
        marks.insert(track.id, Mark::Done);
        Ok(())
    }
    let mut marks = std::collections::HashMap::new();
    for track in &model.tracks {
        visit(model, track, &mut marks)?;
    }
    Ok(())
}

/// Checks a slot's extra outputs and sidechain against the model that will hold it.
pub(crate) fn check_plugin_routing(
    model: &Model,
    track: TrackId,
    slot: &PluginSlot,
) -> Result<(), OpError> {
    let mut buses = std::collections::HashSet::new();
    for output in &slot.outputs {
        if output.bus == 0 || output.bus > MAX_PLUGIN_OUTPUTS {
            return Err(OpError::Invalid(
                "plugin outputs are numbered from 1 (the first after the main output) to 63",
            ));
        }
        if !buses.insert(output.bus) {
            return Err(OpError::Invalid("a plugin output can only be routed once"));
        }
        if output.target == track {
            return Err(OpError::RoutingCycle(track));
        }
        let target = model
            .track(output.target)
            .ok_or(OpError::UnknownTrack(output.target))?;
        if target.kind != TrackKind::Bus {
            return Err(OpError::NotABus(output.target));
        }
    }
    if let Some(sidechain) = slot.sidechain {
        if sidechain.source == track {
            return Err(OpError::RoutingCycle(track));
        }
        let source = model
            .track(sidechain.source)
            .ok_or(OpError::UnknownTrack(sidechain.source))?;
        if source.kind == TrackKind::Master {
            return Err(OpError::Invalid("the master cannot feed a sidechain"));
        }
    }
    Ok(())
}

/// Checks where a track takes notes from.
pub(crate) fn check_events_from(
    model: &Model,
    track: &Track,
    source: Option<TrackId>,
) -> Result<(), OpError> {
    let Some(source) = source else { return Ok(()) };
    if track.kind != TrackKind::Instrument {
        return Err(OpError::Invalid(
            "only an instrument track can take notes from another track",
        ));
    }
    if source == track.id {
        return Err(OpError::RoutingCycle(track.id));
    }
    let from = model.track(source).ok_or(OpError::UnknownTrack(source))?;
    if from.kind == TrackKind::Master {
        return Err(OpError::Invalid("the master has no notes to give"));
    }
    Ok(())
}

/// Checks a loop or punch range.
pub(crate) fn check_range(
    model: &Model,
    range: Option<LoopRange>,
    message: &'static str,
) -> Result<(), OpError> {
    if let Some(range) = range {
        check_position(range.start)?;
        check_position(range.end)?;
        let start = model.tempo_map.position_to_superclock(range.start);
        let end = model.tempo_map.position_to_superclock(range.end);
        if end <= start {
            return Err(OpError::Invalid(message));
        }
    }
    Ok(())
}

fn check_index(index: usize, len: usize) -> Result<(), OpError> {
    if index > len {
        Err(OpError::IndexOutOfRange { index, len })
    } else {
        Ok(())
    }
}

impl Model {
    fn track_mut(&mut self, id: TrackId) -> Result<&mut Track, OpError> {
        if id == TrackId::MASTER {
            return Ok(Arc::make_mut(&mut self.master));
        }
        let track = self
            .tracks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(OpError::UnknownTrack(id))?;
        Ok(Arc::make_mut(track))
    }

    fn require_track(&self, id: TrackId) -> Result<&Arc<Track>, OpError> {
        self.track(id).ok_or(OpError::UnknownTrack(id))
    }

    fn clip_exists(&self, id: ClipId) -> bool {
        self.all_tracks().any(|t| t.clip(id).is_some())
    }

    fn slot_exists(&self, id: SlotId) -> bool {
        self.all_tracks().any(|t| t.plugin(id).is_some())
    }

    fn send_exists(&self, id: SendId) -> bool {
        self.all_tracks().any(|t| t.send(id).is_some())
    }

    fn lane_exists(&self, id: LaneId) -> bool {
        self.all_tracks().any(|t| t.lane(id).is_some())
    }

    /// Applies one op. On success returns its inverse; on failure the model is untouched.
    pub fn apply(&mut self, op: &Op) -> Result<Op, OpError> {
        match op {
            Op::AddTrack { index, track } => {
                if track.kind == TrackKind::Master || track.id == TrackId::MASTER {
                    return Err(OpError::MasterIsFixed);
                }
                if self.track(track.id).is_some() {
                    return Err(OpError::DuplicateTrack(track.id));
                }
                check_index(*index, self.tracks.len())?;
                check_strip(track.gain, track.pan)?;
                if !track.clips.windows(2).all(|w| w[0].id < w[1].id) {
                    return Err(OpError::Invalid("a track's clips must be sorted by id"));
                }
                let mut slots = std::collections::HashSet::new();
                for slot in &track.plugins {
                    if self.slot_exists(slot.id) || !slots.insert(slot.id) {
                        return Err(OpError::DuplicateSlot(slot.id));
                    }
                }
                for clip in &track.clips {
                    if self.clip_exists(clip.id) {
                        return Err(OpError::DuplicateClip(clip.id));
                    }
                    check_clip(self, track, clip)?;
                }
                // Validate routing with the track in place, then commit.
                let mut candidate = self.clone();
                candidate.tracks.insert(*index, Arc::new(track.clone()));
                check_output(&candidate, track.id, track.output)?;
                let mut sends = std::collections::HashSet::new();
                for send in &track.sends {
                    if self.send_exists(send.id) || !sends.insert(send.id) {
                        return Err(OpError::DuplicateSend(send.id));
                    }
                    check_send(&candidate, track.id, send)?;
                }
                for slot in &track.plugins {
                    check_plugin_routing(&candidate, track.id, slot)?;
                }
                check_events_from(&candidate, track, track.events_from)?;
                if !track.automation.windows(2).all(|w| w[0].id < w[1].id) {
                    return Err(OpError::Invalid("a track's lanes must be sorted by id"));
                }
                let mut targets = std::collections::HashSet::new();
                for lane in &track.automation {
                    if self.lane_exists(lane.id) {
                        return Err(OpError::DuplicateLane(lane.id));
                    }
                    if !targets.insert(lane.target) {
                        return Err(OpError::TargetAlreadyAutomated(lane.id));
                    }
                    check_lane(track, lane)?;
                }
                check_routing(&candidate)?;
                *self = candidate;
                Ok(Op::RemoveTrack { id: track.id })
            }

            Op::RemoveTrack { id } => {
                if *id == TrackId::MASTER {
                    return Err(OpError::MasterIsFixed);
                }
                let index = self.track_index(*id).ok_or(OpError::UnknownTrack(*id))?;
                if let Some(feeder) = self.tracks.iter().find(|t| {
                    t.id != *id
                        && (t.output == TrackOutput::Bus(*id)
                            || t.sends.iter().any(|s| s.target == *id)
                            || t.events_from == Some(*id)
                            || t.plugins.iter().any(|slot| {
                                slot.outputs.iter().any(|o| o.target == *id)
                                    || slot.sidechain.is_some_and(|sc| sc.source == *id)
                            }))
                }) {
                    return Err(OpError::BusInUse {
                        bus: *id,
                        by: feeder.id,
                    });
                }
                let removed = self.tracks.remove(index);
                Ok(Op::AddTrack {
                    index,
                    track: Arc::unwrap_or_clone(removed),
                })
            }

            Op::MoveTrack { id, index } => {
                if *id == TrackId::MASTER {
                    return Err(OpError::MasterIsFixed);
                }
                let from = self.track_index(*id).ok_or(OpError::UnknownTrack(*id))?;
                if *index >= self.tracks.len() {
                    return Err(OpError::IndexOutOfRange {
                        index: *index,
                        len: self.tracks.len(),
                    });
                }
                let track = self.tracks.remove(from);
                self.tracks.insert(*index, track);
                Ok(Op::MoveTrack {
                    id: *id,
                    index: from,
                })
            }

            Op::SetTrack { id, change } => {
                let current = self.require_track(*id)?;
                let previous = match change {
                    TrackChange::Name(_) => TrackChange::Name(current.name.clone()),
                    TrackChange::Gain(gain) => {
                        check_strip(*gain, current.pan)?;
                        TrackChange::Gain(current.gain)
                    }
                    TrackChange::Pan(pan) => {
                        check_strip(current.gain, *pan)?;
                        TrackChange::Pan(current.pan)
                    }
                    TrackChange::Mute(_) => TrackChange::Mute(current.mute),
                    TrackChange::Solo(_) => TrackChange::Solo(current.solo),
                    TrackChange::RecordArmed(_) => TrackChange::RecordArmed(current.record_armed),
                    TrackChange::Monitor(_) => TrackChange::Monitor(current.monitor),
                    TrackChange::Invert(_) => TrackChange::Invert(current.invert),
                    TrackChange::Input(input) => {
                        if let TrackInput::Audio { channels, .. } = input {
                            if !(1..=2).contains(channels) {
                                return Err(OpError::Invalid(
                                    "an audio input is one or two channels",
                                ));
                            }
                        }
                        TrackChange::Input(current.input)
                    }
                    TrackChange::Output(output) => {
                        if *id == TrackId::MASTER {
                            return Err(OpError::MasterIsFixed);
                        }
                        let mut candidate = self.clone();
                        candidate.track_mut(*id)?.output = *output;
                        check_output(&candidate, *id, *output)?;
                        check_routing(&candidate)?;
                        TrackChange::Output(current.output)
                    }
                    TrackChange::EventsFrom(source) => {
                        check_events_from(self, current, *source)?;
                        let mut candidate = self.clone();
                        candidate.track_mut(*id)?.events_from = *source;
                        check_routing(&candidate)?;
                        TrackChange::EventsFrom(current.events_from)
                    }
                };
                let track = self.track_mut(*id)?;
                match change.clone() {
                    TrackChange::Name(name) => track.name = name,
                    TrackChange::Gain(gain) => track.gain = gain,
                    TrackChange::Pan(pan) => track.pan = pan,
                    TrackChange::Mute(mute) => track.mute = mute,
                    TrackChange::Solo(solo) => track.solo = solo,
                    TrackChange::RecordArmed(armed) => track.record_armed = armed,
                    TrackChange::Monitor(monitor) => track.monitor = monitor,
                    TrackChange::Invert(invert) => track.invert = invert,
                    TrackChange::Input(input) => track.input = input,
                    TrackChange::Output(output) => track.output = output,
                    TrackChange::EventsFrom(source) => track.events_from = source,
                }
                Ok(Op::SetTrack {
                    id: *id,
                    change: previous,
                })
            }

            Op::AddClip { track, clip } => {
                let target = self.require_track(*track)?;
                if self.clip_exists(clip.id) {
                    return Err(OpError::DuplicateClip(clip.id));
                }
                check_clip(self, target, clip)?;
                let target = self.track_mut(*track)?;
                let at = target.clips.partition_point(|c| c.id < clip.id);
                target.clips.insert(at, Arc::new(clip.clone()));
                Ok(Op::RemoveClip {
                    track: *track,
                    clip: clip.id,
                })
            }

            Op::RemoveClip { track, clip } => {
                let target = self.require_track(*track)?;
                let at = target
                    .clips
                    .binary_search_by_key(clip, |c| c.id)
                    .map_err(|_| OpError::UnknownClip(*clip))?;
                let removed = self.track_mut(*track)?.clips.remove(at);
                Ok(Op::AddClip {
                    track: *track,
                    clip: Arc::unwrap_or_clone(removed),
                })
            }

            Op::ReplaceClip { track, clip } => {
                let target = self.require_track(*track)?;
                let at = target
                    .clips
                    .binary_search_by_key(&clip.id, |c| c.id)
                    .map_err(|_| OpError::UnknownClip(clip.id))?;
                check_clip(self, target, clip)?;
                let slot = &mut self.track_mut(*track)?.clips[at];
                let previous = std::mem::replace(slot, Arc::new(clip.clone()));
                Ok(Op::ReplaceClip {
                    track: *track,
                    clip: Arc::unwrap_or_clone(previous),
                })
            }

            Op::AddMedia { media } => {
                if self.media(media.id).is_some() {
                    return Err(OpError::DuplicateMedia(media.id));
                }
                if media.sample_rate == 0 || media.channels == 0 {
                    return Err(OpError::Invalid("media needs a sample rate and channels"));
                }
                let at = self.media.partition_point(|m| m.id < media.id);
                self.media.insert(at, Arc::new(media.clone()));
                Ok(Op::RemoveMedia { id: media.id })
            }

            Op::RemoveMedia { id } => {
                let at = self
                    .media
                    .binary_search_by_key(id, |m| m.id)
                    .map_err(|_| OpError::UnknownMedia(*id))?;
                for track in self.all_tracks() {
                    for clip in &track.clips {
                        if matches!(&clip.content, ClipContent::Audio { media, .. } if media == id)
                        {
                            return Err(OpError::MediaInUse {
                                media: *id,
                                clip: clip.id,
                            });
                        }
                    }
                }
                let removed = self.media.remove(at);
                Ok(Op::AddMedia {
                    media: Arc::unwrap_or_clone(removed),
                })
            }

            Op::InsertPlugin { track, index, slot } => {
                let target = self.require_track(*track)?;
                if self.slot_exists(slot.id) {
                    return Err(OpError::DuplicateSlot(slot.id));
                }
                check_index(*index, target.plugins.len())?;
                if slot.plugin.format.is_empty() || slot.plugin.uid.is_empty() {
                    return Err(OpError::Invalid("a plugin needs a format and an id"));
                }
                let mut candidate = self.clone();
                candidate
                    .track_mut(*track)?
                    .plugins
                    .insert(*index, Arc::new(slot.clone()));
                if !slot.outputs.is_empty() || slot.sidechain.is_some() {
                    check_plugin_routing(&candidate, *track, slot)?;
                    check_routing(&candidate)?;
                }
                *self = candidate;
                Ok(Op::RemovePlugin {
                    track: *track,
                    slot: slot.id,
                })
            }

            Op::RemovePlugin { track, slot } => {
                let target = self.require_track(*track)?;
                let index = target
                    .plugins
                    .iter()
                    .position(|p| p.id == *slot)
                    .ok_or(OpError::UnknownSlot(*slot))?;
                if let Some(lane) = target.automation.iter().find(|l| {
                    matches!(l.target, AutomationTarget::PluginParam { slot: s, .. } if s == *slot)
                }) {
                    return Err(OpError::Automated(lane.id));
                }
                let removed = self.track_mut(*track)?.plugins.remove(index);
                Ok(Op::InsertPlugin {
                    track: *track,
                    index,
                    slot: Arc::unwrap_or_clone(removed),
                })
            }

            Op::MovePlugin { track, slot, index } => {
                let target = self.require_track(*track)?;
                let from = target
                    .plugins
                    .iter()
                    .position(|p| p.id == *slot)
                    .ok_or(OpError::UnknownSlot(*slot))?;
                if *index >= target.plugins.len() {
                    return Err(OpError::IndexOutOfRange {
                        index: *index,
                        len: target.plugins.len(),
                    });
                }
                let plugins = &mut self.track_mut(*track)?.plugins;
                let moved = plugins.remove(from);
                plugins.insert(*index, moved);
                Ok(Op::MovePlugin {
                    track: *track,
                    slot: *slot,
                    index: from,
                })
            }

            Op::SetPluginState { track, slot, state } => {
                let target = self.require_track(*track)?;
                let index = target
                    .plugins
                    .iter()
                    .position(|p| p.id == *slot)
                    .ok_or(OpError::UnknownSlot(*slot))?;
                let entry = Arc::make_mut(&mut self.track_mut(*track)?.plugins[index]);
                let previous = std::mem::replace(&mut entry.state, state.clone());
                Ok(Op::SetPluginState {
                    track: *track,
                    slot: *slot,
                    state: previous,
                })
            }

            Op::SetPluginBypass {
                track,
                slot,
                bypass,
            } => {
                let target = self.require_track(*track)?;
                let index = target
                    .plugins
                    .iter()
                    .position(|p| p.id == *slot)
                    .ok_or(OpError::UnknownSlot(*slot))?;
                let entry = Arc::make_mut(&mut self.track_mut(*track)?.plugins[index]);
                let previous = std::mem::replace(&mut entry.bypass, *bypass);
                Ok(Op::SetPluginBypass {
                    track: *track,
                    slot: *slot,
                    bypass: previous,
                })
            }

            Op::SetPluginRouting {
                track,
                slot,
                outputs,
                sidechain,
            } => {
                let target = self.require_track(*track)?;
                let index = target
                    .plugins
                    .iter()
                    .position(|p| p.id == *slot)
                    .ok_or(OpError::UnknownSlot(*slot))?;
                let mut candidate = self.clone();
                let entry = Arc::make_mut(&mut candidate.track_mut(*track)?.plugins[index]);
                let old_outputs = std::mem::replace(&mut entry.outputs, outputs.clone());
                let old_sidechain = std::mem::replace(&mut entry.sidechain, *sidechain);
                let routed = entry.clone();
                check_plugin_routing(&candidate, *track, &routed)?;
                check_routing(&candidate)?;
                *self = candidate;
                Ok(Op::SetPluginRouting {
                    track: *track,
                    slot: *slot,
                    outputs: old_outputs,
                    sidechain: old_sidechain,
                })
            }

            Op::AddSend { track, index, send } => {
                let target = self.require_track(*track)?;
                if target.kind == TrackKind::Master {
                    return Err(OpError::MasterIsFixed);
                }
                if self.send_exists(send.id) {
                    return Err(OpError::DuplicateSend(send.id));
                }
                check_index(*index, target.sends.len())?;
                check_send(self, *track, send)?;
                let mut candidate = self.clone();
                candidate.track_mut(*track)?.sends.insert(*index, *send);
                check_routing(&candidate)?;
                *self = candidate;
                Ok(Op::RemoveSend {
                    track: *track,
                    send: send.id,
                })
            }

            Op::RemoveSend { track, send } => {
                let target = self.require_track(*track)?;
                let index = target
                    .sends
                    .iter()
                    .position(|s| s.id == *send)
                    .ok_or(OpError::UnknownSend(*send))?;
                if let Some(lane) = target.automation.iter().find(
                    |l| matches!(l.target, AutomationTarget::SendGain { send: s } if s == *send),
                ) {
                    return Err(OpError::Automated(lane.id));
                }
                let removed = self.track_mut(*track)?.sends.remove(index);
                Ok(Op::AddSend {
                    track: *track,
                    index,
                    send: removed,
                })
            }

            Op::SetSend { track, send } => {
                let target = self.require_track(*track)?;
                let index = target
                    .sends
                    .iter()
                    .position(|s| s.id == send.id)
                    .ok_or(OpError::UnknownSend(send.id))?;
                check_send(self, *track, send)?;
                let mut candidate = self.clone();
                let previous =
                    std::mem::replace(&mut candidate.track_mut(*track)?.sends[index], *send);
                check_routing(&candidate)?;
                *self = candidate;
                Ok(Op::SetSend {
                    track: *track,
                    send: previous,
                })
            }

            Op::AddLane { track, lane } => {
                let target = self.require_track(*track)?;
                if self.lane_exists(lane.id) {
                    return Err(OpError::DuplicateLane(lane.id));
                }
                if let Some(existing) = target.lane_for(lane.target) {
                    return Err(OpError::TargetAlreadyAutomated(existing.id));
                }
                check_lane(target, lane)?;
                let target = self.track_mut(*track)?;
                let at = target.automation.partition_point(|l| l.id < lane.id);
                target.automation.insert(at, Arc::new(lane.clone()));
                Ok(Op::RemoveLane {
                    track: *track,
                    lane: lane.id,
                })
            }

            Op::RemoveLane { track, lane } => {
                let target = self.require_track(*track)?;
                let at = target
                    .automation
                    .binary_search_by_key(lane, |l| l.id)
                    .map_err(|_| OpError::UnknownLane(*lane))?;
                let removed = self.track_mut(*track)?.automation.remove(at);
                Ok(Op::AddLane {
                    track: *track,
                    lane: Arc::unwrap_or_clone(removed),
                })
            }

            Op::ReplaceLane { track, lane } => {
                let target = self.require_track(*track)?;
                let at = target
                    .automation
                    .binary_search_by_key(&lane.id, |l| l.id)
                    .map_err(|_| OpError::UnknownLane(lane.id))?;
                if target.automation[at].target != lane.target {
                    return Err(OpError::Invalid("a lane's target cannot change"));
                }
                check_lane(target, lane)?;
                let slot = &mut self.track_mut(*track)?.automation[at];
                let previous = std::mem::replace(slot, Arc::new(lane.clone()));
                Ok(Op::ReplaceLane {
                    track: *track,
                    lane: Arc::unwrap_or_clone(previous),
                })
            }

            Op::SetTempoMap { map } => {
                let previous = std::mem::replace(&mut self.tempo_map, map.clone());
                Ok(Op::SetTempoMap { map: previous })
            }

            Op::SetLoop { range, enabled } => {
                check_range(self, *range, "a loop must end after it starts")?;
                let previous = Op::SetLoop {
                    range: self.loop_range,
                    enabled: self.loop_enabled,
                };
                self.loop_range = *range;
                self.loop_enabled = *enabled;
                Ok(previous)
            }

            Op::SetPunch { range, enabled } => {
                check_range(self, *range, "a punch range must end after it starts")?;
                let previous = Op::SetPunch {
                    range: self.punch_range,
                    enabled: self.punch_enabled,
                };
                self.punch_range = *range;
                self.punch_enabled = *enabled;
                Ok(previous)
            }

            Op::SetMetronome { metronome } => {
                if !metronome.gain.is_finite() || !(0.0..=4.0).contains(&metronome.gain) {
                    return Err(OpError::Invalid("metronome gain must be between 0 and 4"));
                }
                let previous = std::mem::replace(&mut self.metronome, *metronome);
                Ok(Op::SetMetronome {
                    metronome: previous,
                })
            }
        }
    }

    /// Applies a batch atomically: all of the ops, or none.
    ///
    /// On success returns the inverses **in undo order** (apply them first to last to undo the
    /// batch). On failure returns the index of the op that was refused and why.
    pub fn apply_all(&mut self, ops: &[Op]) -> Result<Vec<Op>, (usize, OpError)> {
        let mut scratch = self.clone();
        let mut inverses = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            match scratch.apply(op) {
                Ok(inverse) => inverses.push(inverse),
                Err(error) => return Err((index, error)),
            }
        }
        *self = scratch;
        inverses.reverse();
        Ok(inverses)
    }
}
