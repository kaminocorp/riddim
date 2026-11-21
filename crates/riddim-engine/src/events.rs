//! What the engine tells its client.

use riddim_model::{ClipId, MediaId, Op, SlotId, TrackId};

/// Transport state as the client sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Stopped,
    Playing,
    Recording,
}

/// An event from the engine. Subscribe with [`Engine::subscribe`](crate::Engine::subscribe).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A batch of ops was applied. `inverses` undo it, applied first to last.
    Applied {
        ticket: u64,
        revision: u64,
        inverses: Vec<Op>,
    },
    /// A batch was refused; the model is unchanged. `index` is the op that failed.
    Rejected {
        ticket: u64,
        index: usize,
        error: String,
    },
    /// The engine changed the model itself (a finished recording). The client should treat this
    /// like its own edit: `inverses` belong on its undo stack.
    ModelChanged {
        revision: u64,
        ops: Vec<Op>,
        inverses: Vec<Op>,
    },
    Transport {
        state: TransportState,
        position_samples: i64,
    },
    /// Sent periodically while the transport is rolling.
    Position {
        samples: i64,
    },
    /// Peak levels (left, right) per track since the previous report.
    Meters {
        tracks: Vec<(TrackId, [f32; 2])>,
    },
    PluginLoaded {
        track: TrackId,
        slot: SlotId,
        latency: u32,
        parameters: usize,
    },
    PluginFailed {
        track: TrackId,
        slot: SlotId,
        error: String,
    },
    /// A plugin's parameter changed from its editor or from the plugin itself.
    PluginParam {
        slot: SlotId,
        id: u32,
        value: f64,
    },
    PluginLatencyChanged {
        slot: SlotId,
        latency: u32,
    },
    RecordingStarted {
        tracks: Vec<TrackId>,
        position_samples: i64,
    },
    /// A take was written and placed on its track.
    RecordingFinished {
        track: TrackId,
        clip: ClipId,
        media: Option<MediaId>,
        frames: u64,
    },
    /// Recorded audio was lost because the disk could not keep up.
    RecordingOverflow {
        track: TrackId,
        frames_dropped: u64,
    },
    MediaMissing {
        media: MediaId,
        error: String,
    },
    Error {
        message: String,
    },
}
