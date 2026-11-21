//! The engine model.
//!
//! This is the data the engine plays: tracks, clips, routing, plugin slots, the tempo map and
//! transport settings. It is plain, typed, serialisable Rust data with a schema version, and it
//! knows nothing about devices, threads or plugin binaries.
//!
//! The model is changed only through [`Op`]s. Applying an op validates it against the current
//! model (a clip must name media that exists, a bus cannot feed itself) and returns the
//! **inverse op**, which undoes it exactly. The application builds its undo stack out of those
//! inverses; the engine keeps none.
//!
//! Snapshots are cheap: tracks, clips and other large pieces sit behind `Arc`, so cloning a
//! [`Model`] copies a few pointers, and an edit copies only what it touches.

mod base64;
mod model;
mod ops;

pub use model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Clip, ClipContent, ClipId,
    CurveShape, LaneId, LoopRange, Media, MediaId, Metronome, MidiEvent, Model, Note, PluginOutput,
    PluginRef, PluginSlot, Send, SendId, Sidechain, SlotId, Take, Track, TrackId, TrackInput,
    TrackKind, TrackOutput, MAX_PLUGIN_OUTPUTS, SCHEMA_VERSION,
};
pub use ops::{Op, OpError, TrackChange};
pub use riddim_time as time;
