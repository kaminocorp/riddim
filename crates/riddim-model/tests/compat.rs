//! Compatibility of the model's JSON: what an earlier version wrote must still load, and what
//! it meant must still be there.
//!
//! The files in `tests/golden/` are projects and edits as version 1 of the schema writes
//! them. They are never regenerated to make a test pass: a change that stops one loading, or
//! that drops or renames something in it, is a breaking change to the project format and
//! needs a schema version bump and a migration (see `docs/api/stability.md`). New fields with
//! defaults are fine, which is why the comparison is "everything in the golden file is still
//! there", not "nothing else is".
//!
//! `RIDDIM_UPDATE_GOLDEN=1 cargo test -p riddim-model --test compat` writes the files afresh.
//! That is for adding a new golden file (a new schema version), not for mending an old one.

use std::path::PathBuf;
use std::sync::Arc;

use riddim_model::time::{Position, Superclock, Tempo, TempoMap, Ticks};
use riddim_model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Clip, ClipContent, ClipId,
    CurveShape, LaneId, LoopRange, Media, MediaId, Metronome, MidiEvent, Model, Note, Op,
    PluginOutput, PluginRef, PluginSlot, Send, SendId, Sidechain, SlotId, Take, Track, TrackChange,
    TrackId, TrackInput, TrackKind, TrackOutput,
};
use serde_json::Value;

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn slot(id: u64, name: &str) -> PluginSlot {
    PluginSlot {
        id: SlotId(id),
        plugin: PluginRef {
            format: "vst3".into(),
            uid: format!("{id:032X}"),
            name: name.into(),
            path: Some(format!("/Library/Audio/Plug-Ins/VST3/{name}.vst3")),
        },
        state: Some(Arc::new(vec![1, 2, 3, 250, 251, 252])),
        bypass: false,
        outputs: Vec::new(),
        sidechain: None,
        bridged: false,
    }
}

/// Every kind of op, in an order that applies cleanly to an empty project. Together they
/// build a project that uses every part of the model.
fn every_op() -> Vec<Op> {
    let track = |id: u64, name: &str, kind| Track::new(TrackId(id), name, kind);
    let set = |id: u64, change| Op::SetTrack {
        id: TrackId(id),
        change,
    };
    let beats = |n: i64| Position::Beats(Ticks::from_beats(n));
    let lane = |points: Vec<AutomationPoint>| AutomationLane {
        id: LaneId(60),
        target: AutomationTarget::TrackGain,
        mode: AutomationMode::Read,
        points: Arc::new(points),
    };
    let point = |beat: i64, value: f32, shape| AutomationPoint {
        at: Ticks::from_beats(beat),
        value,
        shape,
    };
    vec![
        Op::SetTempoMap {
            map: TempoMap::default().with_tempo(Ticks::from_beats(16), Tempo::from_bpm(96.0)),
        },
        Op::AddMedia {
            media: Media {
                id: MediaId(1),
                path: "takes/vocal.wav".into(),
                sample_rate: 48_000,
                channels: 2,
                frames: 480_000,
            },
        },
        Op::AddMedia {
            media: Media {
                id: MediaId(2),
                path: "unused.wav".into(),
                sample_rate: 44_100,
                channels: 1,
                frames: 10,
            },
        },
        Op::RemoveMedia { id: MediaId(2) },
        Op::AddTrack {
            index: 0,
            track: track(10, "Vocal", TrackKind::Audio),
        },
        Op::AddTrack {
            index: 1,
            track: track(20, "Drums", TrackKind::Instrument),
        },
        Op::AddTrack {
            index: 2,
            track: track(30, "Bass", TrackKind::Instrument),
        },
        Op::AddTrack {
            index: 3,
            track: track(40, "Reverb", TrackKind::Bus),
        },
        Op::AddTrack {
            index: 4,
            track: track(50, "Drum room", TrackKind::Bus),
        },
        Op::AddTrack {
            index: 5,
            track: track(99, "Scratch", TrackKind::Audio),
        },
        Op::RemoveTrack { id: TrackId(99) },
        Op::MoveTrack {
            id: TrackId(40),
            index: 4,
        },
        set(10, TrackChange::Name("Lead vocal".into())),
        set(10, TrackChange::Gain(0.8)),
        set(10, TrackChange::Pan(-0.25)),
        set(10, TrackChange::Mute(false)),
        set(20, TrackChange::Solo(false)),
        set(10, TrackChange::RecordArmed(true)),
        set(10, TrackChange::Monitor(true)),
        set(10, TrackChange::Invert(true)),
        set(
            10,
            TrackChange::Input(TrackInput::Audio {
                first_channel: 0,
                channels: 2,
            }),
        ),
        set(20, TrackChange::Input(TrackInput::Midi)),
        set(50, TrackChange::Output(TrackOutput::Bus(TrackId(40)))),
        set(30, TrackChange::EventsFrom(Some(TrackId(20)))),
        Op::AddClip {
            track: TrackId(10),
            clip: Clip {
                id: ClipId(11),
                name: "Verse".into(),
                start: Position::Time(Superclock(282_240_000)),
                muted: true,
                take: Some(Take {
                    group: 500,
                    number: 1,
                }),
                content: ClipContent::Audio {
                    media: MediaId(1),
                    offset: 0,
                    length: 96_000,
                    gain: 1.0,
                    fade_in: 0,
                    fade_out: 0,
                },
            },
        },
        Op::ReplaceClip {
            track: TrackId(10),
            clip: Clip {
                id: ClipId(11),
                name: "Verse".into(),
                start: Position::Time(Superclock(282_240_000)),
                muted: false,
                take: Some(Take {
                    group: 500,
                    number: 2,
                }),
                content: ClipContent::Audio {
                    media: MediaId(1),
                    offset: 96_000,
                    length: 96_000,
                    gain: 0.5,
                    fade_in: 480,
                    fade_out: 960,
                },
            },
        },
        Op::AddClip {
            track: TrackId(20),
            clip: Clip {
                id: ClipId(21),
                name: "Beat".into(),
                start: beats(4),
                muted: false,
                take: None,
                content: ClipContent::Midi {
                    length: Ticks::from_beats(4),
                    notes: Arc::new(vec![Note {
                        start: Ticks::from_beat_fraction(1, 2),
                        length: Ticks::from_beat_fraction(1, 4),
                        key: 36,
                        velocity: 110,
                        channel: 9,
                    }]),
                    events: Arc::new(vec![MidiEvent {
                        at: Ticks::ZERO,
                        data: [0xB9, 7, 100],
                        len: 3,
                    }]),
                },
            },
        },
        Op::AddClip {
            track: TrackId(20),
            clip: Clip {
                id: ClipId(22),
                name: "Fill".into(),
                start: beats(8),
                muted: false,
                take: None,
                content: ClipContent::Midi {
                    length: Ticks::from_beats(1),
                    notes: Arc::new(Vec::new()),
                    events: Arc::new(Vec::new()),
                },
            },
        },
        Op::RemoveClip {
            track: TrackId(20),
            clip: ClipId(22),
        },
        Op::InsertPlugin {
            track: TrackId(20),
            index: 0,
            slot: slot(23, "Drum Sampler"),
        },
        Op::InsertPlugin {
            track: TrackId(40),
            index: 0,
            slot: slot(41, "Plate"),
        },
        Op::InsertPlugin {
            track: TrackId(40),
            index: 1,
            slot: PluginSlot {
                bridged: true,
                ..slot(42, "Compressor")
            },
        },
        Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: slot(12, "Gone"),
        },
        Op::RemovePlugin {
            track: TrackId(10),
            slot: SlotId(12),
        },
        Op::MovePlugin {
            track: TrackId(40),
            slot: SlotId(42),
            index: 0,
        },
        Op::SetPluginState {
            track: TrackId(40),
            slot: SlotId(41),
            state: Some(Arc::new(vec![9, 8, 7])),
        },
        Op::SetPluginBypass {
            track: TrackId(40),
            slot: SlotId(41),
            bypass: true,
        },
        Op::SetPluginRouting {
            track: TrackId(20),
            slot: SlotId(23),
            outputs: vec![PluginOutput {
                bus: 1,
                target: TrackId(50),
            }],
            sidechain: None,
        },
        Op::SetPluginRouting {
            track: TrackId(40),
            slot: SlotId(42),
            outputs: Vec::new(),
            sidechain: Some(Sidechain {
                source: TrackId(10),
                pre_fader: true,
            }),
        },
        Op::AddSend {
            track: TrackId(10),
            index: 0,
            send: Send {
                id: SendId(13),
                target: TrackId(40),
                gain: 0.5,
                pre_fader: false,
                mute: false,
            },
        },
        Op::AddSend {
            track: TrackId(10),
            index: 1,
            send: Send {
                id: SendId(14),
                target: TrackId(50),
                gain: 1.0,
                pre_fader: true,
                mute: true,
            },
        },
        Op::SetSend {
            track: TrackId(10),
            send: Send {
                id: SendId(13),
                target: TrackId(40),
                gain: 0.25,
                pre_fader: true,
                mute: false,
            },
        },
        Op::RemoveSend {
            track: TrackId(10),
            send: SendId(14),
        },
        Op::AddLane {
            track: TrackId(10),
            lane: lane(vec![point(0, 1.0, CurveShape::Linear)]),
        },
        Op::ReplaceLane {
            track: TrackId(10),
            lane: AutomationLane {
                mode: AutomationMode::Touch,
                ..lane(vec![
                    point(0, 0.0, CurveShape::Linear),
                    point(4, 1.0, CurveShape::Hold),
                ])
            },
        },
        Op::AddLane {
            track: TrackId(40),
            lane: AutomationLane {
                id: LaneId(61),
                target: AutomationTarget::PluginParam {
                    slot: SlotId(41),
                    param: 7,
                },
                mode: AutomationMode::Latch,
                points: Arc::new(vec![point(2, 0.5, CurveShape::Linear)]),
            },
        },
        Op::AddLane {
            track: TrackId(10),
            lane: AutomationLane {
                id: LaneId(62),
                target: AutomationTarget::SendGain { send: SendId(13) },
                mode: AutomationMode::Write,
                points: Arc::new(Vec::new()),
            },
        },
        Op::AddLane {
            track: TrackId(10),
            lane: AutomationLane {
                id: LaneId(63),
                target: AutomationTarget::TrackPan,
                mode: AutomationMode::Off,
                points: Arc::new(Vec::new()),
            },
        },
        Op::RemoveLane {
            track: TrackId(10),
            lane: LaneId(63),
        },
        Op::SetLoop {
            range: Some(LoopRange {
                start: beats(4),
                end: beats(12),
            }),
            enabled: true,
        },
        Op::SetPunch {
            range: Some(LoopRange {
                start: beats(8),
                end: Position::Time(Superclock(2_822_400_000)),
            }),
            enabled: false,
        },
        Op::SetMetronome {
            metronome: Metronome {
                enabled: true,
                gain: 0.5,
                count_in_bars: 2,
            },
        },
    ]
}

/// True if everything in `old` is in `new` with the same value: objects key by key (extra
/// keys in `new` are allowed), everything else exactly.
fn preserved(old: &Value, new: &Value, at: &str, missing: &mut Vec<String>) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in old {
                match new.get(key) {
                    Some(now) => preserved(value, now, &format!("{at}.{key}"), missing),
                    None => missing.push(format!("{at}.{key} is gone")),
                }
            }
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            for (index, (value, now)) in old.iter().zip(new).enumerate() {
                preserved(value, now, &format!("{at}[{index}]"), missing);
            }
        }
        (old, new) if old == new => {}
        (old, new) => missing.push(format!("{at} was {old} and is now {new}")),
    }
}

fn check(name: &str, current: &str, reload: impl Fn(&str) -> Result<String, String>) {
    let path = golden(name);
    if std::env::var_os("RIDDIM_UPDATE_GOLDEN").is_some() {
        let pretty: Value = serde_json::from_str(current).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&pretty).unwrap() + "\n").unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }
    let old = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (run with RIDDIM_UPDATE_GOLDEN=1 once)",
            path.display()
        )
    });
    // What the old version wrote still loads...
    let reloaded = reload(&old).unwrap_or_else(|e| {
        panic!("{name} no longer loads: {e}\nThis is a breaking change to the project format.")
    });
    // ...and says what it said.
    let mut missing = Vec::new();
    preserved(
        &serde_json::from_str(&old).unwrap(),
        &serde_json::from_str(&reloaded).unwrap(),
        "",
        &mut missing,
    );
    assert!(
        missing.is_empty(),
        "{name} loads but has lost something:\n  {}\nThis is a breaking change to the project format.",
        missing.join("\n  ")
    );
}

#[test]
fn version_1_ops_still_load_and_mean_what_they_did() {
    let ops = every_op();
    // The list really does cover the op set: every kind appears, and it applies.
    let kinds: std::collections::BTreeSet<String> = ops
        .iter()
        .map(|op| {
            serde_json::to_value(op).unwrap()["op"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds.len(), 25, "{kinds:?}");
    Model::new().apply_all(&ops).expect("the golden ops apply");

    check(
        "ops-v1.json",
        &serde_json::to_string(&ops).unwrap(),
        |old| {
            let ops: Vec<Op> = serde_json::from_str(old).map_err(|e| e.to_string())?;
            let mut model = Model::new();
            model
                .apply_all(&ops)
                .map_err(|(index, error)| format!("op {index}: {error}"))?;
            Ok(serde_json::to_string(&ops).unwrap())
        },
    );
}

#[test]
fn a_version_1_project_still_loads_and_means_what_it_did() {
    let mut model = Model::new();
    model.apply_all(&every_op()).unwrap();
    check("project-v1.json", &model.to_json(), |old| {
        Model::from_json(old).map(|model| model.to_json())
    });
}

#[test]
fn a_project_from_a_newer_schema_is_refused_not_misread() {
    let mut json: Value = serde_json::from_str(&Model::new().to_json()).unwrap();
    json["schema"] = Value::from(riddim_model::SCHEMA_VERSION + 1);
    let error = Model::from_json(&json.to_string()).expect_err("a newer schema must be refused");
    assert!(error.contains("schema"), "{error}");
}
