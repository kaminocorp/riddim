//! Loading media for playback.
//!
//! Phase 2 keeps media in memory: a file is decoded once, converted to the session's sample
//! rate, and shared by every clip that uses it. (Read-ahead streaming from disk is planned;
//! see the decisions log.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use riddim_disk::{resample, wav};
use riddim_model::{Media, MediaId};

use crate::plan::MediaAudio;
use crate::stream::{PagedMedia, StreamStats, StreamingConfig};

pub(crate) struct MediaCache {
    media_dir: PathBuf,
    session_rate: u32,
    loaded: HashMap<MediaId, Result<Arc<MediaAudio>, String>>,
    streaming: StreamingConfig,
    stats: Arc<StreamStats>,
}

impl MediaCache {
    pub fn new(
        media_dir: PathBuf,
        session_rate: u32,
        streaming: StreamingConfig,
        stats: Arc<StreamStats>,
    ) -> Self {
        Self {
            media_dir,
            session_rate,
            loaded: HashMap::new(),
            streaming,
            stats,
        }
    }

    pub fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.media_dir.join(path)
        }
    }

    /// The decoded audio for `media`, loading it on first use. The error is remembered too, so
    /// a missing file is reported once, not on every recompile.
    pub fn get(&mut self, media: &Media) -> (Result<Arc<MediaAudio>, String>, bool) {
        if let Some(existing) = self.loaded.get(&media.id) {
            return (existing.clone(), false);
        }
        let path = self.resolve(&media.path);
        // A long file that needs no conversion is played from disk; anything else is decoded
        // (and, if need be, resampled) into memory.
        let streamed = wav::probe(&path).ok().filter(|info| {
            info.sample_rate == self.session_rate
                && info.channels > 0
                && info.frames >= self.streaming.threshold_frames
        });
        let result = match streamed {
            Some(info) => Ok(Arc::new(MediaAudio::Paged(PagedMedia::new(
                path.clone(),
                info.channels as usize,
                info.frames as usize,
                self.streaming.page_frames,
                self.stats.clone(),
            )))),
            None => wav::read(&path)
                .map(|data| {
                    let data = resample::resample(data, self.session_rate);
                    Arc::new(MediaAudio::Memory {
                        channels: data.channels,
                    })
                })
                .map_err(|e| format!("{}: {e}", path.display())),
        };
        self.loaded.insert(media.id, result.clone());
        (result, true)
    }

    /// Forgets media the model no longer has.
    pub fn retain(&mut self, keep: impl Fn(MediaId) -> bool) {
        self.loaded.retain(|id, _| keep(*id));
    }

    /// Converts a frame count at a media's own rate to session frames.
    pub fn to_session_frames(&self, frames: u64, media_rate: u32) -> i64 {
        riddim_time::mul_div_round(
            frames as i64,
            self.session_rate as i64,
            media_rate.max(1) as i64,
        )
    }
}
