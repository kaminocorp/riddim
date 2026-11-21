//! Recording through the engine: where takes land, what is on disk, and what the model gains.

mod common;

use common::*;
use riddim_disk::wav;
use riddim_engine::{Engine, Event, NullConfig, TransportCommand, TransportState};
use riddim_model::time::{Position, Ticks, TICKS_PER_BEAT};
use riddim_model::{
    ClipContent, ClipId, MediaId, Metronome, Op, TrackChange, TrackId, TrackInput, TrackKind,
};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// A device whose output is wired back to its input with `round_trip` frames of delay, and
/// which reports that delay honestly, split between input and output.
fn loopback(round_trip: u32) -> NullConfig {
    NullConfig {
        loopback_delay: Some(round_trip),
        reported_input_latency: round_trip / 3,
        reported_output_latency: round_trip - round_trip / 3,
        ..NullConfig::default()
    }
}

fn set(id: u64, change: TrackChange) -> Op {
    Op::SetTrack {
        id: TrackId(id),
        change,
    }
}

/// Track 10 plays a single-sample impulse at timeline sample `at`; track 20 is armed to record
/// the device's inputs.
fn impulse_session(engine: &Engine, dir: &std::path::Path, at: i64) {
    let mut impulse = vec![0.0f32; 64];
    impulse[0] = 1.0;
    engine
        .apply_wait(vec![
            media(dir, 1, RATE, vec![impulse]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, at, 64),
            },
            set(
                20,
                TrackChange::Input(TrackInput::Audio {
                    first_channel: 0,
                    channels: 2,
                }),
            ),
            set(20, TrackChange::RecordArmed(true)),
        ])
        .unwrap();
    engine.sync();
}

/// Records for `frames`, stops, and returns the finished take: (clip, media, frames).
fn record(
    engine: &mut Engine,
    events: &std::sync::mpsc::Receiver<Event>,
    frames: usize,
) -> (ClipId, MediaId, u64) {
    let (take, _) = record_with_undo(engine, events, frames);
    take
}

/// As [`record`], also returning the inverse ops the engine reported for the take.
fn record_with_undo(
    engine: &mut Engine,
    events: &std::sync::mpsc::Receiver<Event>,
    frames: usize,
) -> ((ClipId, MediaId, u64), Vec<Op>) {
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(frames);
    assert_eq!(engine.status().transport, TransportState::Recording);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    // The engine first reports its edit to the model, then that the take is finished.
    let mut undo = Vec::new();
    let take = wait_for(events, "the take to be finished", |e| match e {
        Event::ModelChanged { inverses, .. } => {
            undo = inverses.clone();
            None
        }
        Event::RecordingFinished {
            track: TrackId(20),
            clip,
            media: Some(media),
            frames,
        } => Some((*clip, *media, *frames)),
        _ => None,
    });
    (take, undo)
}

#[test]
fn a_loopback_recording_lands_on_the_sample_it_was_played_at() {
    // Phase 2 exit criterion 2: recorded audio within one sample of the loopback-calibrated
    // position. Here it is exact, for several round-trip latencies and impulse positions.
    for (round_trip, impulse_at) in [(300u32, 5000i64), (128, 1), (1024, 12_345), (777, 128 * 40)] {
        let dir = temp_dir("loopback");
        let mut engine = engine_with(&dir, loopback(round_trip));
        let events = engine.subscribe();
        impulse_session(&engine, &dir, impulse_at);

        let total = 20_000;
        let (clip, media, frames) = record(&mut engine, &events, total);
        let model = engine.model();
        let take = model
            .track(TrackId(20))
            .unwrap()
            .clip(clip)
            .expect("the take is on the armed track")
            .clone();
        assert_eq!(
            take.start,
            at_sample(0),
            "the take starts where recording started"
        );
        assert!(matches!(take.content, ClipContent::Audio { length, .. } if length == frames));
        // What was captured covers the timeline from the start to the stop, less the round trip
        // still in flight when the transport stopped.
        assert_eq!(frames as i64, total as i64 - round_trip as i64);

        // On disk: the impulse is at the timeline position it was played at.
        let file = wav::read(std::path::Path::new(&model.media(media).unwrap().path)).unwrap();
        assert_eq!(file.frames() as u64, frames);
        let position = file.channels[0].iter().position(|&s| s != 0.0);
        assert_eq!(
            position,
            Some(impulse_at as usize),
            "round trip {round_trip}: the take is misplaced"
        );
        assert_eq!(file.channels[0][impulse_at as usize], 1.0);
        assert_eq!(file.channels[0].iter().filter(|&&s| s != 0.0).count(), 1);

        // And played back alone, the take puts the impulse where the original was.
        engine
            .apply_wait(vec![
                set(10, TrackChange::Mute(true)),
                set(20, TrackChange::RecordArmed(false)),
            ])
            .unwrap();
        engine.transport(TransportCommand::Locate(Position::START));
        engine.transport(TransportCommand::Play);
        engine.sync();
        let out = engine.run(total);
        assert_eq!(first_sound(&out[0]), Some(impulse_at as usize));
        assert_eq!(
            engine.status().rt_violations,
            0,
            "recording touched the heap on the audio path"
        );
        assert_eq!(engine.status().capture_dropped, 0);
    }
}

#[test]
fn a_take_is_an_undoable_edit_and_overdubs_stack_up() {
    let dir = temp_dir("takes");
    let mut engine = engine_with(&dir, loopback(256));
    let events = engine.subscribe();
    impulse_session(&engine, &dir, 1000);
    let before = engine.model();

    // The engine reports its own edit together with the inverses that undo it.
    let ((first_clip, first_media, _), inverses) = record_with_undo(&mut engine, &events, 8000);
    assert_eq!(inverses.len(), 2, "one op removes the clip, one the media");
    let with_take = engine.model();
    assert_eq!(with_take.media.len(), 2);

    // A second pass from the top records another take beside the first.
    engine.transport(TransportCommand::Locate(Position::START));
    let (second_clip, second_media, _) = record(&mut engine, &events, 8000);
    assert_ne!((first_clip, first_media), (second_clip, second_media));
    assert_eq!(engine.model().track(TrackId(20)).unwrap().clips.len(), 2);
    let files: Vec<String> = engine
        .model()
        .media
        .iter()
        .skip(1)
        .map(|m| m.path.clone())
        .collect();
    assert_ne!(files[0], files[1], "takes must not overwrite each other");
    assert!(files.iter().all(|f| std::path::Path::new(f).is_file()));

    // The takes are independent edits: the first can be undone while the second stays.
    engine.apply_wait(inverses).unwrap();
    let after_undo = engine.model();
    assert!(after_undo
        .track(TrackId(20))
        .unwrap()
        .clip(first_clip)
        .is_none());
    assert!(after_undo
        .track(TrackId(20))
        .unwrap()
        .clip(second_clip)
        .is_some());
    assert_eq!(before.tracks.len(), after_undo.tracks.len());
}

#[test]
fn recording_into_a_running_transport_punches_in_at_a_known_sample() {
    let dir = temp_dir("punch");
    let mut engine = engine_with(&dir, loopback(300));
    let events = engine.subscribe();
    impulse_session(&engine, &dir, 9000);
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(4096);

    engine.transport(TransportCommand::Record);
    engine.sync();
    // The audio thread fixes the start when it sees the command, at its next block.
    engine.run(BLOCK);
    let start = wait_for(&events, "recording to start", |e| match e {
        Event::RecordingStarted {
            position_samples, ..
        } => Some(*position_samples),
        _ => None,
    });
    assert_eq!(
        start,
        4096 + 2 * BLOCK as i64,
        "the punch-in point is two blocks ahead of the command"
    );
    engine.run(10_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    let (clip, media) = wait_for(&events, "the take", |e| match e {
        Event::RecordingFinished {
            clip,
            media: Some(media),
            ..
        } => Some((*clip, *media)),
        _ => None,
    });
    let model = engine.model();
    assert_eq!(
        model.track(TrackId(20)).unwrap().clip(clip).unwrap().start,
        at_sample(start)
    );
    let file = wav::read(std::path::Path::new(&model.media(media).unwrap().path)).unwrap();
    assert_eq!(
        file.channels[0].iter().position(|&s| s != 0.0),
        Some((9000 - start) as usize)
    );
}

#[test]
fn a_count_in_clicks_first_and_the_take_still_starts_on_the_bar() {
    let dir = temp_dir("countin");
    let mut engine = engine_with(&dir, loopback(256));
    let events = engine.subscribe();
    impulse_session(&engine, &dir, 100);
    engine
        .apply_wait(vec![Op::SetMetronome {
            metronome: Metronome {
                enabled: false,
                gain: 0.5,
                count_in_bars: 1,
            },
        }])
        .unwrap();
    // Start from bar 3 (beat 8, sample 192000 at 120 bpm).
    let bar_three = 8 * 24_000;
    engine.transport(TransportCommand::Locate(Position::Beats(
        Ticks::from_beats(8),
    )));
    engine.sync();
    engine.run(BLOCK);

    engine.transport(TransportCommand::Record);
    engine.sync();
    // One bar of count-in: four clicks, although the metronome itself is off.
    let count_in = engine.run(96_000);
    let clicks = (0..4)
        .filter(|beat| count_in[0][beat * 24_000 + 1] != 0.0)
        .count();
    assert_eq!(
        clicks, 4,
        "the count-in should click on each of its four beats"
    );
    assert_eq!(engine.status().position_samples, bar_three);

    // Past the start the metronome is silent again and the take is being captured.
    let after = engine.run(48_000);
    assert!(
        after[0].iter().all(|&s| s == 0.0),
        "the click must stop when recording starts"
    );
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    let clip = wait_for(&events, "the take", |e| match e {
        Event::RecordingFinished { clip, .. } => Some(*clip),
        _ => None,
    });
    assert_eq!(
        engine
            .model()
            .track(TrackId(20))
            .unwrap()
            .clip(clip)
            .unwrap()
            .start,
        at_sample(bar_three)
    );
}

#[test]
fn midi_played_while_recording_becomes_a_clip_with_the_notes_in_place() {
    let dir = temp_dir("midirec");
    // Output latency only matters for where MIDI lands; make it obvious.
    let device = NullConfig {
        reported_output_latency: 480,
        ..NullConfig::default()
    };
    let mut engine = engine_with(&dir, device);
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
        ])
        .unwrap();
    engine.sync();

    engine.transport(TransportCommand::Record);
    engine.sync();
    // Play C4 one second in and release it half a second later; then a D4 that is never
    // released. Each message is fed between blocks, as a keyboard's would arrive.
    engine.run(48_000);
    assert!(engine.feed_midi(&[0x90, 60, 100]));
    let sounding = engine.run(24_000);
    assert!(
        peak(&sounding[0]) > 0.0,
        "the armed track must sound what is played"
    );
    assert!(engine.feed_midi(&[0x80, 60, 0]));
    engine.run(24_000);
    assert!(engine.feed_midi(&[0x90, 62, 64]));
    engine.run(24_000);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    // The player lets go of the key after the transport has stopped.
    assert!(engine.feed_midi(&[0x80, 62, 0]));
    engine.run(BLOCK);

    let (clip, count) = wait_for(&events, "the MIDI take", |e| match e {
        Event::RecordingFinished {
            track: TrackId(10),
            clip,
            media: None,
            frames,
        } => Some((*clip, *frames)),
        _ => None,
    });
    assert_eq!(count, 2);
    let model = engine.model();
    let take = model.track(TrackId(10)).unwrap().clip(clip).unwrap();
    assert_eq!(take.start, Position::Beats(Ticks::ZERO));
    let ClipContent::Midi { notes, length, .. } = &take.content else {
        panic!("not a MIDI clip")
    };

    // The player heard the timeline 480 samples late, so each note belongs 480 samples before
    // the block it arrived in: 48000 - 480 samples is 1.98 beats at 120 bpm.
    let ticks_at = |sample: i64| Ticks(sample * TICKS_PER_BEAT / 24_000);
    assert_eq!(notes[0].key, 60);
    assert_eq!(notes[0].velocity, 100);
    assert_eq!(notes[0].start, ticks_at(48_000 - 480));
    assert_eq!(notes[0].length, ticks_at(24_000));
    assert_eq!(notes[1].key, 62);
    assert_eq!(notes[1].start, ticks_at(96_000 - 480));
    // The unreleased note ends with the take.
    assert_eq!(notes[1].start + notes[1].length, *length);

    // The take is also on disk as a Standard MIDI File with the same notes.
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "mid"))
        .expect("a .mid file for the take");
    let on_disk = riddim_disk::smf::read(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(on_disk.notes.len(), 2);
    assert_eq!(on_disk.notes[0].key, 60);
    // A MIDI file holds 960 ticks per beat, coarser than the model: the note is within one of
    // the file's ticks of where the model has it.
    let file_tick = TICKS_PER_BEAT / 960;
    assert!((on_disk.notes[0].start.0 - notes[0].start.0).abs() <= file_tick);

    // Playing the take back sounds the instrument at the corrected position.
    engine
        .apply_wait(vec![set(10, TrackChange::RecordArmed(false))])
        .unwrap();
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(60_000);
    assert_eq!(first_sound(&out[0]), Some(48_000 - 480 + 1));
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn recording_with_nothing_armed_is_an_error_not_a_silent_no_op() {
    let dir = temp_dir("unarmed");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![add_track(0, 10, TrackKind::Audio)])
        .unwrap();
    engine.transport(TransportCommand::Record);
    engine.sync();
    let message = wait_for(&events, "an error", |e| match e {
        Event::Error { message } => Some(message.clone()),
        _ => None,
    });
    assert!(message.contains("armed"), "{message}");
    engine.run(1024);
    assert_eq!(engine.status().transport, TransportState::Stopped);
}

#[test]
fn eight_tracks_record_together_without_losing_a_frame() {
    // A short stand-in for the phase's "60 minutes on 8 tracks" criterion: the same machinery,
    // eight captures at once, checked frame for frame. (The hour-long run is `long_recording`.)
    let dir = temp_dir("eight");
    let mut engine = engine_with(&dir, loopback(256));
    let events = engine.subscribe();
    impulse_session(&engine, &dir, 2000);
    let mut ops = Vec::new();
    for n in 0..7u64 {
        let id = 100 + n * 10;
        ops.push(add_track(2 + n as usize, id, TrackKind::Audio));
        ops.push(set(
            id,
            TrackChange::Input(TrackInput::Audio {
                first_channel: (n % 2) as u16,
                channels: 1,
            }),
        ));
        ops.push(set(id, TrackChange::RecordArmed(true)));
    }
    engine.apply_wait(ops).unwrap();
    engine.sync();

    let total = 10 * RATE as usize;
    engine.transport(TransportCommand::Record);
    engine.sync();
    // Run in slices and let the disk thread breathe: the manual device runs far faster than
    // real time, which a real one never would.
    for _ in 0..100 {
        engine.run(total / 100);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);

    let mut takes = Vec::new();
    while takes.len() < 8 {
        takes.push(wait_for(&events, "all eight takes", |e| match e {
            Event::RecordingFinished {
                track,
                media: Some(media),
                frames,
                ..
            } => Some((*track, *media, *frames)),
            _ => None,
        }));
    }
    let model = engine.model();
    for (track, media, frames) in takes {
        assert_eq!(frames as usize, total - 256, "track {} is short", track.0);
        let file = wav::read(std::path::Path::new(&model.media(media).unwrap().path)).unwrap();
        assert_eq!(
            file.channels[0].iter().position(|&s| s != 0.0),
            Some(2000),
            "track {}",
            track.0
        );
    }
    assert_eq!(engine.status().capture_dropped, 0);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
#[ignore = "writes about 5.5 GB and takes a few minutes; run with --ignored"]
fn long_recording() {
    // Phase 2 exit criterion 5, as far as it can be checked without a real clock: an hour of
    // timeline on eight tracks, every frame accounted for.
    let dir = temp_dir("hour");
    let mut engine = engine_with(&dir, loopback(256));
    let events = engine.subscribe();
    impulse_session(&engine, &dir, 48_000);
    let mut ops = Vec::new();
    for n in 0..7u64 {
        let id = 100 + n * 10;
        ops.push(add_track(2 + n as usize, id, TrackKind::Audio));
        ops.push(set(
            id,
            TrackChange::Input(TrackInput::Audio {
                first_channel: 0,
                channels: 1,
            }),
        ));
        ops.push(set(id, TrackChange::RecordArmed(true)));
    }
    engine.apply_wait(ops).unwrap();
    engine.sync();

    let total = 60 * 60 * RATE as usize;
    engine.transport(TransportCommand::Record);
    engine.sync();
    // The manual device runs many times faster than real time, so let the disk set the pace:
    // never get more than a second of audio ahead of what has been written. (On real hardware
    // the clock does this; the ring holds four seconds.)
    let slice = RATE as usize / 4;
    let mut worst_backlog = 0;
    let mut first_drop = None;
    for iteration in 0..total / slice {
        engine.run(slice);
        loop {
            // A round trip through the control thread makes the backlog figure current even
            // when this machine is busy with something else.
            engine.sync();
            let status = engine.status();
            let backlog = status.capture_backlog;
            worst_backlog = worst_backlog.max(backlog);
            if status.capture_dropped > 0 && first_drop.is_none() {
                first_drop = Some((iteration, backlog, status.capture_dropped));
            }
            if backlog < RATE as u64 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    eprintln!("first drop (iteration, backlog, dropped): {first_drop:?}");
    eprintln!("worst disk backlog: {worst_backlog} frames");
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(BLOCK);
    let mut finished = 0;
    while finished < 8 {
        let frames = wait_for(&events, "the takes", |e| match e {
            Event::RecordingFinished {
                frames,
                media: Some(_),
                ..
            } => Some(*frames),
            _ => None,
        });
        assert_eq!(frames as usize, total - 256);
        finished += 1;
    }
    assert_eq!(engine.status().capture_dropped, 0, "frames were dropped");
    assert_eq!(engine.status().rt_violations, 0);
    std::fs::remove_dir_all(&dir).ok();
}
