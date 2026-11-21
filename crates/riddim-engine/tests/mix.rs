//! Phase 3: sends, polarity, latency compensation through buses and sends, automation (read and
//! written), offline bounce, and plugin loading that does not stall the audio.

mod common;

use std::sync::Arc;

use common::*;
use riddim_disk::wav::{self, SampleFormat};
use riddim_engine::{
    bounce, BounceRequest, DeviceConfig, Engine, EngineConfig, Event, NullConfig, TransportCommand,
};
use riddim_model::time::{Position, Ticks};
use riddim_model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, CurveShape, LaneId,
    LoopRange, Model, Op, Send, SendId, SlotId, TrackChange, TrackId, TrackKind, TrackOutput,
};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

fn set(id: u64, change: TrackChange) -> Op {
    Op::SetTrack {
        id: TrackId(id),
        change,
    }
}

fn send(id: u64, target: u64, gain: f32, pre_fader: bool) -> Send {
    Send {
        id: SendId(id),
        target: TrackId(target),
        gain,
        pre_fader,
        mute: false,
    }
}

fn point(beats: (i64, i64), value: f32, shape: CurveShape) -> AutomationPoint {
    AutomationPoint {
        at: Ticks::from_beat_fraction(beats.0, beats.1),
        value,
        shape,
    }
}

fn lane(
    id: u64,
    target: AutomationTarget,
    mode: AutomationMode,
    points: Vec<AutomationPoint>,
) -> AutomationLane {
    AutomationLane {
        id: LaneId(id),
        target,
        mode,
        points: Arc::new(points),
    }
}

/// Deterministic noise in -0.5..0.5.
fn noise(frames: usize) -> Vec<f32> {
    let mut state = 0x2545_F491u32;
    (0..frames)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect()
}

fn dbfs(peak: f32) -> f32 {
    if peak == 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * peak.log10()
    }
}

fn play_from_start(engine: &mut Engine, frames: usize) -> Vec<Vec<f32>> {
    engine.transport(TransportCommand::Stop);
    engine.sync();
    // Let fader ramps finish and plugin and delay tails play out before starting again.
    engine.run(2048);
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(frames)
}

#[test]
fn sends_add_a_second_path_before_or_after_the_fader() {
    let dir = temp_dir("sends");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![0.4; 30_000]]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Bus),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 30_000),
            },
            Op::AddSend {
                track: TrackId(10),
                index: 0,
                send: send(30, 20, 0.5, false),
            },
        ])
        .unwrap();
    let level = |engine: &mut Engine, ops: Vec<Op>| {
        if !ops.is_empty() {
            engine.apply_wait(ops).unwrap();
        }
        engine.sync();
        if engine.status().transport == riddim_engine::TransportState::Stopped {
            engine.transport(TransportCommand::Play);
            engine.sync();
        }
        engine.run(1024)[0][1023]
    };
    let close = |got: f32, want: f32| assert!((got - want).abs() < 1e-6, "got {got}, want {want}");

    // Dry 0.4 plus the send at half gain through the bus.
    close(level(&mut engine, vec![]), 0.6);
    // A post-fader send follows the fader...
    close(
        level(&mut engine, vec![set(10, TrackChange::Gain(0.5))]),
        0.3,
    );
    // ...a pre-fader send does not.
    close(
        level(
            &mut engine,
            vec![Op::SetSend {
                track: TrackId(10),
                send: send(30, 20, 0.5, true),
            }],
        ),
        0.4,
    );
    // Pre-fader also means before mute: the track is silent but still feeds the bus.
    close(
        level(&mut engine, vec![set(10, TrackChange::Mute(true))]),
        0.2,
    );
    let mut muted = send(30, 20, 0.5, true);
    muted.mute = true;
    close(
        level(
            &mut engine,
            vec![Op::SetSend {
                track: TrackId(10),
                send: muted,
            }],
        ),
        0.0,
    );
    // The bus has its own fader, and polarity can be flipped.
    close(
        level(
            &mut engine,
            vec![
                set(10, TrackChange::Mute(false)),
                set(10, TrackChange::Gain(1.0)),
                Op::SetSend {
                    track: TrackId(10),
                    send: send(30, 20, 1.0, false),
                },
                set(20, TrackChange::Invert(true)),
            ],
        ),
        0.0,
    );
    close(
        level(&mut engine, vec![set(20, TrackChange::Gain(0.25))]),
        0.3,
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn solo_follows_the_signal_through_sends() {
    // A track at 0.5 with a post-fader send (x0.5) to an effect return, a second track at
    // 0.125 with a pre-fader send (x0.5) to the same return, and a third at 0.03125 that sends
    // nowhere. Powers of two, so every sum below is exact.
    let dir = temp_dir("solo-sends");
    let mut engine = engine(&dir);
    let dc = |id: u64, level: f32| media(&dir, id, RATE, vec![vec![level; 20_000]]);
    let clip = |track: u64, id: u64| Op::AddClip {
        track: TrackId(track),
        clip: audio_clip(id + 100, id, 0, 20_000),
    };
    let add_send = |track: u64, id: u64, pre_fader: bool| Op::AddSend {
        track: TrackId(track),
        index: 0,
        send: send(id, 40, 0.5, pre_fader),
    };
    engine
        .apply_wait(vec![
            dc(1, 0.5),
            dc(2, 0.125),
            dc(3, 0.03125),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            add_track(2, 30, TrackKind::Audio),
            add_track(3, 40, TrackKind::Bus),
            clip(10, 1),
            clip(20, 2),
            clip(30, 3),
            add_send(10, 50, false),
            add_send(20, 51, true),
        ])
        .unwrap();
    engine.sync();
    let level = |engine: &mut Engine, ops: Vec<Op>| {
        engine.apply_wait(ops).unwrap();
        engine.sync();
        let out = play_from_start(engine, 4096);
        assert_eq!(out[0][2000], out[1][2000]);
        out[0][2000]
    };
    let (dry, wet) = (0.5 + 0.125 + 0.03125, 0.25 + 0.0625);
    assert_eq!(level(&mut engine, vec![]), dry + wet);

    // Solo the return: the effect alone. The tracks that feed it are not heard dry, although
    // one of them has to keep its fader running for its post-fader send.
    assert_eq!(
        level(&mut engine, vec![set(40, TrackChange::Solo(true))]),
        wet
    );

    // Solo a track: itself and its share of the return, and nothing of the others.
    assert_eq!(
        level(
            &mut engine,
            vec![
                set(40, TrackChange::Solo(false)),
                set(10, TrackChange::Solo(true))
            ]
        ),
        0.5 + 0.25
    );
    assert_eq!(
        level(
            &mut engine,
            vec![
                set(10, TrackChange::Solo(false)),
                set(20, TrackChange::Solo(true))
            ]
        ),
        0.125 + 0.0625
    );
    // A track with no sends, soloed: the return has nothing to say.
    assert_eq!(
        level(
            &mut engine,
            vec![
                set(20, TrackChange::Solo(false)),
                set(30, TrackChange::Solo(true))
            ]
        ),
        0.03125
    );
    // Mute still wins over solo, and a muted track's post-fader send goes with it.
    assert_eq!(
        level(
            &mut engine,
            vec![
                set(30, TrackChange::Solo(false)),
                set(10, TrackChange::Solo(true)),
                set(10, TrackChange::Mute(true))
            ]
        ),
        0.0
    );
    // Back to no solo: everything again.
    assert_eq!(
        level(
            &mut engine,
            vec![
                set(10, TrackChange::Solo(false)),
                set(10, TrackChange::Mute(false))
            ]
        ),
        dry + wet
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn pdc_null_test_through_a_bus_and_through_a_send() {
    // Phase 3 exit criterion 2: a dry path and a latency-compensated path cancel to -120 dBFS
    // or below at the master, through sends and buses.
    let dir = temp_dir("null");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    let signal = noise(40_000);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![signal.clone()]),
            // Dry: straight to the master.
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 40_000),
            },
            // Wet: the same audio through a bus that carries a plugin with 64 samples of
            // latency, polarity flipped.
            add_track(1, 20, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(20),
                clip: audio_clip(21, 1, 0, 40_000),
            },
            add_track(2, 30, TrackKind::Bus),
            set(20, TrackChange::Output(TrackOutput::Bus(TrackId(30)))),
            Op::InsertPlugin {
                track: TrackId(30),
                index: 0,
                slot: fixture_slot(31, ids::DELAY_UID),
            },
            set(30, TrackChange::Invert(true)),
        ])
        .unwrap();
    wait_for(&events, "the delay to load", |e| {
        matches!(e, Event::PluginLoaded { .. }).then_some(())
    });
    engine.sync();

    let out = play_from_start(&mut engine, 30_000);
    assert_eq!(engine.status().plan_latency, 64);
    let residual = peak(&out[0]).max(peak(&out[1]));
    assert!(
        dbfs(residual) <= -120.0,
        "bus path: residual {} dBFS",
        dbfs(residual)
    );
    // Not a vacuous null: either path alone is loud.
    engine
        .apply_wait(vec![set(30, TrackChange::Mute(true))])
        .unwrap();
    assert!(peak(&play_from_start(&mut engine, 10_000)[0]) > 0.3);

    // The same cancellation with the wet path fed by a send instead of an output.
    engine
        .apply_wait(vec![
            set(30, TrackChange::Mute(false)),
            set(20, TrackChange::Mute(true)),
            Op::AddSend {
                track: TrackId(10),
                index: 0,
                send: send(40, 30, 1.0, false),
            },
        ])
        .unwrap();
    let out = play_from_start(&mut engine, 30_000);
    let residual = peak(&out[0]).max(peak(&out[1]));
    assert!(
        dbfs(residual) <= -120.0,
        "send path: residual {} dBFS",
        dbfs(residual)
    );

    // Bypassing the plugin must not shift the bus in time: still a null.
    engine
        .apply_wait(vec![Op::SetPluginBypass {
            track: TrackId(30),
            slot: SlotId(31),
            bypass: true,
        }])
        .unwrap();
    let out = play_from_start(&mut engine, 30_000);
    let residual = peak(&out[0]).max(peak(&out[1]));
    assert!(
        dbfs(residual) <= -120.0,
        "bypassed: residual {} dBFS",
        dbfs(residual)
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn automation_is_read_from_the_lanes() {
    let dir = temp_dir("autoread");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![1.0; 200_000]]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Bus),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 200_000),
            },
            // Fader: silent until beat 1, a straight line up to 1.0 at beat 2, hold, then a step
            // down to 0.25 at beat 3.
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    50,
                    AutomationTarget::TrackGain,
                    AutomationMode::Read,
                    vec![
                        point((1, 1), 0.0, CurveShape::Linear),
                        point((2, 1), 1.0, CurveShape::Hold),
                        point((3, 1), 0.25, CurveShape::Hold),
                    ],
                ),
            },
        ])
        .unwrap();
    let out = play_from_start(&mut engine, 100_000);
    let left = &out[0];
    // 120 bpm at 48 kHz: a beat is 24000 samples.
    assert!(
        left[..24_000].iter().all(|&s| s == 0.0),
        "before the first point its value holds"
    );
    for (sample, want) in [
        (24_001, 1.0 / 24_000.0),
        (30_000, 0.25),
        (36_000, 0.5),
        (42_000, 0.75),
    ] {
        assert!(
            (left[sample] - want).abs() < 1e-6,
            "at {sample}: {} (want {want})",
            left[sample]
        );
    }
    assert!(
        left[48_000..72_000].iter().all(|&s| s == 1.0),
        "the hold stays at 1.0"
    );
    // The step is on its sample exactly, although that sample is in the middle of a block.
    assert_eq!((left[71_999], left[72_000]), (1.0, 0.25));
    assert!(left[72_000..].iter().all(|&s| s == 0.25));
    // The ramp is a ramp, not a staircase.
    let step = left[24_000..48_000]
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(step < 1e-3, "largest step on the ramp was {step}");

    // A lane switched off stops driving its control; the fader's own value applies again.
    let model = engine.model();
    let mut off = (**model.track(TrackId(10)).unwrap().lane(LaneId(50)).unwrap()).clone();
    off.mode = AutomationMode::Off;
    engine
        .apply_wait(vec![Op::ReplaceLane {
            track: TrackId(10),
            lane: off,
        }])
        .unwrap();
    assert_eq!(play_from_start(&mut engine, 2000)[0][1999], 1.0);

    // Pan and send lanes.
    engine
        .apply_wait(vec![
            Op::AddSend {
                track: TrackId(10),
                index: 0,
                send: send(60, 20, 0.0, true),
            },
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    51,
                    AutomationTarget::TrackPan,
                    AutomationMode::Read,
                    vec![point((0, 1), -1.0, CurveShape::Hold)],
                ),
            },
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    52,
                    AutomationTarget::SendGain { send: SendId(60) },
                    AutomationMode::Read,
                    vec![point((0, 1), 0.5, CurveShape::Hold)],
                ),
            },
        ])
        .unwrap();
    let out = play_from_start(&mut engine, 2000);
    // Hard left on the track (1.0 left, 0 right) plus the send at 0.5 on both sides.
    assert!(
        (out[0][1999] - 1.5).abs() < 1e-6 && (out[1][1999] - 0.5).abs() < 1e-6,
        "{} {}",
        out[0][1999],
        out[1][1999]
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn plugin_parameters_follow_their_lanes() {
    let dir = temp_dir("autoplug");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![0.5; 100_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 100_000),
            },
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::GAIN_UID),
            },
            // The fixture gain is x(2 * value): unity at 0.5, then double from beat 1, then off.
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    50,
                    AutomationTarget::PluginParam {
                        slot: SlotId(12),
                        param: ids::GAIN_GAIN,
                    },
                    AutomationMode::Read,
                    vec![
                        point((0, 1), 0.5, CurveShape::Hold),
                        point((1, 1), 1.0, CurveShape::Hold),
                        point((2, 1), 0.0, CurveShape::Hold),
                    ],
                ),
            },
        ])
        .unwrap();
    engine.sync();
    let out = play_from_start(&mut engine, 60_000);
    let left = &out[0];
    assert_eq!(left[20_000], 0.5);
    // The fixture applies a parameter change from the start of the block it arrives in, so
    // this checks delivery to the plugin, not its sample accuracy.
    assert_eq!(
        left[24_000 + BLOCK],
        1.0,
        "the plugin received the new value in the block of the breakpoint"
    );
    assert_eq!(left[47_000], 1.0);
    assert_eq!(left[48_000 + BLOCK], 0.0);
    // A lane on a plugin cannot outlive its slot without saying so.
    assert!(engine
        .apply_wait(vec![Op::RemovePlugin {
            track: TrackId(10),
            slot: SlotId(12)
        }])
        .is_err());
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn touching_a_fader_writes_its_moves_into_the_lane() {
    let dir = temp_dir("autowrite");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    let flat = lane(
        50,
        AutomationTarget::TrackGain,
        AutomationMode::Touch,
        vec![
            point((0, 1), 1.0, CurveShape::Linear),
            point((16, 1), 1.0, CurveShape::Linear),
        ],
    );
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![1.0; 400_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 400_000),
            },
            Op::AddLane {
                track: TrackId(10),
                lane: flat,
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(48_000);

    // Grab the fader at one second, pull it down in two moves, let go at two seconds.
    engine.touch(TrackId(10), AutomationTarget::TrackGain, true);
    engine.sync();
    engine
        .apply_wait(vec![set(10, TrackChange::Gain(0.5))])
        .unwrap();
    let held = engine.run(24_000);
    assert!(
        (held[0][23_999] - 0.5).abs() < 1e-6,
        "while held, the fader's own value is heard"
    );
    engine
        .apply_wait(vec![set(10, TrackChange::Gain(0.25))])
        .unwrap();
    engine.run(24_000);
    engine.touch(TrackId(10), AutomationTarget::TrackGain, false);
    engine.sync();

    // The pass is committed as one edit, reported with its undo.
    let inverses = wait_for(&events, "the written automation", |e| match e {
        Event::ModelChanged { ops, inverses, .. } if matches!(ops[0], Op::ReplaceLane { .. }) => {
            Some(inverses.clone())
        }
        _ => None,
    });
    let after_release = engine.run(24_000);
    assert!(
        (after_release[0][23_999] - 1.0).abs() < 1e-6,
        "after release the lane is read again"
    );

    let model = engine.model();
    let written = model
        .track(TrackId(10))
        .unwrap()
        .lane(LaneId(50))
        .unwrap()
        .clone();
    let at = |beats: (i64, i64)| {
        written
            .value_at(Ticks::from_beat_fraction(beats.0, beats.1))
            .unwrap()
    };
    assert_eq!(at((1, 1)), 1.0, "before the pass the old curve stands");
    // Held at 0.5 for half a second, then moved: a flat line and a step, not a slide.
    assert_eq!(at((5, 2)), 0.5, "first move");
    assert_eq!(at((11, 4)), 0.5, "first move, held until the second");
    assert!(
        (at((7, 2)) - 0.25).abs() < 1e-6,
        "second move, got {}",
        at((7, 2))
    );
    assert_eq!(at((6, 1)), 1.0, "after the pass the old curve stands");

    // Playing it back reproduces the moves without anyone touching the fader. (The fader's own
    // value is still where it was left; the lane wins.)
    let out = play_from_start(&mut engine, 150_000);
    assert_eq!(out[0][20_000], 1.0);
    assert!((out[0][90_000] - 0.25).abs() < 1e-6);
    assert_eq!(out[0][140_000], 1.0);

    // And it can be undone.
    engine.apply_wait(inverses).unwrap();
    let restored = engine.model();
    assert_eq!(
        restored
            .track(TrackId(10))
            .unwrap()
            .lane(LaneId(50))
            .unwrap()
            .points
            .len(),
        2
    );
}

#[test]
fn latch_and_write_modes_record_until_the_transport_stops() {
    let dir = temp_dir("latch");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![0.5; 400_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 400_000),
            },
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::GAIN_UID),
            },
            // Latch on the plugin's gain; write on the track's pan.
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    50,
                    AutomationTarget::PluginParam {
                        slot: SlotId(12),
                        param: ids::GAIN_GAIN,
                    },
                    AutomationMode::Latch,
                    vec![],
                ),
            },
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    51,
                    AutomationTarget::TrackPan,
                    AutomationMode::Write,
                    vec![],
                ),
            },
        ])
        .unwrap();
    engine.sync();
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(24_000);
    engine.sync();
    // One movement of the knob latches the lane; one pan move is written.
    engine.set_param(SlotId(12), ids::GAIN_GAIN, 1.0);
    engine
        .apply_wait(vec![set(10, TrackChange::Pan(1.0))])
        .unwrap();
    engine.sync();
    engine.run(48_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    engine.sync();

    let mut seen = 0;
    while seen < 2 {
        wait_for(&events, "both lanes to be committed", |e| {
            matches!(e, Event::ModelChanged { ops, .. } if matches!(ops[0], Op::ReplaceLane { .. }))
                .then_some(())
        });
        seen += 1;
    }
    let model = engine.model();
    let track = model.track(TrackId(10)).unwrap();
    let knob = track.lane(LaneId(50)).unwrap();
    assert!(!knob.points.is_empty());
    assert_eq!(
        knob.value_at(Ticks::from_beats(2)),
        Some(1.0),
        "the latched value holds to the stop"
    );
    let pan = track.lane(LaneId(51)).unwrap();
    assert_eq!(
        pan.value_at(Ticks::ZERO),
        Some(0.0),
        "write mode records the control from the start"
    );
    assert_eq!(pan.value_at(Ticks::from_beats(2)), Some(1.0));
}

fn bounce_request(dir: &std::path::Path, name: &str, seconds: i64) -> BounceRequest {
    BounceRequest {
        path: dir.join(name),
        start: Position::START,
        end: at_sample(seconds * RATE as i64),
        tail_seconds: 0.0,
        sample_rate: RATE,
        block_frames: 256,
        format: SampleFormat::Float32,
        media_dir: dir.to_path_buf(),
        tracks: None,
    }
}

/// A project that uses most of the engine: instrument, audio through an effect with latency,
/// a bus, a send, automation.
fn demo_project(dir: &std::path::Path) -> Model {
    let mut model = Model::new();
    let ops = vec![
        media(dir, 1, RATE, vec![noise(96_000)]),
        add_track(0, 10, TrackKind::Instrument),
        add_track(1, 20, TrackKind::Audio),
        add_track(2, 30, TrackKind::Bus),
        Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: fixture_slot(12, ids::SYNTH_UID),
        },
        Op::InsertPlugin {
            track: TrackId(20),
            index: 0,
            slot: fixture_slot(22, ids::DELAY_UID),
        },
        Op::InsertPlugin {
            track: TrackId(30),
            index: 0,
            slot: fixture_slot(32, ids::GAIN_UID),
        },
        Op::AddClip {
            track: TrackId(10),
            clip: midi_clip(
                11,
                0,
                4,
                vec![
                    note((0, 1), (1, 1), 60),
                    note((1, 1), (1, 2), 64),
                    note((2, 1), (2, 1), 67),
                ],
            ),
        },
        Op::AddClip {
            track: TrackId(20),
            clip: audio_clip(21, 1, 0, 96_000),
        },
        Op::AddSend {
            track: TrackId(10),
            index: 0,
            send: send(40, 30, 0.5, false),
        },
        set(20, TrackChange::Gain(0.25)),
        Op::AddLane {
            track: TrackId(10),
            lane: lane(
                50,
                AutomationTarget::TrackPan,
                AutomationMode::Read,
                vec![
                    point((0, 1), -1.0, CurveShape::Linear),
                    point((4, 1), 1.0, CurveShape::Linear),
                ],
            ),
        },
    ];
    model.apply_all(&ops).unwrap();
    model
}

#[test]
fn an_automation_pass_does_not_span_a_loop_wrap() {
    // A write-mode lane over a two-beat loop (one second). The fader is moved once on each
    // time round; each round is its own pass, and the last one played is what the lane holds.
    let dir = temp_dir("loopwrite");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![1.0; 200_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 200_000),
            },
            Op::AddLane {
                track: TrackId(10),
                lane: lane(
                    50,
                    AutomationTarget::TrackGain,
                    AutomationMode::Write,
                    vec![
                        point((0, 1), 1.0, CurveShape::Linear),
                        point((8, 1), 1.0, CurveShape::Linear),
                    ],
                ),
            },
            Op::SetLoop {
                range: Some(LoopRange {
                    start: Position::START,
                    end: Position::Beats(Ticks::from_beats(2)),
                }),
                enabled: true,
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(1024);
    engine.sync();

    let passes = |events: &std::sync::mpsc::Receiver<Event>| {
        let mut count = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(&event, Event::ModelChanged { ops, .. } if matches!(ops[0], Op::ReplaceLane { .. }))
            {
                count += 1;
            }
        }
        count
    };
    // First time round: down to 0.5 at half a second.
    engine.run(24_000 - 1024);
    engine
        .apply_wait(vec![set(10, TrackChange::Gain(0.5))])
        .unwrap();
    engine.run(24_000 + 2048); // ... and over the wrap
    engine.sync();
    assert_eq!(passes(&events), 1, "the wrap ends the first pass");
    // Second time round: down to 0.25 at half a second, then stop before the loop ends.
    engine.run(24_000 - 2048);
    engine
        .apply_wait(vec![set(10, TrackChange::Gain(0.25))])
        .unwrap();
    engine.run(12_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(256);
    engine.sync();
    assert_eq!(passes(&events), 1, "stopping ends the second");

    let model = engine.model();
    let written = model
        .track(TrackId(10))
        .unwrap()
        .lane(LaneId(50))
        .unwrap()
        .clone();
    let at = |beats: (i64, i64)| {
        written
            .value_at(Ticks::from_beat_fraction(beats.0, beats.1))
            .unwrap()
    };
    // Time only ever goes forwards in a lane.
    assert!(
        written.points.windows(2).all(|w| w[0].at < w[1].at),
        "{:?}",
        written.points
    );
    // The second round: 0.5 (where the fader was left) from the loop start, 0.25 from half a
    // second on, as far as it was played (three quarters of a second: beat 1.5).
    assert_eq!(at((1, 2)), 0.5);
    assert_eq!(at((5, 4)), 0.25);
    // Past where the second round stopped, the first round's pass still stands (0.5 to the
    // loop end), and past the loop the lane is untouched.
    assert_eq!(at((7, 4)), 0.5);
    assert_eq!(at((4, 1)), 1.0);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn two_bounces_of_the_same_project_are_bit_identical() {
    // Phase 3 exit criterion 3.
    let dir = temp_dir("bounce");
    let model = demo_project(&dir);
    let first = bounce(&model, &bounce_request(&dir, "a.wav", 2)).unwrap();
    let second = bounce(&model, &bounce_request(&dir, "b.wav", 2)).unwrap();
    assert_eq!(first.frames, 96_000);
    assert!(first.peak > 0.05);
    assert_eq!(
        first.latency_trimmed, 64,
        "the delay plugin's latency is rendered and dropped"
    );
    assert!(
        first.missing_plugins.is_empty(),
        "{:?}",
        first.missing_plugins
    );
    assert_eq!((first.frames, first.peak), (second.frames, second.peak));
    let a = std::fs::read(dir.join("a.wav")).unwrap();
    let b = std::fs::read(dir.join("b.wav")).unwrap();
    assert!(a == b, "the two bounce files differ");

    // The bounce is the timeline, not the timeline plus latency: the synth's first note starts
    // on sample 0 (its first non-zero sample is 1), even though the plan is 64 samples late.
    let audio = wav::read(&dir.join("a.wav")).unwrap();
    let live_left_only = audio.channels[0].iter().position(|&s| s != 0.0);
    assert!(
        live_left_only.is_some_and(|at| at <= 1),
        "the bounce starts late: {live_left_only:?}"
    );

    // And it is what the live engine plays, sample for sample, once its latency is allowed for.
    // (Same block size as the bounce: plugins that apply parameters per block make that matter.)
    let mut engine = engine_with(
        &dir,
        NullConfig {
            buffer_frames: 256,
            ..NullConfig::default()
        },
    );
    engine.load(model).unwrap();
    engine.sync();
    let live = play_from_start(&mut engine, 96_000 + 64);
    assert_eq!(live[0][64..], audio.channels[0][..]);
    assert_eq!(live[1][64..], audio.channels[1][..]);
}

#[test]
fn stems_tails_other_block_sizes_and_missing_plugins() {
    let dir = temp_dir("stems");
    let model = demo_project(&dir);

    // A stem holds only its tracks; two complementary stems add up to the mix.
    let mix = bounce(&model, &bounce_request(&dir, "mix.wav", 2)).unwrap();
    let mut request = bounce_request(&dir, "synth.wav", 2);
    request.tracks = Some(vec![TrackId(10)]);
    bounce(&model, &request).unwrap();
    let mut request = bounce_request(&dir, "noise.wav", 2);
    request.tracks = Some(vec![TrackId(20)]);
    bounce(&model, &request).unwrap();
    let read = |name: &str| wav::read(&dir.join(name)).unwrap().channels;
    let (mix_audio, synth, noise_stem) = (read("mix.wav"), read("synth.wav"), read("noise.wav"));
    let worst = (0..mix.frames as usize)
        .map(|n| (mix_audio[0][n] - synth[0][n] - noise_stem[0][n]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst < 1e-6,
        "the stems do not add up to the mix (off by {worst})"
    );

    // A tail extends the file; a range picks a section.
    let mut request = bounce_request(&dir, "tail.wav", 1);
    request.tail_seconds = 0.5;
    assert_eq!(bounce(&model, &request).unwrap().frames, 72_000);
    let mut request = bounce_request(&dir, "section.wav", 2);
    request.start = at_sample(48_000);
    bounce(&model, &request).unwrap();
    assert_eq!(read("section.wav")[0][..], mix_audio[0][48_000..]);
    request.end = request.start;
    assert!(bounce(&model, &request).is_err());

    // A project with a plugin that is not installed still bounces, and says what was missing.
    let mut broken = model.clone();
    let mut ghost = fixture_slot(99, "00000000000000000000000000000000");
    ghost.plugin.path = Some("/nonexistent/Ghost.vst3".into());
    broken
        .apply(&Op::InsertPlugin {
            track: TrackId(20),
            index: 1,
            slot: ghost,
        })
        .unwrap();
    let result = bounce(&broken, &bounce_request(&dir, "broken.wav", 1)).unwrap();
    assert_eq!(result.missing_plugins.len(), 1);
    assert!(result.peak > 0.0);
}

#[test]
fn a_slow_loading_plugin_does_not_stall_the_audio() {
    // Phase 3 exit criterion 4, with a stand-in: the fixture takes 400 ms to initialise, as a
    // sampler loading its library would. Audio must keep running at its normal pace meanwhile.
    let dir = temp_dir("slowload");
    let mut config = EngineConfig::manual(&dir);
    // A timer-driven device this time: the point is what happens in real time.
    config.device = DeviceConfig::Null(NullConfig {
        buffer_frames: 64,
        ..NullConfig::default()
    });
    let engine = Engine::start(config).unwrap();
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![0.5; 480_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 480_000),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Ask for the slow plugin and watch the block counter while it loads.
    let started = std::time::Instant::now();
    engine.apply(vec![Op::InsertPlugin {
        track: TrackId(10),
        index: 0,
        slot: fixture_slot(12, ids::SLOW_UID),
    }]);
    // The engine keeps taking edits while the plugin loads: a fader move goes through at once.
    engine
        .apply_wait(vec![set(10, TrackChange::Gain(0.5))])
        .unwrap();
    let edit_took = started.elapsed();
    let mut samples = vec![(started.elapsed(), engine.status().blocks)];
    let mut loaded_after = None;
    while loaded_after.is_none() && started.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(10));
        samples.push((started.elapsed(), engine.status().blocks));
        while let Ok(event) = events.try_recv() {
            if matches!(
                event,
                Event::PluginLoaded {
                    slot: SlotId(12),
                    ..
                }
            ) {
                loaded_after = Some(started.elapsed());
            }
        }
    }
    let loaded_after = loaded_after.expect("the slow plugin never loaded");
    assert!(
        edit_took < std::time::Duration::from_millis(ids::SLOW_LOAD_MS / 2),
        "an edit made during the load took {edit_took:?}: it waited for the plugin"
    );
    assert!(
        loaded_after >= std::time::Duration::from_millis(ids::SLOW_LOAD_MS - 50),
        "the plugin loaded in {loaded_after:?}; it is supposed to be slow"
    );

    // While it loaded, the audio callback never went quiet: between any two looks at the
    // counter 10 ms apart (7.5 blocks at 64 frames), it had always moved on.
    let longest_stall = samples
        .windows(2)
        .filter(|w| w[1].1 == w[0].1)
        .map(|w| w[1].0 - w[0].0)
        .max();
    assert!(
        longest_stall.is_none_or(|gap| gap < std::time::Duration::from_millis(40)),
        "the audio callback stalled for {longest_stall:?} while a plugin was loading"
    );
    let blocks = samples.last().unwrap().1 - samples[0].1;
    let expected = loaded_after.as_secs_f64() * 48_000.0 / 64.0;
    assert!(
        blocks as f64 > expected * 0.7,
        "only {blocks} blocks ran in {loaded_after:?} (expected about {expected:.0})"
    );
    assert_eq!(engine.status().rt_violations, 0);
}
