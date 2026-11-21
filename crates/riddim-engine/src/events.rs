//! What the engine tells its client.

use riddim_model::{ClipId, MediaId, Op, SlotId, TrackId};
use serde::Serialize;

/// Transport state as the client sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Stopped,
    Playing,
    Recording,
}

/// An event from the engine. Subscribe with [`Engine::subscribe`](crate::Engine::subscribe).
///
/// Events serialise as JSON objects tagged with `"event"`, which is how they cross the C ABI.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "event")]
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
    /// While rolling, the playhead left `from_samples` and carried on at `to_samples`: a loop
    /// wrapped, or the transport was located.
    Jumped {
        from_samples: i64,
        to_samples: i64,
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
    /// A loaded plugin was taken out of the mix for a moment and activated again: its extra
    /// outputs or sidechain were routed, or the buffer size changed. Its settings are intact.
    PluginRestarted {
        track: TrackId,
        slot: SlotId,
        latency: u32,
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
    /// A control in a plugin's editor was grabbed (`down`) or let go.
    PluginGesture {
        slot: SlotId,
        id: u32,
        down: bool,
    },
    /// A plugin changed many parameter values at once (a preset, say), or, with `info`, its
    /// parameter list itself (names, ranges, count). Anything cached about them is stale.
    PluginParamsChanged {
        slot: SlotId,
        info: bool,
    },
    /// A plugin's state changed in a way its parameters do not show: the project has unsaved
    /// changes.
    PluginStateDirty {
        slot: SlotId,
    },
    /// A plugin's editor wants to be this size, in points.
    PluginEditorResize {
        slot: SlotId,
        width: u32,
        height: u32,
    },
    RecordingStarted {
        tracks: Vec<TrackId>,
        position_samples: i64,
    },
    /// A take was written and placed on its track. `frames` is its length in frames for
    /// audio, and the number of notes for MIDI. Loop recording gives one of these per time
    /// round, numbered from 1 in `take`; the clips of all but one are muted.
    RecordingFinished {
        track: TrackId,
        clip: ClipId,
        media: Option<MediaId>,
        frames: u64,
        take: Option<u32>,
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
    /// The audio device now runs with these settings (after a buffer-size change).
    StreamChanged {
        sample_rate: f64,
        buffer_frames: u32,
        input_latency_frames: u32,
        output_latency_frames: u32,
    },
    /// A file being played from disk was not read in time, and a stretch of it played as
    /// silence. `total` counts the frames lost that way since the engine started.
    PlaybackStarved {
        total: u64,
    },
    /// The device missed a deadline. `total` counts them since the engine started.
    Xrun {
        total: u64,
    },
    /// Sent about once a second: the largest share of a block's duration that rendering one
    /// took in that time (1.0 is the whole of it; more is a dropout).
    Load {
        dsp: f32,
    },
    Error {
        message: String,
    },
}
