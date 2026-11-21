//! Ops against concrete cases: what each one does, what it refuses, and that inverses undo.

use std::sync::Arc;

use riddim_model::time::{Position, Superclock, Tempo, TempoMap, Ticks};
use riddim_model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Clip, ClipContent, ClipId,
    CurveShape, LaneId, LoopRange, Media, MediaId, Metronome, Model, Note, Op, OpError,
    PluginOutput, PluginRef, PluginSlot, Send, SendId, Sidechain, SlotId, Track, TrackChange,
    TrackId, TrackInput, TrackKind, TrackOutput,
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
        take: None,
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
        take: None,
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
        outputs: Vec::new(),
        sidechain: None,
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

fn send(id: u64, target: u64) -> Send {
    Send {
        id: SendId(id),
        target: TrackId(target),
        gain: 0.5,
        pre_fader: false,
        mute: false,
    }
}

fn point(beats: i64, value: f32, shape: CurveShape) -> AutomationPoint {
    AutomationPoint {
        at: Ticks::from_beats(beats),
        value,
        shape,
    }
}

fn lane(id: u64, target: AutomationTarget, points: Vec<AutomationPoint>) -> AutomationLane {
    AutomationLane {
        id: LaneId(id),
        target,
        mode: AutomationMode::Read,
        points: Arc::new(points),
    }
}

#[test]
fn sends_route_into_buses_and_cannot_close_a_loop() {
    let mut model = Model::new();
    add_track(&mut model, 1, TrackKind::Audio);
    add_track(&mut model, 2, TrackKind::Bus);
    add_track(&mut model, 3, TrackKind::Bus);
    apply_checked(
        &mut model,
        Op::AddSend {
            track: TrackId(1),
            index: 0,
            send: send(10, 2),
        },
    );
    apply_checked(
        &mut model,
        Op::AddSend {
            track: TrackId(1),
            index: 0,
            send: send(11, 3),
        },
    );
    assert_eq!(
        model
            .track(TrackId(1))
            .unwrap()
            .sends
            .iter()
            .map(|s| s.id.0)
            .collect::<Vec<_>>(),
        vec![11, 10]
    );
    let mut louder = send(10, 3);
    louder.gain = 2.0;
    louder.pre_fader = true;
    apply_checked(
        &mut model,
        Op::SetSend {
            track: TrackId(1),
            send: louder,
        },
    );
    apply_checked(
        &mut model,
        Op::SetTrack {
            id: TrackId(1),
            change: TrackChange::Invert(true),
        },
    );
    assert_eq!(model.fresh_id(), 12);

    let snapshot = model.clone();
    let refused: Vec<(Op, OpError)> = vec![
        (
            Op::AddSend {
                track: TrackId(1),
                index: 0,
                send: send(10, 2),
            },
            OpError::DuplicateSend(SendId(10)),
        ),
        (
            Op::AddSend {
                track: TrackId(2),
                index: 0,
                send: send(20, 1),
            },
            OpError::NotABus(TrackId(1)),
        ),
        (
            Op::AddSend {
                track: TrackId(2),
                index: 0,
                send: send(20, 2),
            },
            OpError::RoutingCycle(TrackId(2)),
        ),
        (
            Op::AddSend {
                track: TrackId(2),
                index: 0,
                send: send(20, 9),
            },
            OpError::UnknownTrack(TrackId(9)),
        ),
        (
            Op::AddSend {
                track: TrackId(2),
                index: 3,
                send: send(20, 3),
            },
            OpError::IndexOutOfRange { index: 3, len: 0 },
        ),
        (
            Op::AddSend {
                track: TrackId::MASTER,
                index: 0,
                send: send(20, 2),
            },
            OpError::MasterIsFixed,
        ),
        (
            Op::RemoveSend {
                track: TrackId(2),
                send: SendId(10),
            },
            OpError::UnknownSend(SendId(10)),
        ),
        (
            Op::RemoveTrack { id: TrackId(3) },
            OpError::BusInUse {
                bus: TrackId(3),
                by: TrackId(1),
            },
        ),
    ];
    for (op, expected) in refused {
        assert_eq!(model.apply(&op), Err(expected), "{op:?}");
        assert_eq!(model, snapshot);
    }

    // A loop that only exists through a send: bus 2 outputs to bus 3, bus 3 sends to bus 2.
    model
        .apply(&Op::SetTrack {
            id: TrackId(2),
            change: TrackChange::Output(TrackOutput::Bus(TrackId(3))),
        })
        .unwrap();
    assert!(matches!(
        model.apply(&Op::AddSend {
            track: TrackId(3),
            index: 0,
            send: send(30, 2)
        }),
        Err(OpError::RoutingCycle(_))
    ));
    // And the other way round: the send exists first, then the output would close the loop.
    model
        .apply(&Op::SetTrack {
            id: TrackId(2),
            change: TrackChange::Output(TrackOutput::Master),
        })
        .unwrap();
    model
        .apply(&Op::AddSend {
            track: TrackId(3),
            index: 0,
            send: send(30, 2),
        })
        .unwrap();
    assert!(matches!(
        model.apply(&Op::SetTrack {
            id: TrackId(2),
            change: TrackChange::Output(TrackOutput::Bus(TrackId(3)))
        }),
        Err(OpError::RoutingCycle(_))
    ));
    model.validate().unwrap();
    assert_eq!(Model::from_json(&model.to_json()).unwrap(), model);
}

#[test]
fn automation_lanes_follow_their_targets() {
    let mut model = Model::new();
    add_track(&mut model, 1, TrackKind::Instrument);
    add_track(&mut model, 2, TrackKind::Bus);
    model
        .apply(&Op::InsertPlugin {
            track: TrackId(1),
            index: 0,
            slot: slot(5),
        })
        .unwrap();
    model
        .apply(&Op::AddSend {
            track: TrackId(1),
            index: 0,
            send: send(6, 2),
        })
        .unwrap();

    let gain = lane(
        10,
        AutomationTarget::TrackGain,
        vec![
            point(0, 0.0, CurveShape::Linear),
            point(4, 1.0, CurveShape::Hold),
            point(8, 0.5, CurveShape::Linear),
        ],
    );
    apply_checked(
        &mut model,
        Op::AddLane {
            track: TrackId(1),
            lane: gain.clone(),
        },
    );
    apply_checked(
        &mut model,
        Op::AddLane {
            track: TrackId(1),
            lane: lane(
                11,
                AutomationTarget::PluginParam {
                    slot: SlotId(5),
                    param: 7,
                },
                vec![point(1, 0.25, CurveShape::Hold)],
            ),
        },
    );
    apply_checked(
        &mut model,
        Op::AddLane {
            track: TrackId(1),
            lane: lane(9, AutomationTarget::SendGain { send: SendId(6) }, vec![]),
        },
    );
    assert_eq!(
        model
            .track(TrackId(1))
            .unwrap()
            .automation
            .iter()
            .map(|l| l.id.0)
            .collect::<Vec<_>>(),
        vec![9, 10, 11]
    );

    let mut touched = gain.clone();
    touched.mode = AutomationMode::Touch;
    touched.points = Arc::new(vec![point(2, 2.0, CurveShape::Linear)]);
    apply_checked(
        &mut model,
        Op::ReplaceLane {
            track: TrackId(1),
            lane: touched,
        },
    );
    apply_checked(
        &mut model,
        Op::RemoveLane {
            track: TrackId(1),
            lane: LaneId(9),
        },
    );

    let snapshot = model.clone();
    let pan_on_gain_lane = AutomationLane {
        target: AutomationTarget::TrackPan,
        ..gain.clone()
    };
    let refused: Vec<(Op, OpError)> = vec![
        (
            Op::AddLane {
                track: TrackId(1),
                lane: lane(10, AutomationTarget::TrackPan, vec![]),
            },
            OpError::DuplicateLane(LaneId(10)),
        ),
        (
            Op::AddLane {
                track: TrackId(1),
                lane: lane(20, AutomationTarget::TrackGain, vec![]),
            },
            OpError::TargetAlreadyAutomated(LaneId(10)),
        ),
        (
            Op::AddLane {
                track: TrackId(1),
                lane: lane(
                    20,
                    AutomationTarget::PluginParam {
                        slot: SlotId(99),
                        param: 0,
                    },
                    vec![],
                ),
            },
            OpError::UnknownSlot(SlotId(99)),
        ),
        (
            Op::AddLane {
                track: TrackId(2),
                lane: lane(20, AutomationTarget::SendGain { send: SendId(6) }, vec![]),
            },
            OpError::UnknownSend(SendId(6)),
        ),
        (
            Op::AddLane {
                track: TrackId(2),
                lane: lane(
                    20,
                    AutomationTarget::TrackPan,
                    vec![point(0, 1.5, CurveShape::Linear)],
                ),
            },
            OpError::Invalid("an automation value is out of range for its control"),
        ),
        (
            Op::AddLane {
                track: TrackId(2),
                lane: lane(
                    20,
                    AutomationTarget::TrackGain,
                    vec![
                        point(4, 1.0, CurveShape::Linear),
                        point(4, 0.5, CurveShape::Linear),
                    ],
                ),
            },
            OpError::Invalid("automation points must be in strictly increasing order"),
        ),
        (
            Op::ReplaceLane {
                track: TrackId(1),
                lane: pan_on_gain_lane,
            },
            OpError::Invalid("a lane's target cannot change"),
        ),
        (
            Op::RemoveLane {
                track: TrackId(2),
                lane: LaneId(10),
            },
            OpError::UnknownLane(LaneId(10)),
        ),
        // What a lane points at cannot be pulled out from under it.
        (
            Op::RemovePlugin {
                track: TrackId(1),
                slot: SlotId(5),
            },
            OpError::Automated(LaneId(11)),
        ),
    ];
    for (op, expected) in refused {
        assert_eq!(model.apply(&op), Err(expected), "{op:?}");
        assert_eq!(model, snapshot);
    }
    assert_eq!(Model::from_json(&model.to_json()).unwrap(), model);
}

#[test]
fn lane_values_interpolate_hold_and_extend() {
    let ramp = lane(
        1,
        AutomationTarget::TrackGain,
        vec![
            point(4, 0.0, CurveShape::Linear),
            point(8, 2.0, CurveShape::Hold),
            point(12, 1.0, CurveShape::Linear),
        ],
    );
    let at = |beats: (i64, i64)| ramp.value_at(Ticks::from_beat_fraction(beats.0, beats.1));
    assert_eq!(
        at((0, 1)),
        Some(0.0),
        "before the first point its value holds"
    );
    assert_eq!(at((4, 1)), Some(0.0));
    assert_eq!(at((6, 1)), Some(1.0), "halfway up a linear segment");
    assert_eq!(at((7, 1)), Some(1.5));
    assert_eq!(at((8, 1)), Some(2.0));
    assert_eq!(
        at((23, 2)),
        Some(2.0),
        "a hold stays put until the next point"
    );
    assert_eq!(at((12, 1)), Some(1.0));
    assert_eq!(
        at((100, 1)),
        Some(1.0),
        "after the last point its value holds"
    );
    assert_eq!(
        lane(2, AutomationTarget::TrackPan, vec![]).value_at(Ticks::ZERO),
        None
    );
}

#[test]
fn plugin_outputs_sidechains_and_note_routing_are_validated_and_undoable() {
    let mut model = Model::new();
    let add = |model: &mut Model, id: u64, kind: TrackKind| {
        let index = model.tracks.len();
        model
            .apply(&Op::AddTrack {
                index,
                track: Track::new(TrackId(id), format!("t{id}"), kind),
            })
            .unwrap();
    };
    add(&mut model, 1, TrackKind::Instrument); // a multi-out sampler
    add(&mut model, 2, TrackKind::Bus); // its second output
    add(&mut model, 3, TrackKind::Audio); // a kick, say
    add(&mut model, 4, TrackKind::Instrument); // plays the sampler's MIDI out
    model
        .apply(&Op::InsertPlugin {
            track: TrackId(1),
            index: 0,
            slot: slot(10),
        })
        .unwrap();
    model
        .apply(&Op::InsertPlugin {
            track: TrackId(2),
            index: 0,
            slot: slot(20),
        })
        .unwrap();
    let before = model.clone();

    // Route output 1 of the sampler to the bus, and key the bus's plugin from the audio track.
    let route = |outputs: Vec<PluginOutput>, sidechain: Option<Sidechain>| Op::SetPluginRouting {
        track: TrackId(1),
        slot: SlotId(10),
        outputs,
        sidechain,
    };
    let to_bus = PluginOutput {
        bus: 1,
        target: TrackId(2),
    };
    let undo_route = model.apply(&route(vec![to_bus], None)).unwrap();
    let undo_key = model
        .apply(&Op::SetPluginRouting {
            track: TrackId(2),
            slot: SlotId(20),
            outputs: vec![],
            sidechain: Some(Sidechain {
                source: TrackId(3),
                pre_fader: true,
            }),
        })
        .unwrap();
    let undo_notes = model
        .apply(&Op::SetTrack {
            id: TrackId(4),
            change: TrackChange::EventsFrom(Some(TrackId(1))),
        })
        .unwrap();
    assert_eq!(
        model.track(TrackId(1)).unwrap().plugins[0].outputs,
        vec![to_bus]
    );
    assert_eq!(
        model.track(TrackId(4)).unwrap().events_from,
        Some(TrackId(1))
    );
    model.validate().unwrap();
    // It survives the trip through JSON.
    assert_eq!(Model::from_json(&model.to_json()).unwrap(), model);

    // What is refused: output 0 (that is the main output), the same output twice, a target
    // that is not a bus, a route back into the plugin's own track.
    let refused = |model: &mut Model, op: Op| {
        let snapshot = model.clone();
        let error = model.apply(&op).unwrap_err();
        assert_eq!(*model, snapshot, "a refused op changed the model");
        error
    };
    let out = |bus: u8, target: u64| PluginOutput {
        bus,
        target: TrackId(target),
    };
    assert!(matches!(
        refused(&mut model, route(vec![out(0, 2)], None)),
        OpError::Invalid(_)
    ));
    assert!(matches!(
        refused(&mut model, route(vec![out(1, 2), out(1, 2)], None)),
        OpError::Invalid(_)
    ));
    assert_eq!(
        refused(&mut model, route(vec![out(1, 3)], None)),
        OpError::NotABus(TrackId(3))
    );
    assert_eq!(
        refused(&mut model, route(vec![out(1, 9)], None)),
        OpError::UnknownTrack(TrackId(9))
    );
    // Loops through the new kinds of connection are loops all the same: the bus (fed by the
    // sampler's output) cannot key the sampler, and the sampler cannot take notes from track 4
    // (which takes notes from it).
    let key_from_bus = Some(Sidechain {
        source: TrackId(2),
        pre_fader: false,
    });
    assert!(matches!(
        refused(&mut model, route(vec![to_bus], key_from_bus)),
        OpError::RoutingCycle(_)
    ));
    assert!(matches!(
        refused(
            &mut model,
            Op::SetTrack {
                id: TrackId(1),
                change: TrackChange::EventsFrom(Some(TrackId(4))),
            }
        ),
        OpError::RoutingCycle(_)
    ));
    // Only instruments take notes.
    assert!(matches!(
        refused(
            &mut model,
            Op::SetTrack {
                id: TrackId(3),
                change: TrackChange::EventsFrom(Some(TrackId(1))),
            }
        ),
        OpError::Invalid(_)
    ));
    // A track that something listens to cannot be removed from under it.
    for id in [1, 2, 3] {
        assert!(
            matches!(
                refused(&mut model, Op::RemoveTrack { id: TrackId(id) }),
                OpError::BusInUse { .. }
            ),
            "track {id}"
        );
    }
    // Removing the plugin takes its routing with it, and undoing brings both back.
    let undo_remove = model
        .apply(&Op::RemovePlugin {
            track: TrackId(1),
            slot: SlotId(10),
        })
        .unwrap();
    model.apply(&undo_remove).unwrap();
    assert_eq!(
        model.track(TrackId(1)).unwrap().plugins[0].outputs,
        vec![to_bus]
    );

    for undo in [undo_notes, undo_key, undo_route] {
        model.apply(&undo).unwrap();
    }
    assert_eq!(model, before);
}

#[test]
fn punch_ranges_and_takes() {
    let mut model = Model::new();
    let range = |start: i64, end: i64| LoopRange {
        start: Position::Beats(Ticks::from_beats(start)),
        end: Position::Beats(Ticks::from_beats(end)),
    };
    assert!(model
        .apply(&Op::SetPunch {
            range: Some(range(4, 4)),
            enabled: true
        })
        .is_err());
    let undo = model
        .apply(&Op::SetPunch {
            range: Some(range(4, 8)),
            enabled: true,
        })
        .unwrap();
    assert!(model.punch_enabled);
    assert_eq!(Model::from_json(&model.to_json()).unwrap(), model);
    model.apply(&undo).unwrap();
    assert_eq!(model, Model::new());

    // A model saved before takes, punch and plugin routing existed still loads.
    let old = r#"{"schema":1,"tempo_map":TEMPO,"master":MASTER,"tracks":[],"media":[],
        "loop_range":null,"loop_enabled":false,"metronome":{"enabled":false,"gain":0.5,"count_in_bars":0}}"#;
    let current: serde_json::Value = serde_json::from_str(&Model::new().to_json()).unwrap();
    let mut master = current["master"].clone();
    master.as_object_mut().unwrap().remove("events_from");
    let old = old
        .replace("TEMPO", &current["tempo_map"].to_string())
        .replace("MASTER", &master.to_string());
    assert_eq!(Model::from_json(&old).unwrap(), Model::new());
}
