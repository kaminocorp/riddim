//! Shared helpers for the engine's integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use riddim_disk::wav::{self, SampleFormat};
use riddim_disk::AudioData;
use riddim_engine::DeviceConfig;
use riddim_engine::{Engine, EngineConfig, Event, NullConfig};
use riddim_model::time::{Position, Superclock, Ticks};
use riddim_model::{
    Clip, ClipContent, ClipId, Media, MediaId, Note, Op, PluginRef, PluginSlot, SlotId, Track,
    TrackId, TrackKind,
};

pub const RATE: u32 = 48_000;
pub const BLOCK: usize = 128;

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// A fresh scratch directory for one test.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "riddim-engine-{tag}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A deterministic engine on the manual device.
pub fn engine(dir: &Path) -> Engine {
    Engine::start(EngineConfig::manual(dir)).expect("engine start")
}

pub fn engine_with(dir: &Path, device: NullConfig) -> Engine {
    let mut config = EngineConfig::manual(dir);
    config.device = DeviceConfig::Manual(device);
    Engine::start(config).expect("engine start")
}

/// Writes a WAV file and returns the op that registers it as media.
pub fn media(dir: &Path, id: u64, sample_rate: u32, channels: Vec<Vec<f32>>) -> Op {
    let path = dir.join(format!("media-{id}.wav"));
    let frames = channels[0].len() as u64;
    let data = AudioData {
        sample_rate,
        channels,
    };
    wav::write(&path, &data, SampleFormat::Float32).unwrap();
    Op::AddMedia {
        media: Media {
            id: MediaId(id),
            path: path.file_name().unwrap().to_string_lossy().into_owned(),
            sample_rate,
            channels: data.channels.len() as u16,
            frames,
        },
    }
}

pub fn add_track(index: usize, id: u64, kind: TrackKind) -> Op {
    Op::AddTrack {
        index,
        track: Track::new(TrackId(id), format!("track {id}"), kind),
    }
}

/// An audio clip playing all `frames` of `media`, starting at timeline sample `at`.
pub fn audio_clip(id: u64, media: u64, at: i64, frames: u64) -> Clip {
    Clip {
        id: ClipId(id),
        name: String::new(),
        start: Position::Time(riddim_model::time::samples_to_superclock(at, RATE)),
        muted: false,
        take: None,
        content: ClipContent::Audio {
            media: MediaId(media),
            offset: 0,
            length: frames,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
        },
    }
}

pub fn note(start_beats: (i64, i64), length_beats: (i64, i64), key: u8) -> Note {
    Note {
        start: Ticks::from_beat_fraction(start_beats.0, start_beats.1),
        length: Ticks::from_beat_fraction(length_beats.0, length_beats.1),
        key,
        velocity: 127,
        channel: 0,
    }
}

pub fn midi_clip(id: u64, at_beats: i64, length_beats: i64, notes: Vec<Note>) -> Clip {
    Clip {
        id: ClipId(id),
        name: String::new(),
        start: Position::Beats(Ticks::from_beats(at_beats)),
        muted: false,
        take: None,
        content: ClipContent::Midi {
            length: Ticks::from_beats(length_beats),
            notes: std::sync::Arc::new(notes),
            events: Default::default(),
        },
    }
}

/// A slot holding one of the fixture VST3 plugins.
pub fn fixture_slot(id: u64, uid: &str) -> PluginSlot {
    PluginSlot {
        id: SlotId(id),
        plugin: PluginRef {
            format: "vst3".into(),
            uid: uid.into(),
            name: String::new(),
            path: Some(
                riddim_test_support::testplug_bundle()
                    .to_string_lossy()
                    .into_owned(),
            ),
        },
        state: None,
        bypass: false,
        outputs: Vec::new(),
        sidechain: None,
    }
}

/// Position::Time for a sample at the test rate.
pub fn at_sample(sample: i64) -> Position {
    Position::Time(Superclock(
        riddim_model::time::samples_to_superclock(sample, RATE).0,
    ))
}

/// Waits for an event matching `pick`, pumping nothing: the control thread runs by itself.
pub fn wait_for<T>(
    events: &Receiver<Event>,
    what: &str,
    mut pick: impl FnMut(&Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(event) => {
                if let Some(found) = pick(&event) {
                    return found;
                }
            }
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

pub fn peak(buffer: &[f32]) -> f32 {
    buffer.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Index of the first sample that is not exactly zero.
pub fn first_sound(buffer: &[f32]) -> Option<usize> {
    buffer.iter().position(|&s| s != 0.0)
}
