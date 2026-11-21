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
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Clip, ClipContent, ClipId,
    CurveShape, LaneId, Media, MediaId, Model, Note, Op, PluginOutput, PluginRef, PluginSlot, Send,
    SendId, Sidechain, SlotId, Track, TrackChange, TrackId, TrackKind, TrackOutput,
};
use riddim_test_support::ids;

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

/// Deterministic noise in -0.25..0.25, different for each seed.
pub fn seeded_noise(seed: u32, frames: usize) -> Vec<f32> {
    let mut state = 0x2545_F491u32 ^ seed.wrapping_mul(0x9E37_79B9);
    (0..frames)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            ((state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5
        })
        .collect()
}

/// A project wide enough to be worth sharing out and with every kind of connection in it:
/// sixteen audio tracks (each through a gain plugin, every other one through a delay with
/// latency as well), four instrument tracks, three buses fed by outputs and sends, a
/// multi-out instrument, a sidechain, a track playing another's notes, and automation.
pub fn wide_project(dir: &std::path::Path) -> Model {
    let mut model = Model::new();
    let apply = |model: &mut Model, op: Op| {
        model.apply(&op).unwrap_or_else(|e| panic!("{op:?}: {e}"));
    };
    let add = |model: &mut Model, id: u64, kind: TrackKind| {
        let index = model.tracks.len();
        apply(
            model,
            Op::AddTrack {
                index,
                track: Track::new(TrackId(id), format!("t{id}"), kind),
            },
        );
    };
    for bus in [900, 901, 902] {
        add(&mut model, bus, TrackKind::Bus);
    }
    apply(
        &mut model,
        Op::InsertPlugin {
            track: TrackId(901),
            index: 0,
            slot: fixture_slot(9010, ids::DELAY_UID),
        },
    );

    for n in 0..16u64 {
        let track = 100 + n * 10;
        let frames = 60_000;
        let path = dir.join(format!("noise-{n}.wav"));
        let data = AudioData {
            sample_rate: RATE,
            channels: vec![seeded_noise(n as u32, frames)],
        };
        wav::write(&path, &data, SampleFormat::Float32).unwrap();
        apply(
            &mut model,
            Op::AddMedia {
                media: Media {
                    id: MediaId(track),
                    path: path.file_name().unwrap().to_string_lossy().into_owned(),
                    sample_rate: RATE,
                    channels: 1,
                    frames: frames as u64,
                },
            },
        );
        add(&mut model, track, TrackKind::Audio);
        apply(
            &mut model,
            Op::AddClip {
                track: TrackId(track),
                clip: audio_clip(track + 1, track, (n * 137) as i64, frames as u64),
            },
        );
        apply(
            &mut model,
            Op::InsertPlugin {
                track: TrackId(track),
                index: 0,
                slot: fixture_slot(track + 2, ids::GAIN_UID),
            },
        );
        if n % 2 == 0 {
            apply(
                &mut model,
                Op::InsertPlugin {
                    track: TrackId(track),
                    index: 1,
                    slot: fixture_slot(track + 3, ids::DELAY_UID),
                },
            );
        }
        let set = |change| Op::SetTrack {
            id: TrackId(track),
            change,
        };
        apply(&mut model, set(TrackChange::Pan(n as f32 / 8.0 - 1.0)));
        apply(&mut model, set(TrackChange::Gain(0.2 + n as f32 * 0.03)));
        if n % 3 == 0 {
            apply(
                &mut model,
                set(TrackChange::Output(TrackOutput::Bus(TrackId(900)))),
            );
        }
        if n % 4 == 1 {
            apply(
                &mut model,
                Op::AddSend {
                    track: TrackId(track),
                    index: 0,
                    send: Send {
                        id: SendId(track + 4),
                        target: TrackId(901),
                        gain: 0.5,
                        pre_fader: n % 8 == 1,
                        mute: false,
                    },
                },
            );
        }
        if n % 5 == 2 {
            apply(
                &mut model,
                Op::AddLane {
                    track: TrackId(track),
                    lane: AutomationLane {
                        id: LaneId(track + 5),
                        target: AutomationTarget::TrackGain,
                        mode: AutomationMode::Read,
                        points: std::sync::Arc::new(vec![
                            AutomationPoint {
                                at: Ticks::ZERO,
                                value: 0.1,
                                shape: CurveShape::Linear,
                            },
                            AutomationPoint {
                                at: Ticks::from_beat_fraction(3, 2),
                                value: 0.9,
                                shape: CurveShape::Hold,
                            },
                            AutomationPoint {
                                at: Ticks::from_beats(2),
                                value: 0.3,
                                shape: CurveShape::Linear,
                            },
                        ]),
                    },
                },
            );
        }
    }

    for n in 0..4u64 {
        let track = 500 + n * 10;
        add(&mut model, track, TrackKind::Instrument);
        apply(
            &mut model,
            Op::InsertPlugin {
                track: TrackId(track),
                index: 0,
                slot: fixture_slot(track + 2, ids::SYNTH_UID),
            },
        );
        let mut notes = vec![
            note((0, 1), (1, 2), 45 + n as u8 * 7),
            note((1, 1), (1, 1), 52 + n as u8 * 5),
        ];
        // A note on MIDI channel 2: the fixture synth plays it on its second output.
        let mut second = note((1, 2), (1, 1), 64 + n as u8);
        second.channel = 1;
        notes.push(second);
        apply(
            &mut model,
            Op::AddClip {
                track: TrackId(track),
                clip: midi_clip(track + 1, 0, 4, notes),
            },
        );
    }
    // The first instrument's second output goes to a bus; the delay on bus 901 is keyed from
    // an audio track; the last instrument also plays the third one's notes.
    apply(
        &mut model,
        Op::SetPluginRouting {
            track: TrackId(500),
            slot: SlotId(502),
            outputs: vec![PluginOutput {
                bus: 1,
                target: TrackId(902),
            }],
            sidechain: None,
        },
    );
    apply(
        &mut model,
        Op::SetPluginRouting {
            track: TrackId(901),
            slot: SlotId(9010),
            outputs: vec![],
            sidechain: Some(Sidechain {
                source: TrackId(110),
                pre_fader: true,
            }),
        },
    );
    apply(
        &mut model,
        Op::SetTrack {
            id: TrackId(530),
            change: TrackChange::EventsFrom(Some(TrackId(520))),
        },
    );
    model
}
