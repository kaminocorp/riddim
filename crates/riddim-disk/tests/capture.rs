//! The butler: rings in, files out, and what happens when the disk falls behind.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use riddim_disk::capture::{Butler, ButlerConfig};
use riddim_disk::wav::{self, SampleFormat, WavSpec};
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riddim-capture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

const SPEC: WavSpec = WavSpec {
    sample_rate: 48_000,
    channels: 2,
    format: SampleFormat::Float32,
};

fn block(start: usize, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let left = (start..start + frames).map(|n| n as f32).collect();
    let right = (start..start + frames).map(|n| -(n as f32)).collect();
    (left, right)
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn pushed_audio_arrives_in_the_file_in_order_without_heap_traffic_on_the_pushing_side() {
    assert!(riddim_rt::guard::allocator_installed());
    let path = temp("take.wav");
    let butler = Butler::spawn(ButlerConfig {
        flush_interval: Duration::from_millis(50),
        ..ButlerConfig::default()
    });
    let (id, mut sink) = butler.start_capture(&path, SPEC).unwrap();
    let stats = sink.stats().clone();

    // Odd block sizes, so frames straddle the ring's wrap point sooner or later.
    let mut position = 0;
    let mut violations = 0;
    for frames in [64usize, 127, 1, 512, 333, 4096, 77]
        .into_iter()
        .cycle()
        .take(400)
    {
        let (left, right) = block(position, frames);
        let ((), v) = riddim_rt::guard::rt_scope_counted(|| {
            assert!(sink.push(&[&left, &right], frames));
        });
        violations += v;
        position += frames;
        if position % 5 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(violations, 0, "pushing to a capture ring must not allocate");

    // While still "recording", the file on disk becomes valid and grows.
    wait_for("the first flush", || {
        stats.frames_flushed.load(Ordering::Relaxed) > 0
    });
    let partial = wav::read(&path).unwrap();
    assert!(partial.frames() > 0);
    assert_eq!(partial.channels[0][..10], block(0, 10).0[..]);

    drop(sink);
    let result = butler.finish_capture(id).recv().unwrap().unwrap();
    assert_eq!(result.frames, position as u64);
    assert_eq!(result.frames_dropped, 0);
    let data = wav::read(&path).unwrap();
    let (left, right) = block(0, position);
    assert_eq!(data.channels[0], left);
    assert_eq!(data.channels[1], right);
}

#[test]
fn a_stalled_disk_drops_blocks_and_says_so_but_never_blocks_the_pusher() {
    let path = temp("starved.wav");
    let config = ButlerConfig {
        ring_seconds: 0.05,
        ..ButlerConfig::default()
    };
    let butler = Butler::spawn(config);
    butler.set_stall(Duration::from_millis(300));
    let (id, mut sink) = butler.start_capture(&path, SPEC).unwrap();
    let stats = sink.stats().clone();

    let (left, right) = block(0, 480);
    let started = Instant::now();
    let mut accepted = 0u64;
    for _ in 0..100 {
        if sink.push(&[&left, &right], 480) {
            accepted += 480;
        }
    }
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "pushing must not wait for the disk"
    );
    let dropped = stats.frames_dropped.load(Ordering::Relaxed);
    assert!(dropped > 0, "a 50 ms ring cannot hold a second of audio");
    assert_eq!(accepted + dropped, 48_000);

    butler.set_stall(Duration::ZERO);
    drop(sink);
    let result = butler.finish_capture(id).recv().unwrap().unwrap();
    assert_eq!(
        result.frames, accepted,
        "everything that was accepted must reach the file"
    );
    assert_eq!(result.frames_dropped, dropped);
    assert_eq!(wav::probe(&path).unwrap().frames, accepted);
}

#[test]
fn several_captures_run_side_by_side_and_shutdown_leaves_valid_files() {
    let butler = Butler::spawn(ButlerConfig::default());
    let mut sinks = Vec::new();
    for track in 0..8 {
        let path = temp(&format!("multi-{track}.wav"));
        let spec = WavSpec {
            channels: 1 + (track % 2) as u16,
            ..SPEC
        };
        let (_, sink) = butler.start_capture(&path, spec).unwrap();
        sinks.push((path, sink, track));
    }
    for round in 0..50 {
        for (_, sink, track) in sinks.iter_mut() {
            let samples = vec![*track as f32 + round as f32 / 100.0; 256];
            assert!(sink.push(&[&samples, &samples], 256));
        }
    }
    let paths: Vec<_> = sinks
        .iter()
        .map(|(path, _, track)| (path.clone(), *track))
        .collect();
    drop(sinks);
    // No finish: dropping the butler must still close every file properly.
    drop(butler);
    for (path, track) in paths {
        let data = wav::read(&path).unwrap();
        assert_eq!(data.frames(), 50 * 256);
        assert_eq!(data.channels.len(), 1 + track % 2);
        assert_eq!(data.channels[0][0], track as f32);
        assert_eq!(data.channels[0][50 * 256 - 1], track as f32 + 0.49);
    }
}

#[test]
fn bad_paths_fail_at_start_and_unknown_captures_at_finish() {
    let butler = Butler::spawn(ButlerConfig::default());
    assert!(butler
        .start_capture(std::path::Path::new("/nonexistent-dir/x.wav"), SPEC)
        .is_err());
    let missing = butler
        .finish_capture(riddim_disk::capture::CaptureId(999))
        .recv()
        .unwrap();
    assert!(missing.is_err());
}
