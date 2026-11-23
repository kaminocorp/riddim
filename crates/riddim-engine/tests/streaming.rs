//! Long files played from disk: the same samples as from memory, pages coming and going as
//! the playhead moves, and a disk that cannot keep up heard as silence, never as a stall.

mod common;

use std::time::{Duration, Instant};

use common::*;
use riddim_disk::wav::{self, SampleFormat};
use riddim_engine::{
    bounce, BounceRequest, DeviceConfig, Engine, EngineConfig, Event, NullConfig, StreamingConfig,
    TransportCommand, TransportState,
};
use riddim_model::time::Position;
use riddim_model::{LoopRange, Model, Op, TrackId, TrackKind};
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// Ten seconds of stereo whose every sample says where in the file it is.
const FRAMES: usize = 480_000;

fn code(frame: usize, channel: usize) -> f32 {
    ((frame % 65_536) as f32 + 1.0) / 131_072.0 * if channel == 0 { 1.0 } else { -1.0 }
}

fn long_file(dir: &std::path::Path) -> Op {
    media(
        dir,
        1,
        RATE,
        vec![
            (0..FRAMES).map(|n| code(n, 0)).collect(),
            (0..FRAMES).map(|n| code(n, 1)).collect(),
        ],
    )
}

/// Small pages and a short horizon, so that ten seconds of audio is a long file.
fn streaming() -> StreamingConfig {
    StreamingConfig {
        threshold_frames: 100_000,
        page_frames: 4096,
        ahead_seconds: 0.5,
        behind_seconds: 0.1,
        read_delay: None,
    }
}

fn project(dir: &std::path::Path) -> Vec<Op> {
    vec![
        long_file(dir),
        add_track(0, 10, TrackKind::Audio),
        // The clip starts a little into the timeline and a little into the file, with fades.
        Op::AddClip {
            track: TrackId(10),
            clip: {
                let mut clip = audio_clip(11, 1, 1000, 400_000);
                if let riddim_model::ClipContent::Audio {
                    offset,
                    fade_in,
                    fade_out,
                    ..
                } = &mut clip.content
                {
                    *offset = 7777;
                    *fade_in = 3000;
                    *fade_out = 5000;
                }
                clip
            },
        },
    ]
}

fn engine_on(dir: &std::path::Path, device: DeviceConfig, streaming: StreamingConfig) -> Engine {
    let mut config = EngineConfig::manual(dir);
    config.device = device;
    config.streaming = streaming;
    Engine::start(config).unwrap()
}

fn in_memory() -> StreamingConfig {
    StreamingConfig {
        threshold_frames: u64::MAX,
        ..StreamingConfig::default()
    }
}

#[test]
fn a_streamed_file_plays_the_same_samples_as_one_held_in_memory() {
    let dir = temp_dir("stream-same");
    let manual = || DeviceConfig::Manual(NullConfig::default());
    let play = |engine: &mut Engine| {
        engine.apply_wait(project(&dir)).unwrap();
        engine.sync();
        engine.transport(TransportCommand::Locate(Position::START));
        engine.transport(TransportCommand::Play);
        engine.sync();
        let mut out = engine.run(150_000);
        // Jump about: forwards past everything loaded, back to the start, into the fade-out.
        for target in [300_000i64, 2_000, 398_000] {
            engine.transport(TransportCommand::Locate(at_sample(target)));
            engine.sync();
            for channel in engine.run(20_000) {
                out.push(channel);
            }
        }
        out
    };
    let mut memory = engine_on(&dir, manual(), in_memory());
    let reference = play(&mut memory);
    assert!(peak(&reference[0]) > 0.2);
    drop(memory);

    let mut streamed = engine_on(&dir, manual(), streaming());
    let out = play(&mut streamed);
    assert!(
        out == reference,
        "the streamed file does not play what the in-memory one does"
    );
    // On the manual device nothing is ever skipped: a page that is not there is waited for.
    assert_eq!(streamed.status().playback_starved, 0);
    assert_eq!(streamed.status().rt_violations, 0);

    // A bounce of the same project agrees. (A bounce uses the default threshold, under which
    // this file is held in memory; the point is that the two ways of playing it are one.)
    let mut model = Model::new();
    for op in project(&dir) {
        model.apply(&op).unwrap();
    }
    let request = BounceRequest {
        path: dir.join("bounce.wav"),
        start: Position::START,
        end: at_sample(150_000),
        tail_seconds: 0.0,
        sample_rate: RATE,
        block_frames: 128,
        format: SampleFormat::Float32,
        media_dir: dir.clone(),
        tracks: None,
        render_threads: 0,
    };
    let bounced = bounce(&model, &request).unwrap();
    let audio = wav::read(&bounced.path).unwrap();
    assert_eq!(audio.channels[0][..], reference[0][..150_000]);
}

#[test]
fn only_the_neighbourhood_of_the_playhead_is_kept_in_memory() {
    // The timer-driven device: the reader thread has to keep ahead of a real clock.
    let dir = temp_dir("stream-live");
    let engine = engine_on(
        &dir,
        DeviceConfig::Null(NullConfig {
            buffer_frames: 256,
            ..NullConfig::default()
        }),
        streaming(),
    );
    let events = engine.subscribe();
    engine.apply_wait(project(&dir)).unwrap();
    engine
        .apply_wait(vec![Op::SetLoop {
            range: Some(LoopRange {
                start: at_sample(48_000),
                end: at_sample(96_000),
            }),
            enabled: true,
        }])
        .unwrap();
    engine.sync();
    // Give the reader a moment to load the start, as it would have while a user reached for
    // the play button.
    std::thread::sleep(Duration::from_millis(150));
    engine.transport(TransportCommand::Play);
    // Three seconds: twice round the loop and more.
    let started = Instant::now();
    let mut wraps = 0;
    while started.elapsed() < Duration::from_secs(3) {
        if let Ok(Event::Jumped { .. }) = events.recv_timeout(Duration::from_millis(50)) {
            wraps += 1;
        }
    }
    assert!(
        wraps >= 2,
        "the loop wrapped {wraps} times in three seconds"
    );
    let status = engine.status();
    assert_eq!(status.transport, TransportState::Playing);
    // What is in memory is the stretch around the playhead and the start of the loop: with
    // 4096-frame pages, about 0.6 s and 0.5 s of them, a couple of dozen pages, out of the 98
    // the clip covers.
    assert!(
        (4..=28).contains(&status.stream_pages),
        "{} pages are loaded",
        status.stream_pages
    );
    // Nothing was missed: not at the start, not at the wraps (the loop start is kept ready).
    assert_eq!(
        status.playback_starved, 0,
        "the reader fell behind a disk that was not slow"
    );
    assert_eq!(status.rt_violations, 0);

    // A jump to somewhere cold while playing may cost a moment of silence, but only a moment:
    // the reader is told where the playhead is going before it gets there.
    engine
        .apply_wait(vec![Op::SetLoop {
            range: None,
            enabled: false,
        }])
        .unwrap();
    engine.transport(TransportCommand::Locate(at_sample(300_000)));
    std::thread::sleep(Duration::from_millis(500));
    let after_jump = engine.status().playback_starved;
    assert!(
        after_jump < 12_000,
        "{after_jump} frames were missed after a locate: more than a quarter of a second"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        engine.status().playback_starved,
        after_jump,
        "still starving after the jump"
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn a_disk_that_cannot_keep_up_is_heard_as_silence_and_never_stalls_the_audio() {
    // Disk starvation, injected: every page takes 150 ms to read, and a page is 85 ms long.
    let dir = temp_dir("stream-starved");
    let slow = StreamingConfig {
        read_delay: Some(Duration::from_millis(150)),
        ..streaming()
    };
    let engine = engine_on(
        &dir,
        DeviceConfig::Null(NullConfig {
            buffer_frames: 64,
            ..NullConfig::default()
        }),
        slow,
    );
    let events = engine.subscribe();
    engine.apply_wait(project(&dir)).unwrap();
    engine.sync();
    engine.transport(TransportCommand::Play);
    engine.sync();

    let before = engine.status();
    let started = Instant::now();
    let mut reported = 0;
    let mut longest_gap = Duration::ZERO;
    let mut last = (Instant::now(), before.blocks);
    while started.elapsed() < Duration::from_secs(2) {
        if let Ok(Event::PlaybackStarved { total }) = events.recv_timeout(Duration::from_millis(5))
        {
            reported = total;
        }
        let blocks = engine.status().blocks;
        if blocks != last.1 {
            last = (Instant::now(), blocks);
        }
        longest_gap = longest_gap.max(last.0.elapsed());
    }
    let after = engine.status();
    // The disk fell behind, the engine said so...
    assert!(
        after.playback_starved > 20_000,
        "only {} frames starved",
        after.playback_starved
    );
    assert!(reported > 0, "no playback_starved event was sent");
    // ...and the audio callback never waited for it: blocks kept coming at their usual pace
    // (a 64-frame block is 1.3 ms), and the transport is where two seconds of playing put it.
    assert!(
        longest_gap < Duration::from_millis(100),
        "the audio callback went quiet for {longest_gap:?}"
    );
    let played = after.position_samples - before.position_samples;
    assert!(
        (80_000..=110_000).contains(&played),
        "{played} frames played in two seconds: the transport was held up"
    );
    assert_eq!(after.rt_violations, 0);
}
