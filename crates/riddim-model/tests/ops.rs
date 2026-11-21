//! Ops against concrete cases: what each one does, what it refuses, and that inverses undo.

use std::sync::Arc;

use riddim_model::time::{Position, Superclock, Tempo, TempoMap, Ticks};
use riddim_model::{
    Clip, ClipContent, ClipId, LoopRange, Media, MediaId, Metronome, Model, Note, Op, OpError,
    PluginRef, PluginSlot, SlotId, Track, TrackChange, TrackId, TrackInput, TrackKind, TrackOutput,
};

fn media(id: u64, frames: u64) -> Media {
    Media {
        id: MediaId(id),
        path: format!("m{id}.wav"),
        sample_rate: 48_000,
        channels: 2,
        frames,
    }
}

fn audio_clip(id: u64, media: u64, length: u64) -> Clip {
    Clip {
        id: ClipId(id),
        name: format!("clip {id}"),
        start: Position::Time(Superclock::from_seconds(1)),
        muted: false,
        content: ClipContent::Audio {
            media: MediaId(media),
            offset: 0,
            length,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
        },
    }
}

fn midi_clip(id: u64) -> Clip {
    Clip {
        id: ClipId(id),
        name: String::new(),
        start: Position::Beats(Ticks::from_beats(4)),
        muted: false,
        content: ClipContent::Midi {
            length: Ticks::from_beats(4),
            notes: Arc::new(vec![Note {
                start: Ticks::ZERO,
                length: Ticks::from_beats(1),
                key: 60,
                velocity: 100,
                channel: 0,
            }]),
            events: Arc::new(Vec::new()),
        },
    }
}

fn slot(id: u64) -> PluginSlot {
    PluginSlot {
        id: SlotId(id),
        plugin: PluginRef {
            format: "vst3".into(),
            uid: "ABC".into(),
            name: "Synth".into(),
            path: None,
        },
        state: Some(Arc::new(vec![0, 1, 2, 250, 255])),
        bypass: false,
    }
}

fn add_track(model: &mut Model, id: u64, kind: TrackKind) {
    let index = model.tracks.len();
    model
        .apply(&Op::AddTrack {
            index,
            track: Track::new(TrackId(id), format!("t{id}"), kind),
        })
        .unwrap();
}

/// Applies `op`, checks the inverse restores the model exactly and re-applying gives the same
/// result again, and leaves the op applied.
fn apply_checked(model: &mut Model, op: Op) {
    let before = model.clone();
    let inverse = model
        .apply(&op)
        .unwrap_or_else(|e| panic!("{op:?} was refused: {e}"));
    let after = model.clone();
    let redo = model.apply(&inverse).expect("the inverse must apply");
    assert_eq!(
        *model, before,
        "the inverse of {op:?} did not restore the model"
    );
    assert_eq!(
        redo, op,
        "the inverse of the inverse must be the original op"
    );
    model.apply(&op).unwrap();
    assert_eq!(*model, after);
    model.validate().unwrap();
}

#[test]
fn a_small_session_built_from_ops() {
    let mut model = Model::new();
    apply_checked(
        &mut model,
        Op::AddMedia {
            media: media(10, 96_000),
        },
    );
    apply_checked(
        &mut model,
        Op::AddTrack {
            index: 0,
            track: Track::new(TrackId(1), "Guitar", TrackKind::Audio),
        },
    );
    apply_checked(
        &mut model,
        Op::AddTrack {
            index: 1,
            track: Track::new(TrackId(2), "Piano", TrackKind::Instrument),
        },
    );
    apply_checked(
        &mut model,
        Op::AddTrack {
            index: 2,
            track: Track::new(TrackId(3), "Band", TrackKind::Bus),
        },
    );
    apply_checked(
        &mut model,
        Op::AddClip {
            track: TrackId(1),
            clip: audio_clip(20, 10, 48_000),
        },
    );
    apply_checked(
        &mut model,
        Op::AddClip {
            track: TrackId(2),
            clip: midi_clip(21),
        },
    );
    apply_checked(
        &mut model,
        Op::InsertPlugin {
            track: TrackId(2),
            index: 0,
            slot: slot(30),
        },
    );
    apply_checked(
        &mut model,
        Op::InsertPlugin {
            track: TrackId(2),
            index: 1,
            slot: slot(31),
        },
    );
    apply_checked(
        &mut model,
        Op::MovePlugin {
            track: TrackId(2),
            slot: SlotId(31),
            index: 0,
        },
    );
    apply_checked(
        &mut model,
        Op::SetPluginState {
            track: TrackId(2),
            slot: SlotId(30),
            state: None,
        },
    );
    apply_checked(
        &mut model,
        Op::SetPluginBypass {
            track: TrackId(2),
            slot: SlotId(31),
            bypass: true,
        },
    );
    for change in [
        TrackChange::Name("Lead".into()),
        TrackChange::Gain(0.5),
        TrackChange::Pan(-0.25),
        TrackChange::Mute(true),
        TrackChange::Solo(true),
        TrackChange::RecordArmed(true),
        TrackChange::Monitor(true),
        TrackChange::Input(TrackInput::Audio {
            first_channel: 0,
            channels: 2,
        }),
        TrackChange::Output(TrackOutput::Bus(TrackId(3))),
    ] {
        apply_checked(
            &mut model,
            Op::SetTrack {
                id: TrackId(1),
                change,
            },
        );
    }
    apply_checked(
        &mut model,
        Op::SetTrack {
            id: TrackId::MASTER,
            change: TrackChange::Gain(0.8),
        },
    );
    apply_checked(
        &mut model,
        Op::MoveTrack {
            id: TrackId(3),
            index: 0,
        },
    );
    let mut moved = audio_clip(20, 10, 24_000);
    moved.start = Position::Beats(Ticks::from_beats(8));
    apply_checked(
        &mut model,
        Op::ReplaceClip {
            track: TrackId(1),
            clip: moved,
        },
    );
    apply_checked(
        &mut model,
        Op::SetTempoMap {
            map: TempoMap::default().with_tempo(Ticks::from_beats(8), Tempo::from_bpm(90.0)),
        },
    );
    apply_checked(
        &mut model,
        Op::SetLoop {
            range: Some(LoopRange {
                start: Position::Beats(Ticks::from_beats(4)),
                end: Position::Beats(Ticks::from_beats(8)),
            }),
            enabled: true,
        },
    );
    apply_checked(
        &mut model,
        Op::SetMetronome {
            metronome: Metronome {
                enabled: true,
                gain: 0.7,
                count_in_bars: 2,
            },
        },
    );
    apply_checked(
        &mut model,
        Op::RemoveClip {
            track: TrackId(2),
            clip: ClipId(21),
        },
    );
    apply_checked(
        &mut model,
        Op::RemovePlugin {
            track: TrackId(2),
            slot: SlotId(30),
        },
    );
    apply_checked(&mut model, Op::RemoveTrack { id: TrackId(2) });

    assert_eq!(
        model.tracks.iter().map(|t| t.id.0).collect::<Vec<_>>(),
        vec![3, 1]
    );
    assert_eq!(model.track(TrackId(1)).unwrap().name, "Lead");
    assert_eq!(model.fresh_id(), 21);

    // The whole thing survives a trip through JSON, opaque plugin state included.
    let json = model.to_json();
    assert_eq!(Model::from_json(&json).unwrap(), model);
}

#[test]
fn removing_a_track_takes_its_content_and_undo_brings_it_back() {
    let mut model = Model::new();
    model
        .apply(&Op::AddMedia {
            media: media(1, 1000),
        })
        .unwrap();
    add_track(&mut model, 5, TrackKind::Audio);
    model
        .apply(&Op::AddClip {
            track: TrackId(5),
            clip: audio_clip(6, 1, 500),
        })
        .unwrap();
    model
        .apply(&Op::InsertPlugin {
            track: TrackId(5),
            index: 0,
            slot: slot(7),
        })
        .unwrap();
    let before = model.clone();
    let undo = model.apply(&Op::RemoveTrack { id: TrackId(5) }).unwrap();
    assert!(model.tracks.is_empty());
    // With the clip gone the media is free to go; with it back, not.
    assert!(model
        .clone()
        .apply(&Op::RemoveMedia { id: MediaId(1) })
        .is_ok());
    model.apply(&undo).unwrap();
    assert_eq!(model, before);
    assert_eq!(
        model.apply(&Op::RemoveMedia { id: MediaId(1) }),
        Err(OpError::MediaInUse {
            media: MediaId(1),
            clip: ClipId(6)
        })
    );
}

#[test]
fn refused_ops_leave_the_model_alone() {
    let mut model = Model::new();
    model
        .apply(&Op::AddMedia {
            media: media(1, 1000),
        })
        .unwrap();
    add_track(&mut model, 2, TrackKind::Audio);
    add_track(&mut model, 3, TrackKind::Instrument);
    add_track(&mut model, 4, TrackKind::Bus);
    add_track(&mut model, 5, TrackKind::Bus);
    model
        .apply(&Op::AddClip {
            track: TrackId(2),
            clip: audio_clip(9, 1, 1000),
        })
        .unwrap();
    let snapshot = model.clone();

    let track = |id, kind| Track::new(TrackId(id), "x", kind);
    let cases: Vec<(Op, OpError)> = vec![
        (
            Op::AddTrack {
                index: 0,
                track: track(2, TrackKind::Audio),
            },
            OpError::DuplicateTrack(TrackId(2)),
        ),
        (
            Op::AddTrack {
                index: 9,
                track: track(8, TrackKind::Audio),
            },
            OpError::IndexOutOfRange { index: 9, len: 4 },
        ),
        (
            Op::AddTrack {
                index: 0,
                track: track(8, TrackKind::Master),
            },
            OpError::MasterIsFixed,
        ),
        (
            Op::RemoveTrack {
                id: TrackId::MASTER,
            },
            OpError::MasterIsFixed,
        ),
        (
            Op::RemoveTrack { id: TrackId(77) },
            OpError::UnknownTrack(TrackId(77)),
        ),
        (
            Op::MoveTrack {
                id: TrackId(2),
                index: 4,
            },
            OpError::IndexOutOfRange { index: 4, len: 4 },
        ),
        (
            Op::SetTrack {
                id: TrackId(2),
                change: TrackChange::Gain(f32::NAN),
            },
            OpError::Invalid("track gain must be between 0 and 4"),
        ),
        (
            Op::SetTrack {
                id: TrackId(2),
                change: TrackChange::Pan(1.5),
            },
            OpError::Invalid("pan must be between -1 and 1"),
        ),
        (
            Op::SetTrack {
                id: TrackId(2),
                change: TrackChange::Output(TrackOutput::Bus(TrackId(3))),
            },
            OpError::NotABus(TrackId(3)),
        ),
        (
            Op::SetTrack {
                id: TrackId(2),
                change: TrackChange::Output(TrackOutput::Bus(TrackId(99))),
            },
            OpError::UnknownTrack(TrackId(99)),
        ),
        (
            Op::SetTrack {
                id: TrackId(4),
                change: TrackChange::Output(TrackOutput::Bus(TrackId(4))),
            },
            OpError::RoutingCycle(TrackId(4)),
        ),
        (
            Op::SetTrack {
                id: TrackId::MASTER,
                change: TrackChange::Output(TrackOutput::Master),
            },
            OpError::MasterIsFixed,
        ),
        (
            Op::AddClip {
                track: TrackId(2),
                clip: audio_clip(9, 1, 10),
            },
            OpError::DuplicateClip(ClipId(9)),
        ),
        (
            Op::AddClip {
                track: TrackId(2),
                clip: audio_clip(10, 55, 10),
            },
            OpError::UnknownMedia(MediaId(55)),
        ),
        (
            Op::AddClip {
                track: TrackId(2),
                clip: audio_clip(10, 1, 1001),
            },
            OpError::Invalid("the clip reaches past the end of its media"),
        ),
        (
            Op::AddClip {
                track: TrackId(3),
                clip: audio_clip(10, 1, 10),
            },
            OpError::WrongClipKind {
                track: TrackId(3),
                clip: ClipId(10),
            },
        ),
        (
            Op::AddClip {
                track: TrackId(2),
                clip: midi_clip(10),
            },
            OpError::WrongClipKind {
                track: TrackId(2),
                clip: ClipId(10),
            },
        ),
        (
            Op::RemoveClip {
                track: TrackId(3),
                clip: ClipId(9),
            },
            OpError::UnknownClip(ClipId(9)),
        ),
        (
            Op::ReplaceClip {
                track: TrackId(2),
                clip: audio_clip(11, 1, 10),
            },
            OpError::UnknownClip(ClipId(11)),
        ),
        (
            Op::AddMedia { media: media(1, 5) },
            OpError::DuplicateMedia(MediaId(1)),
        ),
        (
            Op::RemoveMedia { id: MediaId(1) },
            OpError::MediaInUse {
                media: MediaId(1),
                clip: ClipId(9),
            },
        ),
        (
            Op::RemovePlugin {
                track: TrackId(2),
                slot: SlotId(1),
            },
            OpError::UnknownSlot(SlotId(1)),
        ),
        (
            Op::InsertPlugin {
                track: TrackId(2),
                index: 1,
                slot: slot(1),
            },
            OpError::IndexOutOfRange { index: 1, len: 0 },
        ),
        (
            Op::SetLoop {
                range: Some(LoopRange {
                    start: Position::Beats(Ticks::from_beats(4)),
                    end: Position::Beats(Ticks::from_beats(4)),
                }),
                enabled: true,
            },
            OpError::Invalid("a loop must end after it starts"),
        ),
    ];
    for (op, expected) in cases {
        assert_eq!(model.apply(&op), Err(expected), "{op:?}");
        assert_eq!(
            model, snapshot,
            "{op:?} changed the model although it was refused"
        );
    }
}

#[test]
fn bus_routing_cannot_loop_and_buses_in_use_cannot_be_removed() {
    let mut model = Model::new();
    add_track(&mut model, 1, TrackKind::Audio);
    add_track(&mut model, 2, TrackKind::Bus);
    add_track(&mut model, 3, TrackKind::Bus);
    let route = |id, bus| Op::SetTrack {
        id: TrackId(id),
        change: TrackChange::Output(TrackOutput::Bus(TrackId(bus))),
    };
    model.apply(&route(1, 2)).unwrap();
    model.apply(&route(2, 3)).unwrap();
    assert_eq!(
        model.apply(&route(3, 2)),
        Err(OpError::RoutingCycle(TrackId(3)))
    );
    assert_eq!(
        model.apply(&Op::RemoveTrack { id: TrackId(3) }),
        Err(OpError::BusInUse {
            bus: TrackId(3),
            by: TrackId(2)
        })
    );
    // A track arriving already routed to a bus is validated the same way.
    let mut routed = Track::new(TrackId(4), "late", TrackKind::Audio);
    routed.output = TrackOutput::Bus(TrackId(1));
    assert_eq!(
        model.apply(&Op::AddTrack {
            index: 0,
            track: routed
        }),
        Err(OpError::NotABus(TrackId(1)))
    );
}

#[test]
fn batches_are_atomic_and_return_inverses_in_undo_order() {
    let mut model = Model::new();
    let before = model.clone();
    let good = vec![
        Op::AddTrack {
            index: 0,
            track: Track::new(TrackId(1), "a", TrackKind::Instrument),
        },
        Op::AddClip {
            track: TrackId(1),
            clip: midi_clip(2),
        },
        Op::SetTrack {
            id: TrackId(1),
            change: TrackChange::Gain(2.0),
        },
    ];
    let mut bad = good.clone();
    bad.push(Op::RemoveClip {
        track: TrackId(1),
        clip: ClipId(404),
    });
    assert_eq!(
        model.apply_all(&bad),
        Err((3, OpError::UnknownClip(ClipId(404))))
    );
    assert_eq!(model, before, "a failed batch must change nothing");

    let inverses = model.apply_all(&good).unwrap();
    assert_eq!(inverses.len(), 3);
    assert_ne!(model, before);
    for inverse in &inverses {
        model.apply(inverse).unwrap();
    }
    assert_eq!(model, before);
}

#[test]
fn snapshots_share_untouched_tracks() {
    let mut model = Model::new();
    add_track(&mut model, 1, TrackKind::Audio);
    add_track(&mut model, 2, TrackKind::Audio);
    let snapshot = model.clone();
    model
        .apply(&Op::SetTrack {
            id: TrackId(2),
            change: TrackChange::Mute(true),
        })
        .unwrap();
    assert!(
        Arc::ptr_eq(&model.tracks[0], &snapshot.tracks[0]),
        "untouched track was copied"
    );
    assert!(!Arc::ptr_eq(&model.tracks[1], &snapshot.tracks[1]));
    assert!(
        !snapshot.tracks[1].mute,
        "the snapshot must not see the edit"
    );
}

#[test]
fn files_from_other_schemas_or_with_broken_invariants_are_refused() {
    let json = Model::new().to_json();
    assert!(
        Model::from_json(&json.replace("\"schema\":1", "\"schema\":999"))
            .unwrap_err()
            .contains("schema 999")
    );
    assert!(Model::from_json("{").is_err());

    let mut model = Model::new();
    add_track(&mut model, 1, TrackKind::Audio);
    let mut broken: serde_json::Value = serde_json::from_str(&model.to_json()).unwrap();
    broken["tracks"][0]["output"] = serde_json::json!({ "type": "bus", "id": 1 });
    assert!(Model::from_json(&broken.to_string()).is_err());
    broken["tracks"][0]["output"] = serde_json::json!({ "type": "master" });
    broken["tracks"][0]["gain"] = serde_json::json!(99.0);
    assert!(Model::from_json(&broken.to_string()).is_err());
}

#[test]
fn ops_serialise_as_tagged_json() {
    let op = Op::SetTrack {
        id: TrackId(3),
        change: TrackChange::Gain(0.5),
    };
    let json = serde_json::to_string(&op).unwrap();
    assert_eq!(
        json,
        r#"{"op":"set_track","id":3,"change":{"field":"gain","value":0.5}}"#
    );
    assert_eq!(serde_json::from_str::<Op>(&json).unwrap(), op);
    let state = Op::SetPluginState {
        track: TrackId(1),
        slot: SlotId(2),
        state: Some(Arc::new(b"foobar".to_vec())),
    };
    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains("Zm9vYmFy"), "{json}");
    assert_eq!(serde_json::from_str::<Op>(&json).unwrap(), state);
}
