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

pub(crate) struct MediaCache {
    media_dir: PathBuf,
    session_rate: u32,
    loaded: HashMap<MediaId, Result<Arc<MediaAudio>, String>>,
}

impl MediaCache {
    pub fn new(media_dir: PathBuf, session_rate: u32) -> Self {
        Self {
            media_dir,
            session_rate,
            loaded: HashMap::new(),
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
        let result = wav::read(&path)
            .map(|data| {
                let data = resample::resample(data, self.session_rate);
                Arc::new(MediaAudio {
                    channels: data.channels,
                })
            })
            .map_err(|e| format!("{}: {e}", path.display()));
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
