//! The model's data types.

use std::sync::Arc;

use riddim_time::{Position, Superclock, TempoMap, Ticks, SUPERCLOCK_PER_SECOND};
use serde::{Deserialize, Serialize};

/// Version of the serialised model. Bumped whenever a stored field changes meaning.
pub const SCHEMA_VERSION: u32 = 1;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}

id_type!(
    /// Identifies a track for the life of the project. The master track is [`TrackId::MASTER`].
    TrackId
);
id_type!(
    /// Identifies a clip within the project.
    ClipId
);
id_type!(
    /// Identifies a media file known to the project.
    MediaId
);
id_type!(
    /// Identifies a plugin slot within the project.
    SlotId
);

impl TrackId {
    pub const MASTER: TrackId = TrackId(0);
}

/// What a track is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    /// Plays audio clips and records audio.
    Audio,
    /// Plays MIDI clips into an instrument plugin and records MIDI.
    Instrument,
    /// Sums other tracks.
    Bus,
    /// The final mix. Exactly one exists and it cannot be removed.
    Master,
}

/// Where a track records from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TrackInput {
    None,
    /// `channels` (1 or 2) device input channels starting at `first_channel`.
    Audio {
        first_channel: u16,
        channels: u8,
    },
    /// Every MIDI input port, all channels.
    Midi,
}

/// Where a track's output goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "id")]
pub enum TrackOutput {
    Master,
    Bus(TrackId),
}

/// A plugin, identified the way `riddim-plugin` identifies it, without depending on it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginRef {
    /// `vst3`, `au`, `clap` or `internal`.
    pub format: String,
    /// The format's unique id for the plugin.
    pub uid: String,
    /// Display name, kept so a missing plugin can still be named.
    #[serde(default)]
    pub name: String,
    /// Where the plugin was last found, as a hint for formats without a registry.
    #[serde(default)]
    pub path: Option<String>,
}

/// One plugin in a track's chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSlot {
    pub id: SlotId,
    pub plugin: PluginRef,
    /// The plugin's opaque saved state. Kept even when the plugin is missing.
    #[serde(default, with = "crate::base64::optional_bytes")]
    pub state: Option<Arc<Vec<u8>>>,
    #[serde(default)]
    pub bypass: bool,
}

/// A note in a MIDI clip. Times are relative to the clip's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Note {
    pub start: Ticks,
    pub length: Ticks,
    pub key: u8,
    /// 1..=127.
    pub velocity: u8,
    pub channel: u8,
}

/// A non-note MIDI message in a clip (controller, pitch bend, pressure, program change).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MidiEvent {
    pub at: Ticks,
    pub data: [u8; 3],
    pub len: u8,
}

/// What a clip plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ClipContent {
    Audio {
        media: MediaId,
        /// First frame of the media to play, in the media's own frames.
        offset: u64,
        /// Frames of the media to play.
        length: u64,
        /// Linear gain.
        gain: f32,
        /// Fade lengths in media frames.
        fade_in: u64,
        fade_out: u64,
    },
    Midi {
        length: Ticks,
        notes: Arc<Vec<Note>>,
        #[serde(default)]
        events: Arc<Vec<MidiEvent>>,
    },
}

/// A piece of audio or MIDI placed on a track's timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    #[serde(default)]
    pub name: String,
    pub start: Position,
    #[serde(default)]
    pub muted: bool,
    pub content: ClipContent,
}

/// An audio file the project uses. The engine reads it; the application owns where it lives.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Media {
    pub id: MediaId,
    /// Absolute, or relative to the project's media directory.
    pub path: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

impl Media {
    /// Duration of `frames` of this media in audio time.
    pub fn frames_to_superclock(&self, frames: u64) -> Superclock {
        Superclock(riddim_time::mul_div_floor(
            frames as i64,
            SUPERCLOCK_PER_SECOND,
            self.sample_rate.max(1) as i64,
        ))
    }
}

/// One channel strip and what it plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    /// Linear fader gain, 0.0 to 4.0 (+12 dB).
    pub gain: f32,
    /// -1.0 (left) to 1.0 (right).
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub record_armed: bool,
    /// Pass the live input through the track while not playing back a recording.
    pub monitor: bool,
    pub input: TrackInput,
    pub output: TrackOutput,
    /// Kept sorted by id, so the order is the same whatever order clips were added in.
    #[serde(default)]
    pub clips: Vec<Arc<Clip>>,
    /// The plugin chain, in processing order.
    #[serde(default)]
    pub plugins: Vec<Arc<PluginSlot>>,
}

impl Track {
    /// A track with neutral settings and nothing on it.
    pub fn new(id: TrackId, name: impl Into<String>, kind: TrackKind) -> Track {
        Track {
            id,
            name: name.into(),
            kind,
            gain: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            record_armed: false,
            monitor: false,
            input: TrackInput::None,
            output: TrackOutput::Master,
            clips: Vec::new(),
            plugins: Vec::new(),
        }
    }

    pub fn clip(&self, id: ClipId) -> Option<&Arc<Clip>> {
        self.clips
            .binary_search_by_key(&id, |c| c.id)
            .ok()
            .map(|i| &self.clips[i])
    }

    pub fn plugin(&self, id: SlotId) -> Option<&Arc<PluginSlot>> {
        self.plugins.iter().find(|p| p.id == id)
    }
}

/// The loop region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopRange {
    pub start: Position,
    pub end: Position,
}

/// Metronome settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metronome {
    pub enabled: bool,
    /// Linear gain of the click.
    pub gain: f32,
    /// Bars of click before recording starts. 0 disables the count-in.
    pub count_in_bars: u8,
}

impl Default for Metronome {
    fn default() -> Self {
        Metronome {
            enabled: false,
            gain: 0.5,
            count_in_bars: 0,
        }
    }
}

/// Everything the engine plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub schema: u32,
    pub tempo_map: TempoMap,
    pub master: Arc<Track>,
    /// Every track except the master, in mixer order.
    pub tracks: Vec<Arc<Track>>,
    /// Kept sorted by id.
    pub media: Vec<Arc<Media>>,
    pub loop_range: Option<LoopRange>,
    pub loop_enabled: bool,
    pub metronome: Metronome,
}

impl Default for Model {
    fn default() -> Self {
        Model::new()
    }
}

impl Model {
    /// An empty project: a master track, 120 bpm, 4/4.
    pub fn new() -> Model {
        Model {
            schema: SCHEMA_VERSION,
            tempo_map: TempoMap::default(),
            master: Arc::new(Track::new(TrackId::MASTER, "Master", TrackKind::Master)),
            tracks: Vec::new(),
            media: Vec::new(),
            loop_range: None,
            loop_enabled: false,
            metronome: Metronome::default(),
        }
    }

    /// The track with this id, master included.
    pub fn track(&self, id: TrackId) -> Option<&Arc<Track>> {
        if id == TrackId::MASTER {
            Some(&self.master)
        } else {
            self.tracks.iter().find(|t| t.id == id)
        }
    }

    /// Position of a (non-master) track in mixer order.
    pub fn track_index(&self, id: TrackId) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    pub fn media(&self, id: MediaId) -> Option<&Arc<Media>> {
        self.media
            .binary_search_by_key(&id, |m| m.id)
            .ok()
            .map(|i| &self.media[i])
    }

    /// Every track, master last.
    pub fn all_tracks(&self) -> impl Iterator<Item = &Arc<Track>> {
        self.tracks.iter().chain(std::iter::once(&self.master))
    }

    /// An id not used by any track, clip, media or plugin slot in the model. Ids only need to
    /// be unique within their own kind; one counter for all of them is simply convenient.
    pub fn fresh_id(&self) -> u64 {
        let mut highest = 0;
        for track in self.all_tracks() {
            highest = highest.max(track.id.0);
            for clip in &track.clips {
                highest = highest.max(clip.id.0);
            }
            for slot in &track.plugins {
                highest = highest.max(slot.id.0);
            }
        }
        for media in &self.media {
            highest = highest.max(media.id.0);
        }
        highest + 1
    }

    /// Serialises the model as JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("the model always serialises")
    }

    /// Parses a model, refusing schema versions this build does not know.
    pub fn from_json(json: &str) -> Result<Model, String> {
        let model: Model = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if model.schema != SCHEMA_VERSION {
            return Err(format!(
                "model schema {} is not supported (this build reads schema {})",
                model.schema, SCHEMA_VERSION
            ));
        }
        model.validate()?;
        Ok(model)
    }

    /// Checks the invariants ops maintain, for models that came from outside (a file).
    pub fn validate(&self) -> Result<(), String> {
        if self.master.id != TrackId::MASTER || self.master.kind != TrackKind::Master {
            return Err("the master track is malformed".into());
        }
        let mut track_ids = std::collections::HashSet::new();
        let mut clip_ids = std::collections::HashSet::new();
        let mut slot_ids = std::collections::HashSet::new();
        for track in self.all_tracks() {
            if !track_ids.insert(track.id) {
                return Err(format!("duplicate track id {}", track.id.0));
            }
            if track.kind == TrackKind::Master && track.id != TrackId::MASTER {
                return Err("a second master track".into());
            }
            crate::ops::check_strip(track.gain, track.pan).map_err(|e| e.to_string())?;
            if !track.clips.windows(2).all(|w| w[0].id < w[1].id) {
                return Err(format!(
                    "clips of track {} are not sorted by id",
                    track.id.0
                ));
            }
            for clip in &track.clips {
                if !clip_ids.insert(clip.id) {
                    return Err(format!("duplicate clip id {}", clip.id.0));
                }
                crate::ops::check_clip(self, track, clip).map_err(|e| e.to_string())?;
            }
            for slot in &track.plugins {
                if !slot_ids.insert(slot.id) {
                    return Err(format!("duplicate plugin slot id {}", slot.id.0));
                }
            }
        }
        if !self.media.windows(2).all(|w| w[0].id < w[1].id) {
            return Err("media are not sorted by id".into());
        }
        for track in &self.tracks {
            crate::ops::check_output(self, track.id, track.output).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
