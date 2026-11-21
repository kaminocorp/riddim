//! The Riddim engine.
//!
//! [`Engine`] ties the other crates together. It has three sides, each on its own thread:
//!
//! * **Control** owns the model. It applies ops, compiles a render plan for every change
//!   (`riddim-graph`), hands plans to the audio side, frees retired ones, and runs recordings.
//! * **Audio** is the device callback. It drains commands, advances the transport, runs the
//!   plan, feeds capture rings and publishes position and meters. It never allocates, locks or
//!   blocks.
//! * **Main** hosts plugin instances, which must stay on one thread (see [`MainThread`]).
//!
//! A fourth, the butler (`riddim-disk`), writes recordings.
//!
//! Clients talk to the engine through three non-blocking channels: commands
//! ([`Engine::apply`], [`Engine::transport`], [`Engine::set_param`]), queries
//! ([`Engine::model`], [`Engine::status`]) and events ([`Engine::subscribe`]).

mod audio;
mod bounce;
mod control;
mod dsp;
mod engine;
mod events;
mod host;
mod media;
mod nodes;
mod plan;
mod shared;

pub use bounce::{bounce, BounceRequest, BounceResult};
pub use control::{Applied, Rejected, TransportCommand};
pub use engine::{
    DeviceConfig, Engine, EngineConfig, EngineError, MainThreadMode, Status, MAX_BUFFER_FRAMES,
    MIN_BUFFER_FRAMES,
};
pub use events::{Event, TransportState};
pub use host::MainThread;
pub use riddim_io::null::NullConfig;
