//! Plugin hosting for Riddim.
//!
//! The rest of the engine sees plugins only through the format-agnostic contract in this crate:
//!
//! * [`PluginDescriptor`] says what a plugin is, without loading it.
//! * [`PluginInstance`] is the control side of a loaded plugin: buses, parameters, state, the
//!   editor. It lives on the main thread.
//! * [`PluginProcessor`] is the audio side, handed out by [`PluginInstance::activate`]. It is the
//!   only part that crosses to the audio thread, and its `process` is real-time safe on our side
//!   of the boundary.
//!
//! Formats are modules under [`format`]. VST3 is cross-platform; Audio Units and editor windows
//! are macOS only and compile out elsewhere.
//!
//! Scanning runs in a child process (`riddim-scan`) so that a plugin that crashes while being
//! probed cannot take the host down; see [`scan`].

#[cfg(unix)]
pub mod bridge;
pub mod check;
pub mod descriptor;
pub mod error;
pub mod event;
pub mod format;
pub mod host;
pub mod instance;
pub mod process;
pub mod scan;
pub mod state;

#[cfg(target_os = "macos")]
pub mod editor_window;

pub use descriptor::{
    BusDirection, BusInfo, BusKind, BusLayout, PluginDescriptor, PluginFormat, PluginId, PluginKind,
};
pub use error::{PluginError, Result};
pub use event::{Event, EventBody, EventBuffer};
pub use instance::{
    ActivateConfig, EditorSize, ParamFlags, ParamId, ParamInfo, ParentWindow, PluginEvent,
    PluginInstance, PluginProcessor, ProcessMode, Tail,
};
pub use process::{ProcessContext, ProcessStatus, TransportInfo};
pub use state::StateBlob;
