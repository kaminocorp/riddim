//! Riddim: a native audio engine.
//!
//! This crate is the engine's public Rust API. A client talks to a running [`Engine`] through
//! three channels, none of which block on audio:
//!
//! * **Commands**: [`Engine::apply`] (edit [`model::Op`]s, validated and applied atomically,
//!   each returning its inverse), [`Engine::transport`], [`Engine::set_param`].
//! * **Queries**: [`Engine::model`] (a snapshot of the model), [`Engine::status`].
//! * **Events**: [`Engine::subscribe`] (applied and rejected edits, transport, position,
//!   meters, plugin and recording notifications).
//!
//! ```no_run
//! use riddim::model::{Op, Track, TrackId, TrackKind};
//! use riddim::{Engine, EngineConfig, TransportCommand};
//!
//! let mut engine = Engine::start(EngineConfig::manual("/tmp/riddim-demo")).unwrap();
//! let applied = engine
//!     .apply_wait(vec![Op::AddTrack {
//!         index: 0,
//!         track: Track::new(TrackId(1), "Piano", TrackKind::Instrument),
//!     }])
//!     .unwrap();
//! // `applied.inverses` undoes the edit: that is all an undo stack needs.
//! engine.transport(TransportCommand::Play);
//! engine.sync();
//! let audio = engine.run(48_000); // manual device: render one second
//! assert_eq!(audio.len(), 2);
//! # let _ = applied;
//! ```
//!
//! What the engine owns and what its client owns is set out in the vision (§1.1): the engine
//! owns everything that has to be correct in time; the client owns intent and presentation,
//! including the undo stack and the project file.

pub mod import;

pub use riddim_engine::{bounce, BounceRequest, BounceResult};
pub use riddim_engine::{
    Applied, DeviceConfig, Engine, EngineConfig, EngineError, Event, MainThread, MainThreadMode,
    NullConfig, Rejected, Status, TransportCommand, TransportState, MAX_BUFFER_FRAMES,
    MAX_RENDER_THREADS, MIN_BUFFER_FRAMES, PARALLEL_FROM_STEPS,
};

/// The engine model: tracks, clips, routing, plugin slots, and the ops that edit them.
pub use riddim_model as model;

/// Integer time: superclock, beat ticks, tempo and meter maps.
pub use riddim_time as time;

/// Plugin discovery and the hosting contract.
pub mod plugins {
    pub use riddim_plugin::check;
    pub use riddim_plugin::scan::{default_targets, ScanOutcome, ScanReport, ScanTarget, Scanner};
    pub use riddim_plugin::{
        EventBody, PluginDescriptor, PluginEvent, PluginFormat, PluginId, PluginInstance,
        PluginKind, ProcessMode,
    };

    #[cfg(target_os = "macos")]
    pub use riddim_plugin::editor_window;

    use riddim_model::PluginRef;

    /// The model's reference to a scanned plugin.
    pub fn plugin_ref(descriptor: &PluginDescriptor) -> PluginRef {
        PluginRef {
            format: descriptor.id.format.as_str().to_string(),
            uid: descriptor.id.uid.clone(),
            name: descriptor.name.clone(),
            path: descriptor
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// Audio devices.
pub mod devices {
    pub use riddim_io::{DeviceId, DeviceInfo, StreamConfig, StreamInfo};

    /// The audio devices on this machine. Empty on platforms without a hardware backend.
    pub fn list() -> Vec<DeviceInfo> {
        #[cfg(target_os = "macos")]
        {
            riddim_io::coreaudio::devices()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Vec::new()
        }
    }
}

/// Audio and MIDI files.
pub mod files {
    pub use riddim_disk::wav;
    pub use riddim_disk::{smf, AudioData, DiskError};
}

/// The real-time allocation guard, for binaries that want the engine's "no heap traffic on the
/// audio thread" claim measured: install [`rt::RtCheckAllocator`] as the global allocator and
/// read [`Status::rt_violations`].
pub mod rt {
    pub use riddim_rt::guard::{allocator_installed, RtCheckAllocator};
}

/// Version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
