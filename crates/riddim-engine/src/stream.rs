//! Playing long audio files from disk instead of from memory.
//!
//! A file above a size threshold is not decoded whole. It is cut into pages, and a reader
//! thread keeps the pages around the playhead (and around the loop start, and wherever the
//! transport has just been sent) in memory, a few seconds ahead and a little behind. The audio
//! thread finds a page with one atomic load; it never waits for the disk. If a page is not
//! there when its turn comes, that stretch plays as silence and is counted, which is what a
//! disk that cannot keep up sounds like in any recorder.
//!
//! The one exception is an engine on the manual device (tests, offline bounce), where there
//! is no clock to keep up with: there a missing page is read on the spot, so that a render
//! never depends on how fast the disk happened to be.
//!
//! Pages the reader lets go of are freed two audio blocks later: a page pointer the audio
//! thread picked up is only used within the block it was picked up in.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use riddim_disk::wav;

use crate::plan::MediaAudio;
use crate::shared::Shared;

/// How files are streamed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamingConfig {
    /// Files of the session's sample rate with at least this many frames are streamed; smaller
    /// ones, and files that have to be resampled, are decoded into memory whole.
    pub threshold_frames: u64,
    /// Frames per page.
    pub page_frames: usize,
    /// How far ahead of the playhead to keep loaded, and how far behind it.
    pub ahead_seconds: f64,
    pub behind_seconds: f64,
    /// Wait this long before every page read. For tests that need a slow disk.
    pub read_delay: Option<Duration>,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        StreamingConfig {
            // Half a minute at 48 kHz: below that, a file is cheaper to hold than to manage.
            threshold_frames: 1_440_000,
            page_frames: 32_768,
            ahead_seconds: 4.0,
            behind_seconds: 1.0,
            read_delay: None,
        }
    }
}

/// What the streamed files are doing, for whoever wants to show it.
#[derive(Default)]
pub struct StreamStats {
    /// Frames that were due but not in memory, and so played as silence.
    pub starved: AtomicU64,
    /// Pages in memory right now, over all files.
    pub pages: AtomicU64,
}

/// One page of a streamed file: `page_frames` samples per channel (the last page is padded
/// with silence).
pub(crate) struct Page {
    channels: Vec<Vec<f32>>,
}

/// A file played from disk a page at a time.
pub(crate) struct PagedMedia {
    path: PathBuf,
    channels: usize,
    frames: usize,
    page_frames: usize,
    pages: Vec<AtomicPtr<Page>>,
    stats: Arc<StreamStats>,
}

// SAFETY: pages are published and withdrawn through atomic pointers, and a withdrawn page is
// kept alive until no audio block that could have seen it is still running (see the module
// documentation).
unsafe impl Send for PagedMedia {}
unsafe impl Sync for PagedMedia {}

impl PagedMedia {
    pub fn new(
        path: PathBuf,
        channels: usize,
        frames: usize,
        page_frames: usize,
        stats: Arc<StreamStats>,
    ) -> PagedMedia {
        let page_frames = page_frames.max(64);
        let count = frames.div_ceil(page_frames);
        PagedMedia {
            path,
            channels,
            frames,
            page_frames,
            pages: (0..count)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
            stats,
        }
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    fn read_page(&self, index: usize) -> Page {
        let first = index * self.page_frames;
        let wanted = self.page_frames.min(self.frames - first);
        let mut channels = match wav::read_range(&self.path, first as u64, wanted as u64) {
            Ok(data) => data.channels,
            // A file that has gone missing or been cut short plays as silence from here on.
            Err(_) => Vec::new(),
        };
        channels.resize(self.channels, Vec::new());
        for channel in &mut channels {
            channel.resize(self.page_frames, 0.0);
        }
        Page { channels }
    }

    fn is_loaded(&self, index: usize) -> bool {
        !self.pages[index].load(Ordering::Acquire).is_null()
    }

    /// Reads a page from disk and publishes it, unless it is there already.
    fn load(&self, index: usize) {
        if self.is_loaded(index) {
            return;
        }
        let page = Box::into_raw(Box::new(self.read_page(index)));
        if self.pages[index]
            .compare_exchange(
                std::ptr::null_mut(),
                page,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // Someone published it first; ours was never visible to anyone.
            // SAFETY: we just made it and it was not shared.
            drop(unsafe { Box::from_raw(page) });
        } else {
            self.stats.pages.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Withdraws a page. The caller must keep what comes back alive until the audio thread
    /// can no longer be using it.
    fn withdraw(&self, index: usize) -> Option<Withdrawn> {
        let page = self.pages[index].swap(std::ptr::null_mut(), Ordering::AcqRel);
        if page.is_null() {
            return None;
        }
        self.stats.pages.fetch_sub(1, Ordering::Relaxed);
        Some(Withdrawn(page))
    }

    /// The samples of `channel` from `frame` on, as far as they lie in one page: at most `max`.
    ///
    /// Called on the audio thread. `Missing(n)` means the next `n` frames are not in memory;
    /// with `blocking` the page is read here and now instead (for the manual device only).
    pub fn chunk(&self, channel: usize, frame: usize, max: usize, blocking: bool) -> Chunk<'_> {
        if frame >= self.frames || max == 0 {
            return Chunk::End;
        }
        let index = frame / self.page_frames;
        let within = frame % self.page_frames;
        let count = max.min(self.page_frames - within).min(self.frames - frame);
        let mut page = self.pages[index].load(Ordering::Acquire);
        if page.is_null() && blocking {
            // No clock to keep: wait for the disk rather than play a hole.
            riddim_rt::guard::permit(|| self.load(index));
            page = self.pages[index].load(Ordering::Acquire);
        }
        if page.is_null() {
            if channel == 0 {
                self.stats
                    .starved
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
            return Chunk::Missing(count);
        }
        // SAFETY: a published page stays alive for at least the audio block in which its
        // pointer was loaded, and this reference is not kept beyond the block.
        let page = unsafe { &*page };
        Chunk::Data(&page.channels[channel.min(page.channels.len() - 1)][within..within + count])
    }
}

impl Drop for PagedMedia {
    fn drop(&mut self) {
        for page in &self.pages {
            let page = page.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !page.is_null() {
                self.stats.pages.fetch_sub(1, Ordering::Relaxed);
                // SAFETY: nothing can reach the media any more, so nothing can reach its pages.
                drop(unsafe { Box::from_raw(page) });
            }
        }
    }
}

/// A stretch of one channel of a file.
pub(crate) enum Chunk<'a> {
    Data(&'a [f32]),
    /// This many frames are not in memory.
    Missing(usize),
    /// Past the end of the file.
    End,
}

/// What the reader wants of one file this round: the file, its pages in the order to load
/// them, and the same pages as a set.
type Wanted = (Arc<MediaAudio>, Vec<usize>, BTreeSet<usize>);

/// A page taken out of circulation, waiting to be freed.
struct Withdrawn(*mut Page);

// SAFETY: sole owner of the page from the moment it was withdrawn.
unsafe impl Send for Withdrawn {}

impl Drop for Withdrawn {
    fn drop(&mut self) {
        // SAFETY: withdrawn pages are owned by whoever holds this.
        drop(unsafe { Box::from_raw(self.0) });
    }
}

/// Where a streamed file is played on the timeline: what the reader needs to know to load the
/// right part of it in time.
pub(crate) struct StreamClip {
    pub media: Arc<MediaAudio>,
    /// Timeline sample of the clip's first frame, the file frame that plays there, and for
    /// how long.
    pub start: i64,
    pub offset: i64,
    pub length: i64,
}

pub(crate) enum StreamMsg {
    /// The clips now in the project, and the loop region if one is active.
    Clips {
        clips: Vec<StreamClip>,
        loop_range: Option<(i64, i64)>,
    },
    /// The transport is about to be sent here: start loading now.
    Seek(i64),
    Shutdown,
}

/// The reader thread.
pub(crate) struct Streamer {
    tx: Sender<StreamMsg>,
    thread: Option<JoinHandle<()>>,
}

impl Streamer {
    pub fn start(config: StreamingConfig, rate: u32, shared: Arc<Shared>) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("riddim-reader".into())
            .spawn(move || reader_main(rx, config, rate, shared))?;
        Ok(Streamer {
            tx,
            thread: Some(thread),
        })
    }

    pub fn send(&self, message: StreamMsg) {
        let _ = self.tx.send(message);
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        let _ = self.tx.send(StreamMsg::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn reader_main(rx: Receiver<StreamMsg>, config: StreamingConfig, rate: u32, shared: Arc<Shared>) {
    let ahead = (config.ahead_seconds * rate as f64) as i64;
    let behind = (config.behind_seconds * rate as f64) as i64;
    let mut clips: Vec<StreamClip> = Vec::new();
    let mut loop_range: Option<(i64, i64)> = None;
    // Somewhere the transport was sent, kept in view until the playhead has got there.
    let mut seeking: Option<i64> = None;
    // Pages withdrawn, with the audio block count at the time.
    let mut limbo: Vec<(u64, Withdrawn)> = Vec::new();
    let mut shutdown = false;

    loop {
        if shutdown {
            return;
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(StreamMsg::Clips {
                clips: new,
                loop_range: range,
            }) => {
                clips = new;
                loop_range = range;
            }
            Ok(StreamMsg::Seek(position)) => seeking = Some(position),
            Ok(StreamMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let position = shared.position.load(Ordering::Acquire);
        if seeking.is_some_and(|target| (position - target).abs() <= ahead / 2) {
            seeking = None;
        }
        // The stretches of timeline to have ready: around the playhead, from the loop start
        // (it will be needed the instant the loop wraps), and around a pending seek.
        let mut windows = vec![(position - behind, position + ahead)];
        if let Some((start, end)) = loop_range {
            if position < end {
                windows.push((start, start + ahead));
            }
        }
        if let Some(target) = seeking {
            windows.push((target - behind, target + ahead));
        }

        // Which pages of which file those stretches call for, nearest the window's start first.
        let mut wanted: HashMap<*const PagedMedia, Wanted> = HashMap::new();
        for clip in &clips {
            let MediaAudio::Paged(paged) = &*clip.media else {
                continue;
            };
            for &(from, to) in &windows {
                let from = from.max(clip.start);
                let to = to.min(clip.start + clip.length);
                if to <= from {
                    continue;
                }
                let first = (clip.offset + (from - clip.start)).max(0) as usize;
                let last = (clip.offset + (to - clip.start) - 1).max(0) as usize;
                let last = last.min(paged.frames.saturating_sub(1));
                if paged.frames == 0 || first > last {
                    continue;
                }
                let entry = wanted
                    .entry(paged as *const PagedMedia)
                    .or_insert_with(|| (clip.media.clone(), Vec::new(), BTreeSet::new()));
                for index in first / paged.page_frames..=last / paged.page_frames {
                    if entry.2.insert(index) {
                        entry.1.push(index);
                    }
                }
            }
        }

        // Load what is missing. One page per file per round, round robin, so that no file
        // waits behind all of another's.
        let mut progress = true;
        let mut rounds = 0;
        while progress && rounds < 64 {
            progress = false;
            rounds += 1;
            for (media, order, _) in wanted.values() {
                let MediaAudio::Paged(paged) = &**media else {
                    continue;
                };
                if let Some(&index) = order.iter().find(|&&index| !paged.is_loaded(index)) {
                    if let Some(delay) = config.read_delay {
                        std::thread::sleep(delay);
                    }
                    paged.load(index);
                    progress = true;
                }
            }
            // Keep listening while catching up: a seek must not wait for a long backlog.
            if rx_has_news(
                &rx,
                &mut clips,
                &mut loop_range,
                &mut seeking,
                &mut shutdown,
            ) {
                break;
            }
        }

        // Let go of pages nothing calls for any more.
        let now = shared.blocks.load(Ordering::Acquire);
        let mut seen: BTreeSet<*const PagedMedia> = BTreeSet::new();
        for clip in &clips {
            let MediaAudio::Paged(paged) = &*clip.media else {
                continue;
            };
            let key = paged as *const PagedMedia;
            if !seen.insert(key) {
                continue;
            }
            let keep = wanted.get(&key).map(|entry| &entry.2);
            for index in 0..paged.pages.len() {
                if keep.is_some_and(|keep| keep.contains(&index)) {
                    continue;
                }
                if let Some(page) = paged.withdraw(index) {
                    limbo.push((now, page));
                }
            }
        }
        // A page pointer is only used within the block it was loaded in, so two blocks after
        // a page was withdrawn nobody can still be reading it.
        limbo.retain(|(withdrawn_at, _)| now < withdrawn_at + 2);
    }
}

/// Takes in whatever has arrived without waiting. Returns whether anything did.
fn rx_has_news(
    rx: &Receiver<StreamMsg>,
    clips: &mut Vec<StreamClip>,
    loop_range: &mut Option<(i64, i64)>,
    seeking: &mut Option<i64>,
    shutdown: &mut bool,
) -> bool {
    let mut news = false;
    loop {
        match rx.try_recv() {
            Ok(StreamMsg::Clips {
                clips: new,
                loop_range: range,
            }) => {
                *clips = new;
                *loop_range = range;
                news = true;
            }
            Ok(StreamMsg::Seek(position)) => {
                *seeking = Some(position);
                news = true;
            }
            Ok(StreamMsg::Shutdown) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *shutdown = true;
                return true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return news,
        }
    }
}
