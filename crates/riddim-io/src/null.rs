//! A device with no hardware behind it.
//!
//! [`ManualStream`] is stepped explicitly and is fully deterministic: tests and offline tools
//! use it. [`NullStream`] runs the same driver on a timer thread at roughly real-time pace, for
//! running the engine headless.
//!
//! Both can loop their output back to their input after a fixed delay, which stands in for a
//! cable from output to input when testing recording alignment.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use riddim_rt::planar::PlanarScratch;

use crate::{AudioCallback, AudioStream, IoBlock, IoError, StreamInfo};

/// How a deviceless stream is shaped.
#[derive(Clone, Debug, PartialEq)]
pub struct NullConfig {
    pub sample_rate: f64,
    pub buffer_frames: u32,
    pub input_channels: u32,
    pub output_channels: u32,
    /// Feed the output back into the input, delayed by this many frames (the simulated
    /// round-trip latency of a loopback cable). `None` gives silent inputs.
    ///
    /// The delay cannot be shorter than one buffer (a block's input is fixed before its output
    /// exists, exactly as on real hardware), so smaller values are raised to `buffer_frames`.
    pub loopback_delay: Option<u32>,
    /// Latencies the stream should claim to have, as a real device would report them.
    pub reported_input_latency: u32,
    pub reported_output_latency: u32,
}

impl Default for NullConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            buffer_frames: 128,
            input_channels: 2,
            output_channels: 2,
            loopback_delay: None,
            reported_input_latency: 0,
            reported_output_latency: 0,
        }
    }
}

/// The part shared by both stream flavours: buffers, the callback, and the loopback line.
struct Driver {
    callback: Box<dyn AudioCallback>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    scratch: PlanarScratch,
    /// One ring per loopback channel, indexed by absolute frame, plus the delay in frames.
    loopback: Option<(Vec<Vec<f32>>, usize)>,
    frames_run: u64,
    sample_rate: f64,
    max_frames: usize,
}

impl Driver {
    fn new(config: &NullConfig, callback: Box<dyn AudioCallback>) -> Self {
        let max_frames = config.buffer_frames.max(1) as usize;
        let ins = config.input_channels as usize;
        let outs = config.output_channels as usize;
        let loopback = config.loopback_delay.map(|delay| {
            let delay = (delay as usize).max(max_frames);
            (vec![vec![0.0; delay + max_frames]; ins.min(outs)], delay)
        });
        Self {
            callback,
            inputs: vec![vec![0.0; max_frames]; ins],
            outputs: vec![vec![0.0; max_frames]; outs],
            scratch: PlanarScratch::new(ins, outs),
            loopback,
            frames_run: 0,
            sample_rate: config.sample_rate,
            max_frames,
        }
    }

    /// Re-shapes the buffers for another block size. What was in the loopback line is lost.
    fn resize(&mut self, config: &NullConfig) {
        let max_frames = config.buffer_frames.max(1) as usize;
        for channel in self.inputs.iter_mut().chain(self.outputs.iter_mut()) {
            channel.clear();
            channel.resize(max_frames, 0.0);
        }
        let channels = self.inputs.len().min(self.outputs.len());
        self.loopback = config.loopback_delay.map(|delay| {
            let delay = (delay as usize).max(max_frames);
            (vec![vec![0.0; delay + max_frames]; channels], delay)
        });
        // The loopback line is indexed by absolute frame; start it afresh.
        self.frames_run = 0;
        self.max_frames = max_frames;
    }

    fn run_block(&mut self, frames: usize) {
        let frames = frames.min(self.max_frames);
        // Inputs for this block: what the output produced `delay` frames ago.
        let start = self.frames_run as usize;
        if let Some((rings, delay)) = &self.loopback {
            for (input, ring) in self.inputs.iter_mut().zip(rings) {
                for (i, sample) in input[..frames].iter_mut().enumerate() {
                    let at = start + i;
                    *sample = if at >= *delay {
                        ring[(at - delay) % ring.len()]
                    } else {
                        0.0
                    };
                }
            }
        }
        for output in &mut self.outputs {
            output[..frames].fill(0.0);
        }
        let output_time_ns = (self.frames_run as f64 / self.sample_rate * 1e9) as u64;
        let callback = &mut self.callback;
        {
            let (inputs, outputs) = self.scratch.fill(
                self.inputs.iter().map(|c| c.as_slice()),
                self.outputs.iter_mut().map(|c| c.as_mut_slice()),
                frames,
            );
            let _denormals = riddim_rt::denormal::ScopedNoDenormals::new();
            let mut block = IoBlock {
                frames,
                inputs,
                outputs,
                output_time_ns,
            };
            riddim_rt::guard::rt_scope(|| callback.process(&mut block));
        }
        // Remember this block's output for when its turn comes round.
        if let Some((rings, _)) = &mut self.loopback {
            for (ring, output) in rings.iter_mut().zip(&self.outputs) {
                let len = ring.len();
                for (i, sample) in output[..frames].iter().enumerate() {
                    ring[(start + i) % len] = *sample;
                }
            }
        }
        self.frames_run += frames as u64;
    }
}

fn info_for(config: &NullConfig, name: &str) -> StreamInfo {
    StreamInfo {
        device_name: name.to_string(),
        sample_rate: config.sample_rate,
        max_frames: config.buffer_frames.max(1),
        input_channels: config.input_channels,
        output_channels: config.output_channels,
        input_latency_frames: config.reported_input_latency,
        output_latency_frames: config.reported_output_latency,
    }
}

/// A stream that only advances when told to. Not real-time, entirely deterministic.
pub struct ManualStream {
    driver: Driver,
    info: StreamInfo,
    config: NullConfig,
}

impl ManualStream {
    pub fn new(config: NullConfig, callback: impl AudioCallback) -> Self {
        Self {
            info: info_for(&config, "manual"),
            driver: Driver::new(&config, Box::new(callback)),
            config,
        }
    }

    /// Runs one block of `frames` (at most the configured buffer size) on the calling thread.
    pub fn run_block(&mut self, frames: usize) {
        self.driver.run_block(frames);
    }

    /// Runs `count` full-size blocks.
    pub fn run_blocks(&mut self, count: usize) {
        for _ in 0..count {
            self.driver.run_block(self.driver.max_frames);
        }
    }

    /// What the callback last wrote to output channel `channel`.
    pub fn last_output(&self, channel: usize) -> &[f32] {
        &self.driver.outputs[channel]
    }

    pub fn frames_run(&self) -> u64 {
        self.driver.frames_run
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Changes the block size the stream runs at.
    pub fn set_buffer_frames(&mut self, frames: u32) {
        self.config.buffer_frames = frames.max(1);
        self.driver.resize(&self.config);
        self.info = info_for(&self.config, "manual");
    }
}

/// A stream paced by a timer thread, approximating a real device's cadence.
pub struct NullStream {
    info: StreamInfo,
    config: NullConfig,
    driver: Option<Driver>,
    thread: Option<JoinHandle<Driver>>,
    running: Arc<AtomicBool>,
}

impl NullStream {
    pub fn new(config: NullConfig, callback: impl AudioCallback) -> Self {
        Self {
            info: info_for(&config, "null"),
            driver: Some(Driver::new(&config, Box::new(callback))),
            config,
            thread: None,
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl AudioStream for NullStream {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn start(&mut self) -> Result<(), IoError> {
        let Some(mut driver) = self.driver.take() else {
            return Ok(());
        };
        let running = self.running.clone();
        running.store(true, Ordering::Release);
        let frames = self.config.buffer_frames.max(1) as usize;
        let period = Duration::from_secs_f64(frames as f64 / self.config.sample_rate);
        let thread = std::thread::Builder::new()
            .name("riddim-null-audio".into())
            .spawn(move || {
                let mut next = Instant::now();
                while running.load(Ordering::Acquire) {
                    driver.run_block(frames);
                    next += period;
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    } else {
                        // We fell behind; do not try to catch up in a burst.
                        next = Instant::now();
                    }
                }
                driver
            })
            .map_err(|_| IoError::Unsupported("could not start the audio thread"))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if let Ok(driver) = thread.join() {
                self.driver = Some(driver);
            }
        }
    }

    fn xruns(&self) -> u64 {
        0
    }

    fn set_buffer_frames(&mut self, frames: u32) -> Result<(), IoError> {
        let was_running = self.thread.is_some();
        self.stop();
        self.config.buffer_frames = frames.max(1);
        if let Some(driver) = self.driver.as_mut() {
            driver.resize(&self.config);
        }
        self.info = info_for(&self.config, "null");
        if was_running {
            self.start()?;
        }
        Ok(())
    }
}

impl Drop for NullStream {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn manual_stream_runs_the_callback_with_planar_buffers() {
        let mut stream = ManualStream::new(NullConfig::default(), |block: &mut IoBlock<'_>| {
            assert_eq!(block.inputs.len(), 2);
            assert_eq!(block.outputs.len(), 2);
            for (c, channel) in block.outputs.iter_mut().enumerate() {
                assert_eq!(channel.len(), block.frames);
                channel.fill(c as f32 + 1.0);
            }
        });
        stream.run_block(64);
        assert_eq!(&stream.last_output(0)[..64], &[1.0; 64][..]);
        assert_eq!(&stream.last_output(1)[..64], &[2.0; 64][..]);
        stream.run_blocks(3);
        assert_eq!(stream.frames_run(), 64 + 3 * 128);
    }

    /// Emits a single 1.0 at an absolute frame and records where a 1.0 comes back in.
    fn loopback_round_trip(delay: u32, buffer: u32, impulse_at: u64) -> u64 {
        let config = NullConfig {
            buffer_frames: buffer,
            loopback_delay: Some(delay),
            ..NullConfig::default()
        };
        let heard_at = Arc::new(AtomicU64::new(u64::MAX));
        let heard = heard_at.clone();
        let mut position = 0u64;
        let mut stream = ManualStream::new(config, move |block: &mut IoBlock<'_>| {
            for frame in 0..block.frames {
                let absolute = position + frame as u64;
                if block.inputs[0][frame] == 1.0 {
                    heard.store(absolute, Ordering::Relaxed);
                }
                if absolute == impulse_at {
                    block.outputs[0][frame] = 1.0;
                }
            }
            position += block.frames as u64;
        });
        stream.run_blocks(((impulse_at + delay as u64) / buffer as u64) as usize + 4);
        heard_at.load(Ordering::Relaxed)
    }

    #[test]
    fn loopback_returns_the_output_after_exactly_the_delay() {
        for (delay, buffer, at) in [
            (300, 128, 5),
            (128, 128, 0),
            (129, 128, 127),
            (1000, 256, 255),
            (4096, 64, 1000),
        ] {
            let heard = loopback_round_trip(delay, buffer, at);
            assert_eq!(
                heard,
                at + delay as u64,
                "delay {delay} buffer {buffer} at {at}"
            );
        }
    }

    #[test]
    fn loopback_delay_is_at_least_one_buffer() {
        assert_eq!(loopback_round_trip(0, 128, 10), 10 + 128);
        assert_eq!(loopback_round_trip(64, 256, 7), 7 + 256);
    }

    #[test]
    fn paced_stream_starts_and_stops() {
        let count = Arc::new(AtomicU64::new(0));
        let seen = count.clone();
        let config = NullConfig {
            buffer_frames: 64,
            ..NullConfig::default()
        };
        let mut stream = NullStream::new(config, move |_: &mut IoBlock<'_>| {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        stream.start().unwrap();
        std::thread::sleep(Duration::from_millis(60));
        stream.stop();
        let blocks = count.load(Ordering::Relaxed);
        assert!(
            blocks > 5,
            "the callback should have run several times, ran {blocks}"
        );
        // Restartable.
        stream.start().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        stream.stop();
        assert!(count.load(Ordering::Relaxed) > blocks);
    }
}
