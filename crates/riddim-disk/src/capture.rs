//! Recording to disk without the audio thread ever waiting for it.
//!
//! Each capture is a pre-sized ring. The audio thread interleaves samples into it through a
//! [`CaptureSink`]; the [`Butler`] thread drains every ring into its file a few times a second
//! and brings the file's header up to date at a regular interval, so a take is a valid file on
//! disk while it is still being recorded.
//!
//! If the disk cannot keep up and a ring fills, whole blocks are dropped and counted; the audio
//! thread still does not block. A drop is an error the application must be told about, never a
//! silent one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use riddim_rt::spsc::{self, Consumer, Producer};

use crate::wav::{WavSpec, WavWriter};
use crate::DiskError;

/// Progress of one capture, readable from any thread.
#[derive(Debug, Default)]
pub struct CaptureStats {
    /// Frames the audio thread handed over.
    pub frames_pushed: AtomicU64,
    /// Frames lost because the ring was full.
    pub frames_dropped: AtomicU64,
    /// Frames that have reached the file.
    pub frames_written: AtomicU64,
    /// Frames covered by the file's header as of the last flush.
    pub frames_flushed: AtomicU64,
    /// Set if writing failed (disk full, file removed).
    pub failed: AtomicBool,
}

/// The audio thread's end of a capture.
pub struct CaptureSink {
    ring: Producer<f32>,
    channels: usize,
    stats: Arc<CaptureStats>,
}

impl CaptureSink {
    /// Queues `frames` frames from `channels` for writing. Wait-free, no allocation.
    ///
    /// Missing channels are written as silence; extra ones are ignored. Returns false, and
    /// counts the frames as dropped, if the ring has no room for the whole block.
    #[inline]
    pub fn push(&mut self, channels: &[&[f32]], frames: usize) -> bool {
        let samples = frames * self.channels;
        let Ok(chunk) = self.ring.write_chunk_uninit(samples) else {
            self.stats
                .frames_dropped
                .fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        };
        let width = self.channels;
        let interleaved = (0..samples).map(|i| {
            let (frame, channel) = (i / width, i % width);
            channels.get(channel).map_or(0.0, |c| c[frame])
        });
        chunk.fill_from_iter(interleaved);
        self.stats
            .frames_pushed
            .fetch_add(frames as u64, Ordering::Relaxed);
        true
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn stats(&self) -> &Arc<CaptureStats> {
        &self.stats
    }
}

/// Identifies a capture to the butler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CaptureId(pub u64);

/// What a finished capture left on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureResult {
    pub path: PathBuf,
    pub frames: u64,
    pub frames_dropped: u64,
}

/// How the butler behaves.
#[derive(Clone, Copy, Debug)]
pub struct ButlerConfig {
    /// How often the rings are drained.
    pub poll_interval: Duration,
    /// How often each file's header is brought up to date.
    pub flush_interval: Duration,
    /// Also ask the operating system to commit to the device at each flush. Survives power
    /// loss, costs latency on slow disks. Without it, flushed data survives a crash of the
    /// process but not of the machine.
    pub sync_on_flush: bool,
    /// Seconds of audio each ring can hold before the audio thread has to drop.
    pub ring_seconds: f64,
}

impl Default for ButlerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(20),
            flush_interval: Duration::from_secs(1),
            sync_on_flush: false,
            ring_seconds: 4.0,
        }
    }
}

struct ActiveCapture {
    writer: WavWriter,
    ring: Consumer<f32>,
    path: PathBuf,
    stats: Arc<CaptureStats>,
    last_flush: Instant,
    error: Option<String>,
}

impl ActiveCapture {
    /// Moves everything queued (in whole frames) into the file.
    fn drain(&mut self) {
        let channels = self.writer.spec().channels as usize;
        let available = self.ring.slots();
        let samples = available - available % channels;
        if samples == 0 {
            return;
        }
        let Ok(chunk) = self.ring.read_chunk(samples) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        let mut failure = None;
        if self.error.is_none() {
            // A frame can straddle the ring's wrap point, so write the two halves as one run
            // only when the first ends on a frame boundary.
            let result = if first.len() % channels == 0 {
                self.writer
                    .write_interleaved(first)
                    .and_then(|()| self.writer.write_interleaved(second))
            } else {
                let mut joined = Vec::with_capacity(samples);
                joined.extend_from_slice(first);
                joined.extend_from_slice(second);
                self.writer.write_interleaved(&joined)
            };
            match result {
                Ok(()) => {
                    self.stats
                        .frames_written
                        .fetch_add((samples / channels) as u64, Ordering::Relaxed);
                }
                Err(e) => failure = Some(e),
            }
        }
        // Consume the samples even if writing failed, so the audio thread keeps running.
        chunk.commit_all();
        if let Some(error) = failure {
            self.fail(error);
        }
    }

    fn flush(&mut self, sync: bool) {
        if self.error.is_some() {
            return;
        }
        match self.writer.flush(sync) {
            Ok(()) => self
                .stats
                .frames_flushed
                .store(self.writer.frames(), Ordering::Relaxed),
            Err(e) => self.fail(e),
        }
        self.last_flush = Instant::now();
    }

    fn fail(&mut self, error: DiskError) {
        self.error = Some(error.to_string());
        self.stats.failed.store(true, Ordering::Relaxed);
    }
}

enum Command {
    Start(CaptureId, Box<ActiveCapture>),
    Finish(CaptureId, Sender<Result<CaptureResult, String>>),
    Shutdown,
}

/// The disk thread and the handle to it.
pub struct Butler {
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    config: ButlerConfig,
    next_id: AtomicU64,
    /// Test hook: extra milliseconds to sleep per pass, simulating a disk that cannot keep up.
    stall_ms: Arc<AtomicU64>,
}

impl Butler {
    pub fn spawn(config: ButlerConfig) -> Butler {
        let (commands, inbox) = mpsc::channel();
        let stall_ms = Arc::new(AtomicU64::new(0));
        let stall = stall_ms.clone();
        let thread = std::thread::Builder::new()
            .name("riddim-butler".into())
            .spawn(move || butler_loop(inbox, config, stall))
            .expect("spawning the disk thread");
        Butler {
            commands,
            thread: Some(thread),
            config,
            next_id: AtomicU64::new(1),
            stall_ms,
        }
    }

    /// Creates the file and its ring, and returns the sink to give to the audio thread.
    ///
    /// The file exists (with a valid, empty header) when this returns, so a bad path fails
    /// here, on the caller's thread, not later on the butler's.
    pub fn start_capture(
        &self,
        path: &Path,
        spec: WavSpec,
    ) -> Result<(CaptureId, CaptureSink), DiskError> {
        let writer = WavWriter::create(path, spec)?;
        let capacity =
            (self.config.ring_seconds * spec.sample_rate as f64) as usize * spec.channels as usize;
        let (producer, consumer) =
            spsc::channel::<f32>(capacity.max(spec.channels as usize * 1024));
        let stats = Arc::new(CaptureStats::default());
        let id = CaptureId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let capture = ActiveCapture {
            writer,
            ring: consumer,
            path: path.to_path_buf(),
            stats: stats.clone(),
            last_flush: Instant::now(),
            error: None,
        };
        self.commands
            .send(Command::Start(id, Box::new(capture)))
            .map_err(|_| DiskError::Format("the disk thread is gone".into()))?;
        Ok((
            id,
            CaptureSink {
                ring: producer,
                channels: spec.channels as usize,
                stats,
            },
        ))
    }

    /// Asks the butler to write out whatever is still queued for `id` and close its file.
    /// The audio thread must have stopped pushing to the sink first. The result arrives on the
    /// returned channel.
    pub fn finish_capture(&self, id: CaptureId) -> Receiver<Result<CaptureResult, String>> {
        let (reply, result) = mpsc::channel();
        if self
            .commands
            .send(Command::Finish(id, reply.clone()))
            .is_err()
        {
            let _ = reply.send(Err("the disk thread is gone".into()));
        }
        result
    }

    /// Test hook: make every pass of the butler take this much longer.
    pub fn set_stall(&self, stall: Duration) {
        self.stall_ms
            .store(stall.as_millis() as u64, Ordering::Relaxed);
    }
}

impl Drop for Butler {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn finish(mut capture: ActiveCapture) -> Result<CaptureResult, String> {
    capture.drain();
    if let Some(error) = capture.error {
        return Err(format!("{}: {error}", capture.path.display()));
    }
    let dropped = capture.stats.frames_dropped.load(Ordering::Relaxed);
    let stats = capture.stats;
    let path = capture.path;
    let frames = capture
        .writer
        .finish()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    stats.frames_flushed.store(frames, Ordering::Relaxed);
    Ok(CaptureResult {
        path,
        frames,
        frames_dropped: dropped,
    })
}

fn butler_loop(inbox: Receiver<Command>, config: ButlerConfig, stall_ms: Arc<AtomicU64>) {
    let mut captures: HashMap<CaptureId, Box<ActiveCapture>> = HashMap::new();
    loop {
        match inbox.recv_timeout(config.poll_interval) {
            Ok(Command::Start(id, capture)) => {
                captures.insert(id, capture);
            }
            Ok(Command::Finish(id, reply)) => {
                let result = match captures.remove(&id) {
                    Some(capture) => finish(*capture),
                    None => Err(format!("no capture {} is running", id.0)),
                };
                let _ = reply.send(result);
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                // Leave every file valid on the way out.
                for (_, capture) in captures.drain() {
                    let _ = finish(*capture);
                }
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let stall = stall_ms.load(Ordering::Relaxed);
        if stall > 0 {
            std::thread::sleep(Duration::from_millis(stall));
        }
        for capture in captures.values_mut() {
            capture.drain();
            if capture.last_flush.elapsed() >= config.flush_interval {
                capture.flush(config.sync_on_flush);
            }
        }
    }
}
