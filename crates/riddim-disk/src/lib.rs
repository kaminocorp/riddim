//! Everything that touches the disk, none of it on the audio thread.
//!
//! * [`wav`]: reading WAV/RF64 files, and writing them incrementally so that a recording is a
//!   valid file after every flush, not just after a clean stop.
//! * [`capture`]: the butler. The audio thread pushes samples into pre-sized rings; one disk
//!   thread drains them into files. The audio thread never waits for it.
//! * [`resample`]: sample-rate conversion for media whose rate differs from the session's.
//! * [`smf`]: Standard MIDI Files in and out.

pub mod capture;
pub mod resample;
pub mod smf;
pub mod wav;

use std::fmt;

/// Decoded audio, one `Vec` per channel.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioData {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl AudioData {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

#[derive(Debug)]
pub enum DiskError {
    Io(std::io::Error),
    /// The file is not something we can read, and why.
    Format(String),
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskError::Io(e) => write!(f, "{e}"),
            DiskError::Format(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for DiskError {}

impl From<std::io::Error> for DiskError {
    fn from(e: std::io::Error) -> Self {
        DiskError::Io(e)
    }
}
