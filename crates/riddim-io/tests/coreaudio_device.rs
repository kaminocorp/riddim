//! The CoreAudio backend against whatever device the Mac has. Tolerant by design: CI machines
//! may have no audio hardware at all, in which case the test reports that and passes.

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use riddim_io::coreaudio::{devices, CoreAudioStream};
use riddim_io::{AudioStream, IoBlock, StreamConfig};

#[test]
fn devices_can_be_listed() {
    let found = devices();
    for device in &found {
        eprintln!(
            "{}: {} in, {} out, {} Hz, buffers {:?}",
            device.name,
            device.input_channels,
            device.output_channels,
            device.sample_rate,
            device.buffer_range
        );
        assert!(!device.id.0.is_empty());
    }
    eprintln!("{} device(s)", found.len());
}

#[test]
fn default_output_runs_the_callback() {
    if !devices().iter().any(|d| d.is_default_output) {
        eprintln!("no default output device on this machine; skipped");
        return;
    }
    let blocks = Arc::new(AtomicU64::new(0));
    let frames_seen = Arc::new(AtomicU64::new(0));
    let (b, f) = (blocks.clone(), frames_seen.clone());
    let config = StreamConfig {
        buffer_frames: 256,
        ..StreamConfig::default()
    };
    let mut stream = match CoreAudioStream::open(&config, move |block: &mut IoBlock<'_>| {
        b.fetch_add(1, Ordering::Relaxed);
        f.fetch_add(block.frames as u64, Ordering::Relaxed);
        for channel in block.outputs.iter_mut() {
            channel.fill(0.0); // stay silent
        }
    }) {
        Ok(stream) => stream,
        Err(error) => {
            // Some virtual devices refuse 48 kHz; that is a property of the machine.
            eprintln!("could not open the default output: {error}; skipped");
            return;
        }
    };
    let info = stream.info().clone();
    eprintln!("opened {info:?}");
    assert!(info.output_channels > 0);
    stream.start().expect("start");
    std::thread::sleep(Duration::from_millis(400));
    stream.stop();
    let ran = blocks.load(Ordering::Relaxed);
    assert!(ran > 0, "the IOProc never ran");
    eprintln!(
        "{ran} blocks, {} frames, {} overloads",
        frames_seen.load(Ordering::Relaxed),
        stream.xruns()
    );
}
