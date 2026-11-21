//! State both sides of the engine read without locking.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use riddim_rt::atomic::AtomicF32;

/// Counters and flags published by the audio thread and the control thread.
#[derive(Default)]
pub struct Shared {
    /// Timeline position of the next block, in samples at the session rate.
    pub position: AtomicI64,
    pub playing: AtomicBool,
    pub recording: AtomicBool,
    /// Revision of the model the control thread holds.
    pub model_revision: AtomicU64,
    /// Sequence number of the plan the audio thread is running.
    pub plan_serial: AtomicU64,
    /// Blocks processed since the stream started.
    pub blocks: AtomicU64,
    /// Heap operations seen on the audio thread inside real-time sections. Only meaningful when
    /// the process installed `riddim_rt::guard::RtCheckAllocator`.
    pub rt_violations: AtomicU64,
    /// Frames recorded audio lost because a capture ring was full.
    pub capture_dropped: AtomicU64,
    /// Recorded frames waiting in memory for the disk, for the track furthest behind.
    pub capture_backlog: AtomicU64,
    /// Things the audio thread had to leak rather than free (a full garbage chute).
    pub leaked: AtomicU64,
    /// Latency the running plan adds between the timeline and the device output, in samples.
    pub plan_latency: AtomicU32,
    /// Messages for the control thread that did not fit in its queue.
    pub events_lost: AtomicU64,
    /// The largest share of a block's duration that rendering it has taken since this was
    /// last read and reset: 1.0 means a block took exactly as long as it lasts.
    pub dsp_load: AtomicF32,
    /// The last value of `dsp_load` that was reported: the load over the second before.
    pub dsp_load_reported: AtomicF32,
}

/// Peak meter for one strip. The audio thread raises the peaks; whoever reads them takes the
/// value and resets it, so a reader sees the maximum since it last looked.
#[derive(Default)]
pub struct MeterCell {
    peaks: [AtomicF32; 2],
}

impl MeterCell {
    #[inline]
    pub(crate) fn raise(&self, channel: usize, peak: f32) {
        if peak > self.peaks[channel].load(Ordering::Relaxed) {
            self.peaks[channel].store(peak, Ordering::Relaxed);
        }
    }

    /// Returns the peaks (left, right) since the last call and starts a new measurement.
    pub fn take(&self) -> [f32; 2] {
        [
            self.peaks[0].swap(0.0, Ordering::Relaxed),
            self.peaks[1].swap(0.0, Ordering::Relaxed),
        ]
    }
}
