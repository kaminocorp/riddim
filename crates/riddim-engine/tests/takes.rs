//! Loop recording (a take per time round) and punch recording (a take confined to a range).

mod common;

use common::*;
use riddim_disk::wav;
use riddim_engine::{Engine, Event, NullConfig, TransportCommand, TransportState};
use riddim_model::time::{Position, Ticks, TICKS_PER_BEAT};
use riddim_model::{
    ClipContent, ClipId, LoopRange, MediaId, Op, Take, TrackChange, TrackId, TrackInput, TrackKind,
};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

const ROUND_TRIP: u32 = 300;
/// One beat at 120 bpm.
const BEAT: i64 = 24_000;

/// A device whose output is wired back to its input, reporting the delay honestly.
fn loopback() -> NullConfig {
    NullConfig {
        loopback_delay: Some(ROUND_TRIP),
        reported_input_latency: ROUND_TRIP / 3,
        reported_output_latency: ROUND_TRIP - ROUND_TRIP / 3,
        ..NullConfig::default()
    }
}

fn set(id: u64, change: TrackChange) -> Op {
    Op::SetTrack {
        id: TrackId(id),
        change,
    }
}

fn beats(start: i64, end: i64) -> LoopRange {
    LoopRange {
        start: Position::Beats(Ticks::from_beats(start)),
        end: Position::Beats(Ticks::from_beats(end)),
    }
}

/// What the guide track plays at timeline sample `n`: a value that says where it is.
fn code(n: i64) -> f32 {
    (n + 1) as f32 / 262_144.0
}

/// Track 10 plays a signal that encodes the timeline position; track 20 is armed to record
/// the device input, which is the output come back round.
fn coded_session(engine: &Engine, dir: &std::path::Path) {
    let guide: Vec<f32> = (0..200_000).map(code).collect();
    engine
        .apply_wait(vec![
            media(dir, 1, RATE, vec![guide]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 200_000),
            },
            set(
                20,
                TrackChange::Input(TrackInput::Audio {
                    first_channel: 0,
                    channels: 1,
                }),
            ),
            set(20, TrackChange::RecordArmed(true)),
        ])
        .unwrap();
    engine.sync();
}

/// Collects the takes reported once recording has ended, and the undo for the engine's edit.
#[allow(clippy::type_complexity)]
fn finished(
    events: &std::sync::mpsc::Receiver<Event>,
    track: u64,
    count: usize,
) -> (Vec<(ClipId, Option<MediaId>, u64, Option<u32>)>, Vec<Op>) {
    let mut takes = Vec::new();
    let mut undo = Vec::new();
    while takes.len() < count {
        wait_for(events, "the takes to be finished", |e| match e {
            Event::ModelChanged { inverses, .. } => {
                undo = inverses.clone();
                None
            }
            Event::RecordingFinished {
                track: t,
                clip,
                media,
                frames,
                take,
            } if *t == TrackId(track) => {
                takes.push((*clip, *media, *frames, *take));
                Some(())
            }
            Event::Error { message } => panic!("the engine reported: {message}"),
            _ => None,
        });
    }
    (takes, undo)
}

#[test]
fn loop_recording_makes_a_take_of_every_time_round() {
    let dir = temp_dir("looprec");
    let mut engine = engine_with(&dir, loopback());
    let events = engine.subscribe();
    coded_session(&engine, &dir);
    // Loop the second beat. Recording starts at the top, so the first time through runs from
    // 0 to the loop's end, and every later one is the loop itself.
    engine
        .apply_wait(vec![Op::SetLoop {
            range: Some(beats(1, 2)),
            enabled: true,
        }])
        .unwrap();
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Record);
    engine.sync();
    // Twice round after the first pass, and 10,000 frames into a fourth.
    let rolled = 2 * BEAT + 2 * BEAT + 10_000;
    engine.run(rolled as usize);
    assert_eq!(engine.status().transport, TransportState::Recording);
    assert_eq!(
        engine.status().position_samples,
        BEAT + 10_000,
        "the loop kept wrapping"
    );
    // The transport cannot be moved in the middle of a take.
    engine.transport(TransportCommand::Locate(Position::START));
    engine.sync();
    engine.run(BLOCK);
    assert_eq!(
        engine.status().position_samples,
        BEAT + 10_000 + BLOCK as i64
    );
    let refusal = wait_for(&events, "the refusal to locate", |e| match e {
        Event::Error { message } => Some(message.clone()),
        _ => None,
    });
    assert!(refusal.contains("while recording"), "{refusal}");
    let rolled = rolled + BLOCK as i64;
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);

    let (takes, undo) = finished(&events, 20, 4);
    // Four takes, numbered, all cut from one file.
    let media = takes[0].1.expect("an audio take has media");
    assert!(takes.iter().all(|t| t.1 == Some(media)));
    assert_eq!(
        takes.iter().map(|t| t.3).collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3), Some(4)]
    );
    // What had not yet come back through the input when the transport stopped is missing
    // from the last one.
    let last = rolled - 4 * BEAT - ROUND_TRIP as i64;
    assert_eq!(
        takes.iter().map(|t| t.2 as i64).collect::<Vec<_>>(),
        vec![2 * BEAT, BEAT, BEAT, last]
    );

    let model = engine.model();
    let track = model.track(TrackId(20)).unwrap();
    assert_eq!(track.clips.len(), 4);
    let file = model.media(media).unwrap();
    assert_eq!(file.frames as i64, rolled - ROUND_TRIP as i64);
    let audio = wav::read(&dir.join(&file.path)).unwrap().channels;
    let mut group = None;
    for (index, (id, _, frames, _)) in takes.iter().enumerate() {
        let clip = track.clip(*id).unwrap();
        let take: Take = clip.take.expect("loop takes are marked");
        assert_eq!(take.number as usize, index + 1);
        assert_eq!(
            *group.get_or_insert(take.group),
            take.group,
            "one group for all of them"
        );
        // Where each sits on the timeline, and which part of the file it plays.
        let starts_at = if index == 0 { 0 } else { BEAT };
        assert_eq!(clip.start, at_sample(starts_at));
        let ClipContent::Audio { offset, length, .. } = clip.content else {
            panic!("not an audio clip")
        };
        assert_eq!(length, *frames);
        let offset_expected = [0, 2 * BEAT, 3 * BEAT, 4 * BEAT][index];
        assert_eq!(offset as i64, offset_expected);
        // And it holds what was played over those very samples, to the sample.
        for probe in [0i64, 1, 777, *frames as i64 - 1] {
            assert_eq!(
                audio[0][(offset as i64 + probe) as usize],
                code(starts_at + probe),
                "take {} at {probe}",
                index + 1
            );
        }
        // The newest whole take is the one that is heard; the cut-short fourth is not.
        assert_eq!(clip.muted, index != 2, "take {}", index + 1);
    }

    // Playing the track back gives take three and nothing else.
    engine
        .apply_wait(vec![
            set(10, TrackChange::Mute(true)),
            set(20, TrackChange::RecordArmed(false)),
            Op::SetLoop {
                range: None,
                enabled: false,
            },
        ])
        .unwrap();
    // (Let the guide track's fader finish closing before listening.)
    engine.sync();
    engine.run(1024);
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(60_000);
    assert_eq!(peak(&out[0][..BEAT as usize]), 0.0);
    assert_eq!(out[0][BEAT as usize + 5], code(BEAT + 5));
    assert_eq!(out[0][2 * BEAT as usize - 1], code(2 * BEAT - 1));
    assert_eq!(peak(&out[0][2 * BEAT as usize..]), 0.0);
    engine.transport(TransportCommand::Stop);

    // Choosing another take is muting and unmuting clips; undoing the recording removes all
    // four and the file's entry in one step.
    engine.apply_wait(undo).unwrap();
    let model = engine.model();
    assert!(model.track(TrackId(20)).unwrap().clips.is_empty());
    assert!(model.media(media).is_none());
    assert_eq!(engine.status().rt_violations, 0);
    assert_eq!(engine.status().capture_dropped, 0);
}

#[test]
fn loop_recording_midi_gives_a_clip_per_time_round() {
    let dir = temp_dir("looprec-midi");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Instrument),
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::SYNTH_UID),
            },
            set(10, TrackChange::Input(TrackInput::Midi)),
            set(10, TrackChange::RecordArmed(true)),
            Op::SetLoop {
                range: Some(beats(0, 2)),
                enabled: true,
            },
        ])
        .unwrap();
    engine.sync();
    engine.transport(TransportCommand::Record);
    engine.sync();

    // First time round: C4 for half a beat, starting half a beat in.
    engine.run(12_000);
    assert!(engine.feed_midi(&[0x90, 60, 100]));
    engine.run(12_000);
    assert!(engine.feed_midi(&[0x80, 60, 0]));
    engine.run(24_000);
    // Second time round: D4, and an E4 that is still held when the loop wraps.
    engine.run(6_000);
    assert!(engine.feed_midi(&[0x90, 62, 90]));
    engine.run(12_000);
    assert!(engine.feed_midi(&[0x80, 62, 0]));
    engine.run(28_000);
    assert!(engine.feed_midi(&[0x90, 64, 80]));
    engine.run(2_000);
    // Third time round: the E4 is let go, and nothing else is played.
    engine.run(4_000);
    assert!(engine.feed_midi(&[0x80, 64, 0]));
    engine.run(8_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);

    // Two takes with notes in them; the third time round had none and leaves no clip.
    let (takes, _) = finished(&events, 10, 2);
    assert_eq!(
        takes.iter().map(|t| (t.2, t.3)).collect::<Vec<_>>(),
        vec![(1, Some(1)), (2, Some(2))]
    );
    let model = engine.model();
    let track = model.track(TrackId(10)).unwrap();
    assert_eq!(track.clips.len(), 2);
    let ticks = |samples: i64| Ticks(samples * TICKS_PER_BEAT / BEAT);
    let notes_of = |clip: ClipId| {
        let clip = track.clip(clip).unwrap();
        assert_eq!(clip.start, Position::Beats(Ticks::ZERO));
        let ClipContent::Midi { notes, length, .. } = &clip.content else {
            panic!("not a MIDI clip")
        };
        assert_eq!(*length, Ticks::from_beats(2));
        (notes.clone(), clip.muted)
    };
    let (first, first_muted) = notes_of(takes[0].0);
    assert_eq!(
        first
            .iter()
            .map(|n| (n.key, n.start, n.length))
            .collect::<Vec<_>>(),
        vec![(60, ticks(12_000), ticks(12_000))]
    );
    let (second, second_muted) = notes_of(takes[1].0);
    assert_eq!(
        second
            .iter()
            .map(|n| (n.key, n.start, n.length))
            .collect::<Vec<_>>(),
        vec![
            (62, ticks(6_000), ticks(12_000)),
            // Held over the wrap: it ends with its own time round.
            (64, ticks(46_000), ticks(2_000)),
        ]
    );
    // Both are whole takes, so the newer one is heard.
    assert!(first_muted && !second_muted);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn punch_recording_takes_only_the_range_and_makes_room_for_it() {
    let dir = temp_dir("punch");
    let mut engine = engine_with(&dir, loopback());
    let events = engine.subscribe();
    coded_session(&engine, &dir);
    // Track 20 already has something on it: a steady 0.5 for four beats.
    engine
        .apply_wait(vec![
            media(&dir, 2, RATE, vec![vec![0.5; 96_000]]),
            Op::AddClip {
                track: TrackId(20),
                clip: audio_clip(21, 2, 0, 96_000),
            },
            // Its own playback must not be re-recorded through the loopback.
            set(20, TrackChange::Mute(true)),
            Op::SetPunch {
                range: Some(LoopRange {
                    start: at_sample(BEAT),
                    end: at_sample(BEAT + 12_000),
                }),
                enabled: true,
            },
        ])
        .unwrap();
    let before = engine.model();

    // Start well before the punch-in: the transport plays, and recording waits for the range.
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(BLOCK);
    let started = wait_for(&events, "recording to start", |e| match e {
        Event::RecordingStarted {
            position_samples, ..
        } => Some(*position_samples),
        _ => None,
    });
    assert_eq!(started, BEAT, "the take starts at the punch-in");
    engine.run(60_000 - BLOCK);
    // The range is long past: recording ended by itself and the transport is still playing.
    assert_eq!(engine.status().transport, TransportState::Playing);
    assert_eq!(engine.status().position_samples, 60_000);

    let (takes, undo) = finished(&events, 20, 1);
    let (clip, media, frames, take) = takes[0];
    assert_eq!(
        (frames, take),
        (12_000, None),
        "exactly the range, and not a loop take"
    );
    let model = engine.model();
    let track = model.track(TrackId(20)).unwrap();
    let punched = track.clip(clip).unwrap();
    assert_eq!(punched.start, at_sample(BEAT));
    let file = model.media(media.unwrap()).unwrap();
    assert_eq!(file.frames, 12_000);
    let audio = wav::read(&dir.join(&file.path)).unwrap().channels;
    for probe in [0i64, 1, 5_000, 11_999] {
        assert_eq!(audio[0][probe as usize], code(BEAT + probe), "at {probe}");
    }

    // The old clip made room: it now ends at the punch-in, and what followed the punch-out is
    // a clip of its own that carries on from the right place in the old file.
    assert_eq!(track.clips.len(), 3);
    let part = |id: ClipId| {
        let clip = track.clip(id).unwrap();
        let ClipContent::Audio {
            media,
            offset,
            length,
            ..
        } = clip.content
        else {
            panic!("not an audio clip")
        };
        (clip.start, media, offset, length)
    };
    assert_eq!(part(ClipId(21)), (at_sample(0), MediaId(2), 0, BEAT as u64));
    let after = track
        .clips
        .iter()
        .find(|c| c.id != ClipId(21) && c.id != clip)
        .unwrap();
    assert_eq!(
        part(after.id),
        (
            at_sample(BEAT + 12_000),
            MediaId(2),
            BEAT as u64 + 12_000,
            96_000 - BEAT as u64 - 12_000
        )
    );

    // Heard back: the old material, the punched-in stretch, the old material again.
    engine.transport(TransportCommand::Stop);
    engine
        .apply_wait(vec![
            set(10, TrackChange::Mute(true)),
            set(20, TrackChange::Mute(false)),
            set(20, TrackChange::RecordArmed(false)),
        ])
        .unwrap();
    let play = |engine: &mut Engine| {
        engine.transport(TransportCommand::Stop);
        engine.sync();
        engine.run(1024);
        engine.transport(TransportCommand::Locate(Position::START));
        engine.transport(TransportCommand::Play);
        engine.sync();
        engine.run(60_000)
    };
    let out = play(&mut engine);
    let (punch_in, punch_out) = (BEAT as usize, BEAT as usize + 12_000);
    assert!(out[0][..punch_in].iter().all(|&s| s == 0.5));
    assert_eq!(out[0][punch_in], code(BEAT));
    assert_eq!(out[0][punch_out - 1], code(BEAT + 11_999));
    assert!(out[0][punch_out..].iter().all(|&s| s == 0.5));

    // One undo puts the track back exactly as it was.
    engine.apply_wait(undo).unwrap();
    assert_eq!(
        engine.model().track(TrackId(20)).unwrap().clips,
        before.track(TrackId(20)).unwrap().clips
    );
    assert!(play(&mut engine)[0].iter().all(|&s| s == 0.5));
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn a_punch_that_cannot_happen_is_refused_and_one_already_under_way_is_cut_to_fit() {
    let dir = temp_dir("punch-edges");
    let mut engine = engine_with(&dir, loopback());
    let events = engine.subscribe();
    coded_session(&engine, &dir);
    engine
        .apply_wait(vec![Op::SetPunch {
            range: Some(LoopRange {
                start: at_sample(BEAT),
                end: at_sample(2 * BEAT),
            }),
            enabled: true,
        }])
        .unwrap();

    // The playhead is already past the range: nothing to record, and the engine says so.
    engine.transport(TransportCommand::Locate(at_sample(3 * BEAT)));
    engine.sync();
    engine.run(BLOCK);
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(BLOCK);
    let message = wait_for(&events, "the refusal", |e| match e {
        Event::Error { message } => Some(message.clone()),
        _ => None,
    });
    assert!(message.contains("punch range"), "{message}");
    assert_eq!(engine.status().transport, TransportState::Stopped);

    // Starting in the middle of the range records from there to the punch-out.
    let from = BEAT + 6_000;
    engine.transport(TransportCommand::Locate(at_sample(from)));
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(40_000);
    let (takes, _) = finished(&events, 20, 1);
    assert_eq!(takes[0].2 as i64, 2 * BEAT - from);
    let model = engine.model();
    let clip = model.track(TrackId(20)).unwrap().clip(takes[0].0).unwrap();
    assert_eq!(clip.start, at_sample(from));

    // With punch switched off, recording is as it always was: from here until stopped.
    engine.transport(TransportCommand::Stop);
    engine
        .apply_wait(vec![Op::SetPunch {
            range: None,
            enabled: false,
        }])
        .unwrap();
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(20_000);
    assert_eq!(engine.status().transport, TransportState::Recording);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    let (takes, _) = finished(&events, 20, 1);
    assert_eq!(takes[0].2, 20_000 - ROUND_TRIP as u64);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn record_pressed_again_before_the_last_take_is_put_away_starts_a_new_one() {
    // Stop and Record in quick succession: the first take's audio is still on its way back
    // from the audio thread when the second recording is asked for. It must not be lost.
    let dir = temp_dir("record-again");
    let mut engine = engine_with(&dir, loopback());
    let events = engine.subscribe();
    coded_session(&engine, &dir);
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(10_000);
    // No block runs between these two, so the audio thread has not even stopped yet.
    engine.transport(TransportCommand::Stop);
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(BLOCK);
    let (first, _) = finished(&events, 20, 1);
    assert_eq!(first[0].2, 10_000 - ROUND_TRIP as u64);
    // The second recording starts by itself once the first has been put away.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.status().transport != TransportState::Recording {
        assert!(
            std::time::Instant::now() < deadline,
            "the second recording never started"
        );
        engine.sync();
        engine.run(BLOCK);
    }
    let from = engine.status().position_samples;
    engine.run(8_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    let (second, _) = finished(&events, 20, 1);
    assert!(
        second[0].2 >= 8_000 - ROUND_TRIP as u64,
        "the second take has {} frames",
        second[0].2
    );
    assert!(from >= 10_000);
    // Pressing Record twice while one is running is still just one recording.
    engine.transport(TransportCommand::Record);
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(4_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    finished(&events, 20, 1);
    engine.sync();
    engine.run(4 * BLOCK);
    engine.sync();
    assert_eq!(engine.status().transport, TransportState::Stopped);
    assert_eq!(engine.model().track(TrackId(20)).unwrap().clips.len(), 3);
}
