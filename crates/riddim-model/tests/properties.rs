//! The phase 2 property: for any sequence of edits, applying an op and then its inverse gives
//! back an identical model, and re-applying gives the same result as the first time.

use std::sync::Arc;

use proptest::prelude::*;
use riddim_model::time::{Position, Superclock, Tempo, TempoMap, Ticks, TICKS_PER_BEAT};
use riddim_model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Clip, ClipContent, ClipId,
    CurveShape, LaneId, LoopRange, Media, MediaId, Metronome, Model, Note, Op, PluginOutput,
    PluginRef, PluginSlot, Send, SendId, Sidechain, SlotId, Track, TrackChange, TrackId,
    TrackInput, TrackKind, TrackOutput,
};

/// An edit described by raw numbers; `realise` turns it into an op that refers to things that
/// exist in the model at that moment (or, on purpose, sometimes do not).
#[derive(Clone, Debug)]
struct Seed {
    kind: u8,
    a: u64,
    b: u64,
    c: u64,
    x: f32,
}

fn seeds() -> impl Strategy<Value = Vec<Seed>> {
    prop::collection::vec(
        (
            0u8..36,
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            -0.5f32..4.5,
        )
            .prop_map(|(kind, a, b, c, x)| Seed { kind, a, b, c, x }),
        1..60,
    )
}

fn pick<T>(items: &[T], n: u64) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        Some(&items[(n % items.len() as u64) as usize])
    }
}

fn position(n: u64) -> Position {
    if n % 2 == 0 {
        Position::Beats(Ticks((n % 64) as i64 * TICKS_PER_BEAT / 4))
    } else {
        Position::Time(Superclock((n % 1000) as i64 * 5880 * 100))
    }
}

fn clip_for(track: &Track, model: &Model, id: u64, seed: &Seed) -> Option<Clip> {
    let content = match track.kind {
        TrackKind::Audio => {
            let media = pick(&model.media, seed.b)?;
            let length = 1 + seed.c % media.frames;
            let offset = seed.a % (media.frames - length + 1);
            ClipContent::Audio {
                media: media.id,
                offset,
                length,
                gain: seed.x.abs(),
                fade_in: length / 4,
                fade_out: length / 3,
            }
        }
        TrackKind::Instrument => {
            let notes = (0..seed.c % 5)
                .map(|i| Note {
                    start: Ticks(i as i64 * TICKS_PER_BEAT),
                    length: Ticks(TICKS_PER_BEAT / 2),
                    key: (40 + (seed.a + i) % 40) as u8,
                    velocity: 1 + ((seed.b + i) % 127) as u8,
                    channel: (i % 16) as u8,
                })
                .collect();
            ClipContent::Midi {
                length: Ticks::from_beats(1 + (seed.c % 8) as i64),
                notes: Arc::new(notes),
                events: Arc::new(Vec::new()),
            }
        }
        _ => return None,
    };
    Some(Clip {
        id: ClipId(id),
        name: format!("c{id}"),
        start: position(seed.a),
        muted: seed.b % 3 == 0,
        take: None,
        content,
    })
}

fn realise(seed: &Seed, model: &Model) -> Option<Op> {
    let fresh = model.fresh_id();
    let any_track = |n: u64| -> Option<Arc<Track>> { pick(&model.tracks, n).cloned() };
    Some(match seed.kind {
        0 | 1 => {
            let kind =
                [TrackKind::Audio, TrackKind::Instrument, TrackKind::Bus][(seed.a % 3) as usize];
            let mut track = Track::new(TrackId(fresh), format!("t{fresh}"), kind);
            track.gain = seed.x; // sometimes out of range on purpose
            Op::AddTrack {
                index: (seed.b % (model.tracks.len() as u64 + 2)) as usize,
                track,
            }
        }
        2 => Op::RemoveTrack {
            id: any_track(seed.a).map_or(TrackId(seed.a % 5), |t| t.id),
        },
        3 => Op::MoveTrack {
            id: any_track(seed.a)?.id,
            index: (seed.b % (model.tracks.len() as u64 + 1)) as usize,
        },
        4 => {
            let change = match seed.b % 9 {
                0 => TrackChange::Name(format!("n{}", seed.c % 100)),
                1 => TrackChange::Gain(seed.x),
                2 => TrackChange::Pan(seed.x - 1.5),
                3 => TrackChange::Mute(seed.c % 2 == 0),
                4 => TrackChange::Solo(seed.c % 2 == 0),
                5 => TrackChange::RecordArmed(seed.c % 2 == 0),
                6 => TrackChange::Monitor(seed.c % 2 == 0),
                7 => TrackChange::Input(match seed.c % 3 {
                    0 => TrackInput::None,
                    1 => TrackInput::Audio {
                        first_channel: (seed.c % 8) as u16,
                        channels: 1 + (seed.c % 3) as u8,
                    },
                    _ => TrackInput::Midi,
                }),
                _ => TrackChange::Output(match any_track(seed.c) {
                    Some(target) if seed.c % 4 != 0 => TrackOutput::Bus(target.id),
                    _ => TrackOutput::Master,
                }),
            };
            let id = if seed.a % 7 == 0 {
                TrackId::MASTER
            } else {
                any_track(seed.a)?.id
            };
            Op::SetTrack { id, change }
        }
        5..=7 => {
            let track = any_track(seed.a)?;
            Op::AddClip {
                track: track.id,
                clip: clip_for(&track, model, fresh, seed)?,
            }
        }
        8 => {
            let track = any_track(seed.a)?;
            Op::RemoveClip {
                track: track.id,
                clip: pick(&track.clips, seed.b).map_or(ClipId(seed.b % 9), |c| c.id),
            }
        }
        9 | 10 => {
            let track = any_track(seed.a)?;
            let existing = pick(&track.clips, seed.b)?.id;
            Op::ReplaceClip {
                track: track.id,
                clip: clip_for(&track, model, existing.0, seed)?,
            }
        }
        11 | 12 => Op::AddMedia {
            media: Media {
                id: MediaId(fresh),
                path: format!("m{fresh}.wav"),
                sample_rate: [44_100, 48_000, 96_000][(seed.a % 3) as usize],
                channels: 1 + (seed.b % 2) as u16,
                frames: 1 + seed.c % 1_000_000,
            },
        },
        13 => Op::RemoveMedia {
            id: pick(&model.media, seed.a).map_or(MediaId(seed.a % 9), |m| m.id),
        },
        14 | 15 => {
            let track = any_track(seed.a)?;
            Op::InsertPlugin {
                track: track.id,
                index: (seed.b % (track.plugins.len() as u64 + 2)) as usize,
                slot: PluginSlot {
                    id: SlotId(fresh),
                    plugin: PluginRef {
                        format: "vst3".into(),
                        uid: format!("{:X}", seed.c),
                        name: "p".into(),
                        path: None,
                    },
                    state: (seed.c % 2 == 0).then(|| Arc::new(seed.c.to_le_bytes().to_vec())),
                    bypass: seed.b % 5 == 0,
                    outputs: Vec::new(),
                    sidechain: None,
                },
            }
        }
        16 => {
            let track = any_track(seed.a)?;
            Op::RemovePlugin {
                track: track.id,
                slot: pick(&track.plugins, seed.b).map_or(SlotId(1), |s| s.id),
            }
        }
        17 => {
            let track = any_track(seed.a)?;
            let slot = pick(&track.plugins, seed.b)?.id;
            match seed.c % 3 {
                0 => Op::MovePlugin {
                    track: track.id,
                    slot,
                    index: (seed.c % (track.plugins.len() as u64 + 1)) as usize,
                },
                1 => Op::SetPluginState {
                    track: track.id,
                    slot,
                    state: Some(Arc::new(vec![seed.c as u8; (seed.c % 40) as usize])),
                },
                _ => Op::SetPluginBypass {
                    track: track.id,
                    slot,
                    bypass: seed.a % 2 == 0,
                },
            }
        }
        18 | 19 => Op::SetTempoMap {
            map: TempoMap::default().with_tempo(
                Ticks::from_beats(1 + (seed.a % 32) as i64),
                Tempo::from_bpm(40.0 + (seed.b % 200) as f64),
            ),
        },
        20 => Op::SetLoop {
            range: (seed.a % 4 != 0).then(|| LoopRange {
                start: position(seed.b),
                end: position(seed.c),
            }),
            enabled: seed.a % 2 == 0,
        },
        22 => Op::SetTrack {
            id: any_track(seed.a)?.id,
            change: TrackChange::Invert(seed.b % 2 == 0),
        },
        // Plugin outputs and sidechains: often valid, often not (a target that is not a bus, a
        // route that would loop). Both outcomes must leave the model sound.
        30..=32 => {
            let track = any_track(seed.a)?;
            let slot = pick(&track.plugins, seed.b)?.id;
            let outputs = (0..seed.c % 3)
                .map(|i| PluginOutput {
                    bus: (seed.b % 4 + i) as u8,
                    target: any_track(seed.c + i).map_or(TrackId(77), |t| t.id),
                })
                .collect();
            Op::SetPluginRouting {
                track: track.id,
                slot,
                outputs,
                sidechain: (seed.a % 3 != 0).then(|| Sidechain {
                    source: any_track(seed.b + 1).map_or(TrackId(78), |t| t.id),
                    pre_fader: seed.c % 2 == 0,
                }),
            }
        }
        33 | 34 => Op::SetTrack {
            id: any_track(seed.a)?.id,
            change: TrackChange::EventsFrom(
                (seed.b % 4 != 0).then(|| any_track(seed.c).map_or(TrackId(79), |t| t.id)),
            ),
        },
        35 => Op::SetPunch {
            range: (seed.a % 4 != 0).then(|| LoopRange {
                start: position(seed.b),
                end: position(seed.c),
            }),
            enabled: seed.a % 2 == 0,
        },
        23 | 24 => {
            let track = any_track(seed.a)?;
            Op::AddSend {
                track: track.id,
                index: (seed.b % (track.sends.len() as u64 + 2)) as usize,
                send: Send {
                    id: SendId(fresh),
                    // Often a real bus, sometimes not: both outcomes must leave the model sound.
                    target: any_track(seed.c).map_or(TrackId(77), |t| t.id),
                    gain: seed.x,
                    pre_fader: seed.c % 2 == 0,
                    mute: seed.c % 5 == 0,
                },
            }
        }
        25 => {
            let track = any_track(seed.a)?;
            match pick(&track.sends, seed.b) {
                Some(send) if seed.c % 2 == 0 => Op::RemoveSend {
                    track: track.id,
                    send: send.id,
                },
                Some(send) => Op::SetSend {
                    track: track.id,
                    send: Send {
                        target: any_track(seed.c).map_or(send.target, |t| t.id),
                        gain: seed.x.abs().min(4.0),
                        ..*send
                    },
                },
                None => Op::RemoveSend {
                    track: track.id,
                    send: SendId(seed.b % 9),
                },
            }
        }
        26 | 27 => {
            let track = any_track(seed.a)?;
            let target = match seed.b % 4 {
                0 => AutomationTarget::TrackGain,
                1 => AutomationTarget::TrackPan,
                2 => AutomationTarget::SendGain {
                    send: pick(&track.sends, seed.c).map_or(SendId(1), |s| s.id),
                },
                _ => AutomationTarget::PluginParam {
                    slot: pick(&track.plugins, seed.c).map_or(SlotId(1), |s| s.id),
                    param: (seed.c % 8) as u32,
                },
            };
            let (low, high) = target.range();
            let points = (0..seed.c % 5)
                .map(|i| AutomationPoint {
                    at: Ticks((i * 3 + seed.a % 3) as i64 * TICKS_PER_BEAT / 2),
                    value: low + (high - low) * ((seed.b + i) % 11) as f32 / 10.0,
                    shape: if (seed.a + i) % 2 == 0 {
                        CurveShape::Linear
                    } else {
                        CurveShape::Hold
                    },
                })
                .collect();
            let modes = [
                AutomationMode::Off,
                AutomationMode::Read,
                AutomationMode::Touch,
                AutomationMode::Latch,
                AutomationMode::Write,
            ];
            let lane = AutomationLane {
                id: LaneId(fresh),
                target,
                mode: modes[(seed.c % 5) as usize],
                points: Arc::new(points),
            };
            // Half the time, replace an existing lane's contents instead of adding a new one.
            match pick(&track.automation, seed.c) {
                Some(existing) if seed.a % 2 == 0 => Op::ReplaceLane {
                    track: track.id,
                    lane: AutomationLane {
                        id: existing.id,
                        target: existing.target,
                        ..lane
                    },
                },
                _ => Op::AddLane {
                    track: track.id,
                    lane,
                },
            }
        }
        28 => {
            let track = any_track(seed.a)?;
            Op::RemoveLane {
                track: track.id,
                lane: pick(&track.automation, seed.b).map_or(LaneId(seed.b % 9), |l| l.id),
            }
        }
        _ => Op::SetMetronome {
            metronome: Metronome {
                enabled: seed.a % 2 == 0,
                gain: seed.x,
                count_in_bars: (seed.b % 4) as u8,
            },
        },
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn every_op_is_undone_exactly_by_its_inverse(seeds in seeds()) {
        let mut model = Model::new();
        let mut applied = 0;
        let mut undo_stack = Vec::new();
        let origin = model.clone();

        for seed in &seeds {
            let Some(op) = realise(seed, &model) else { continue };
            let before = model.clone();
            match model.apply(&op) {
                Err(_) => prop_assert_eq!(&model, &before, "a refused op changed the model: {:?}", op),
                Ok(inverse) => {
                    applied += 1;
                    let after = model.clone();
                    prop_assert!(model.validate().is_ok(), "{:?} broke an invariant: {:?}", op, model.validate());

                    // apply -> inverse restores the model exactly...
                    let redo = model.apply(&inverse);
                    prop_assert!(redo.is_ok(), "the inverse of {:?} was refused: {:?}", op, redo);
                    prop_assert_eq!(&model, &before, "the inverse of {:?} did not restore the model", op);
                    // ...and -> apply again gives the identical result.
                    let again = model.apply(&redo.unwrap());
                    prop_assert!(again.is_ok());
                    prop_assert_eq!(&model, &after);
                    undo_stack.push(again.unwrap());
                }
            }
        }

        // The whole history unwinds to the empty project, and the final state survives JSON.
        let last = model.clone();
        let reloaded = Model::from_json(&last.to_json());
        prop_assert_eq!(reloaded.as_ref(), Ok(&last));
        while let Some(inverse) = undo_stack.pop() {
            prop_assert!(model.apply(&inverse).is_ok());
        }
        prop_assert_eq!(&model, &origin);
        prop_assume!(applied > 0);
    }

    #[test]
    fn batches_apply_all_or_nothing(seeds in seeds()) {
        // Build a batch against an evolving scratch model, then apply it in one go.
        let mut scratch = Model::new();
        let mut batch = Vec::new();
        for seed in &seeds {
            if let Some(op) = realise(seed, &scratch) {
                let _ = scratch.apply(&op);
                batch.push(op);
            }
        }
        let mut model = Model::new();
        let before = model.clone();
        match model.apply_all(&batch) {
            Err(_) => prop_assert_eq!(&model, &before),
            Ok(inverses) => {
                prop_assert_eq!(inverses.len(), batch.len());
                for inverse in &inverses {
                    prop_assert!(model.apply(inverse).is_ok());
                }
                prop_assert_eq!(&model, &before);
            }
        }
    }
}
