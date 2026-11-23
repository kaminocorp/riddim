//! Playback through the engine on the deterministic manual device: clips land on the sample,
//! the transport does what it is told, the mixer adds up.

mod common;

use common::*;
use riddim_engine::{Event, TransportCommand, TransportState};
use riddim_model::time::{Position, Tempo, TempoMap, Ticks};
use riddim_model::{LoopRange, Metronome, Op, TrackChange, TrackId, TrackKind, TrackOutput};
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// A recognisable signal: sample `n` of channel `c` is `(n + 1) / 1000 * (c + 1)`.
fn ramp(frames: usize) -> Vec<Vec<f32>> {
    (0..2)
        .map(|c| {
            (0..frames)
                .map(|n| (n + 1) as f32 / 1000.0 * (c + 1) as f32)
                .collect()
        })
        .collect()
}

#[test]
fn an_idle_engine_plays_silence_and_reports_its_state() {
    let dir = temp_dir("idle");
    let mut engine = engine(&dir);
    engine.sync();
    let out = engine.run(1000);
    assert_eq!(out.len(), 2);
    assert!(out
        .iter()
        .all(|c| c.len() == 1000 && c.iter().all(|&s| s == 0.0)));
    let status = engine.status();
    assert_eq!(status.transport, TransportState::Stopped);
    assert_eq!(status.position_samples, 0);
    assert_eq!(
        (status.sample_rate, status.block_frames),
        (RATE, BLOCK as u32)
    );
    assert_eq!(
        status.blocks, 8,
        "1000 frames is seven full blocks and a short one"
    );
}

#[test]
fn an_audio_clip_plays_at_exactly_its_position() {
    assert!(riddim_rt::guard::allocator_installed());
    let dir = temp_dir("clip");
    let mut engine = engine(&dir);
    let signal = ramp(500);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, signal.clone()),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 1000, 500),
            },
        ])
        .unwrap();
    engine.sync();

    // Stopped: nothing plays, the position stays put.
    assert!(engine.run(256)[0].iter().all(|&s| s == 0.0));
    assert_eq!(engine.status().position_samples, 0);

    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(2000);
    assert_eq!(engine.status().transport, TransportState::Playing);
    assert_eq!(engine.status().position_samples, 2000);
    for (c, channel) in out.iter().enumerate() {
        assert!(
            channel[..1000].iter().all(|&s| s == 0.0),
            "sound before the clip"
        );
        assert_eq!(
            channel[1000..1500],
            signal[c][..],
            "channel {c}: the clip must play bit for bit"
        );
        assert!(
            channel[1500..].iter().all(|&s| s == 0.0),
            "sound after the clip"
        );
    }
    assert_eq!(
        engine.status().rt_violations,
        0,
        "the audio path touched the heap"
    );
}

#[test]
fn stop_locate_and_play_again() {
    let dir = temp_dir("transport");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, ramp(4000)),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 4000),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    let first = engine.run(300);
    assert_eq!(first[0][299], 0.3);

    engine.transport(TransportCommand::Stop);
    engine.sync();
    assert!(engine.run(128)[0].iter().all(|&s| s == 0.0));
    assert_eq!(
        engine.status().position_samples,
        300,
        "stopping keeps the playhead where it was"
    );

    engine.transport(TransportCommand::Locate(at_sample(2000)));
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(10);
    assert_eq!(out[0][0], 2.001);
    assert_eq!(engine.status().position_samples, 2010);

    // Locating while rolling jumps on the next block.
    engine.transport(TransportCommand::Locate(at_sample(100)));
    engine.sync();
    assert_eq!(engine.run(1)[0][0], 0.101);
}

#[test]
fn clips_anchored_to_beats_follow_the_tempo_map() {
    let dir = temp_dir("beats");
    let mut engine = engine(&dir);
    let mut clip = audio_clip(11, 1, 0, 100);
    clip.start = Position::Beats(Ticks::from_beats(2));
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, ramp(100)),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip,
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    // 120 bpm: beat 2 is one second in.
    let out = engine.run(50_000);
    assert_eq!(first_sound(&out[0]), Some(48_000));

    // At 60 bpm the same clip is two seconds in. Changing the tempo is just another op.
    engine.transport(TransportCommand::Stop);
    engine.transport(TransportCommand::Locate(Position::START));
    engine
        .apply_wait(vec![Op::SetTempoMap {
            map: TempoMap::new(Tempo::from_bpm(60.0), Default::default()),
        }])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(100_000);
    assert_eq!(first_sound(&out[0]), Some(96_000));
}

#[test]
fn clip_gain_fades_offsets_and_a_different_media_rate() {
    let dir = temp_dir("clipdetail");
    let mut engine = engine(&dir);
    let ones = vec![vec![1.0f32; 1000]];
    let mut faded = audio_clip(11, 1, 0, 400);
    if let riddim_model::ClipContent::Audio {
        offset,
        gain,
        fade_in,
        fade_out,
        ..
    } = &mut faded.content
    {
        *offset = 100;
        *gain = 0.5;
        *fade_in = 100;
        *fade_out = 200;
    }
    // 44.1 kHz media in a 48 kHz session: one second must still last one second.
    let tone: Vec<f32> = (0..44_100)
        .map(|n| (std::f32::consts::TAU * 441.0 * n as f32 / 44_100.0).sin())
        .collect();
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, ones),
            media(&dir, 2, 44_100, vec![tone]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: faded,
            },
            Op::AddClip {
                track: TrackId(20),
                clip: audio_clip(21, 2, 10_000, 44_100),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(60_000);
    let left = &out[0];
    // Mono media plays on both sides.
    assert_eq!(out[0][..400], out[1][..400]);
    assert_eq!(left[0], 0.0, "a fade-in starts from silence");
    assert!(
        (left[50] - 0.25).abs() < 1e-6,
        "halfway up the fade at half gain"
    );
    assert_eq!(left[150], 0.5, "between the fades the clip gain applies");
    assert!((left[300] - 0.25).abs() < 1e-6, "halfway down the fade-out");
    assert_eq!(left[400], 0.0);

    // The resampled tone: still 441 Hz, so 48000/441 samples per cycle, for exactly one second.
    let tone_out = &left[10_000..58_000];
    assert!(peak(&tone_out[500..47_500]) > 0.99);
    let expected = |n: usize| (std::f32::consts::TAU * 441.0 * n as f32 / 48_000.0).sin();
    for n in (1000..47_000).step_by(997) {
        assert!((tone_out[n] - expected(n)).abs() < 1e-3, "sample {n}");
    }
    assert!(
        peak(&left[58_100..]) < 1e-3,
        "the clip must end after one second"
    );
}

#[test]
fn looping_wraps_on_the_sample() {
    let dir = temp_dir("loop");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, ramp(1000)),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 1000),
            },
            Op::SetLoop {
                range: Some(LoopRange {
                    start: at_sample(200),
                    end: at_sample(500),
                }),
                enabled: true,
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(1100);
    let left = &out[0];
    // 0..500 plays straight through, then 200..500 repeats.
    for (n, sample) in left.iter().enumerate() {
        let timeline = if n < 500 { n } else { 200 + (n - 500) % 300 };
        assert_eq!(*sample, (timeline + 1) as f32 / 1000.0, "output sample {n}");
    }
    // 600 frames past the first wrap is exactly two more passes of the 300-frame loop.
    assert_eq!(engine.status().position_samples, 200);

    // Turning the loop off lets playback run on.
    engine
        .apply_wait(vec![Op::SetLoop {
            range: None,
            enabled: false,
        }])
        .unwrap();
    engine.sync();
    let position = engine.status().position_samples;
    engine.run(1000);
    assert_eq!(engine.status().position_samples, position + 1000);
}

#[test]
fn the_mixer_sums_routes_mutes_and_solos() {
    let dir = temp_dir("mixer");
    let mut engine = engine(&dir);
    let level = |value: f32| vec![vec![value; 20_000]];
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, level(0.1)),
            media(&dir, 2, RATE, level(0.2)),
            media(&dir, 3, RATE, level(0.4)),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            add_track(2, 30, TrackKind::Audio),
            add_track(3, 40, TrackKind::Bus),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 20_000),
            },
            Op::AddClip {
                track: TrackId(20),
                clip: audio_clip(21, 2, 0, 20_000),
            },
            Op::AddClip {
                track: TrackId(30),
                clip: audio_clip(31, 3, 0, 20_000),
            },
            // Tracks 20 and 30 go through the bus.
            Op::SetTrack {
                id: TrackId(20),
                change: TrackChange::Output(TrackOutput::Bus(TrackId(40))),
            },
            Op::SetTrack {
                id: TrackId(30),
                change: TrackChange::Output(TrackOutput::Bus(TrackId(40))),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();

    // Settled level after any gain ramp, both channels.
    let level_after = |engine: &mut riddim_engine::Engine, ops: Vec<Op>| -> (f32, f32) {
        if !ops.is_empty() {
            engine.apply_wait(ops).unwrap();
        }
        engine.sync();
        let out = engine.run(1024);
        (out[0][1023], out[1][1023])
    };
    let close = |got: (f32, f32), want: (f32, f32)| {
        assert!(
            (got.0 - want.0).abs() < 1e-6 && (got.1 - want.1).abs() < 1e-6,
            "got {got:?}, want {want:?}"
        );
    };
    let set = |id: u64, change: TrackChange| Op::SetTrack {
        id: TrackId(id),
        change,
    };

    close(level_after(&mut engine, vec![]), (0.7, 0.7));
    close(
        level_after(&mut engine, vec![set(40, TrackChange::Gain(0.5))]),
        (0.4, 0.4),
    );
    close(
        level_after(&mut engine, vec![set(10, TrackChange::Mute(true))]),
        (0.3, 0.3),
    );
    close(
        level_after(
            &mut engine,
            vec![
                set(10, TrackChange::Mute(false)),
                set(10, TrackChange::Pan(-1.0)),
            ],
        ),
        (0.4, 0.3),
    );
    close(
        level_after(
            &mut engine,
            vec![
                set(10, TrackChange::Pan(0.0)),
                set(0, TrackChange::Gain(2.0)),
            ],
        ),
        (0.8, 0.8),
    );
    // Solo a track that plays through the bus: the bus stays open, the others close.
    close(
        level_after(
            &mut engine,
            vec![
                set(0, TrackChange::Gain(1.0)),
                set(20, TrackChange::Solo(true)),
            ],
        ),
        (0.1, 0.1),
    );
    // Solo the bus instead: everything feeding it is heard, track 10 is not.
    close(
        level_after(
            &mut engine,
            vec![
                set(20, TrackChange::Solo(false)),
                set(40, TrackChange::Solo(true)),
            ],
        ),
        (0.3, 0.3),
    );
    close(
        level_after(
            &mut engine,
            vec![
                set(40, TrackChange::Solo(false)),
                set(0, TrackChange::Mute(true)),
            ],
        ),
        (0.0, 0.0),
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn gain_changes_ramp_instead_of_stepping() {
    let dir = temp_dir("ramp");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![1.0; 10_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 10_000),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    assert_eq!(engine.run(128)[0][127], 1.0);
    engine
        .apply_wait(vec![Op::SetTrack {
            id: TrackId(10),
            change: TrackChange::Gain(0.0),
        }])
        .unwrap();
    engine.sync();
    let out = engine.run(512);
    let largest_step = out[0]
        .windows(2)
        .map(|w| (w[0] - w[1]).abs())
        .fold(0.0f32, f32::max);
    assert!(largest_step < 0.01, "the fader jumped by {largest_step}");
    assert_eq!(out[0][511], 0.0);
}

#[test]
fn meters_and_transport_are_reported_as_events() {
    let dir = temp_dir("events");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![0.5; 48_000], vec![0.25; 48_000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 48_000),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(4096);
    wait_for(&events, "the transport to report playing", |e| {
        matches!(
            e,
            Event::Transport {
                state: TransportState::Playing,
                ..
            }
        )
        .then_some(())
    });
    let peaks = wait_for(&events, "a meter reading", |e| match e {
        Event::Meters { tracks } => tracks
            .iter()
            .find(|(id, peaks)| *id == TrackId(10) && peaks[0] > 0.0)
            .map(|t| t.1),
        _ => None,
    });
    assert_eq!(peaks, [0.5, 0.25]);
}

#[test]
fn the_metronome_clicks_on_every_beat_with_an_accent_on_the_bar() {
    let dir = temp_dir("click");
    let mut engine = engine(&dir);
    engine
        .apply_wait(vec![Op::SetMetronome {
            metronome: Metronome {
                enabled: true,
                gain: 0.5,
                count_in_bars: 0,
            },
        }])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(5 * 24_000);
    let left = &out[0];
    // 120 bpm at 48 kHz: a beat every 24000 samples. A click starts at sin(0), so the first
    // non-zero sample is one after the beat.
    let mut onsets = Vec::new();
    let mut n = 0;
    while n < left.len() {
        if left[n] != 0.0 {
            onsets.push(n);
            n += 4000; // skip past this click
        } else {
            n += 1;
        }
    }
    assert_eq!(onsets, vec![1, 24_001, 48_001, 72_001, 96_001]);
    // The bar-line click is an octave up: it crosses zero twice as often.
    let crossings = |from: usize| {
        left[from..from + 1000]
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count()
    };
    assert!(crossings(1) > crossings(24_001) * 3 / 2);
    assert_eq!(
        crossings(1),
        crossings(96_001),
        "the fifth beat starts bar two"
    );

    engine
        .apply_wait(vec![Op::SetMetronome {
            metronome: Metronome::default(),
        }])
        .unwrap();
    engine.sync();
    engine.run(4000);
    assert!(
        engine.run(24_000)[0].iter().all(|&s| s == 0.0),
        "the metronome was switched off"
    );
}

#[test]
fn edits_through_the_engine_are_undone_by_their_inverses() {
    let dir = temp_dir("undo");
    let engine = engine(&dir);
    let events = engine.subscribe();
    let empty = engine.model();
    let ticket = engine.apply(vec![
        add_track(0, 10, TrackKind::Instrument),
        Op::AddClip {
            track: TrackId(10),
            clip: midi_clip(11, 0, 4, vec![note((0, 1), (1, 1), 60)]),
        },
        Op::SetTrack {
            id: TrackId(10),
            change: TrackChange::Gain(0.25),
        },
    ]);
    let (revision, inverses) = wait_for(&events, "the batch to be applied", |e| match e {
        Event::Applied {
            ticket: t,
            revision,
            inverses,
        } if *t == ticket => Some((*revision, inverses.clone())),
        _ => None,
    });
    assert_eq!(revision, 1);
    let edited = engine.model();
    assert_eq!(edited.tracks.len(), 1);

    let redo = engine.apply_wait(inverses).unwrap();
    assert_eq!(
        *engine.model(),
        *empty,
        "the inverses must restore the model exactly"
    );
    engine.apply_wait(redo.inverses).unwrap();
    assert_eq!(*engine.model(), *edited);

    // A bad batch is refused as a whole and says which op was wrong.
    let rejected = engine
        .apply_wait(vec![
            add_track(1, 20, TrackKind::Audio),
            Op::RemoveTrack { id: TrackId(99) },
        ])
        .unwrap_err();
    assert_eq!(rejected.index, 1);
    assert_eq!(*engine.model(), *edited);
    assert_eq!(engine.status().model_revision, 3);
}
