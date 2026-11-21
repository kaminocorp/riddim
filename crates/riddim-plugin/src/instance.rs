//! The format-agnostic plugin contract.

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::descriptor::{BusDirection, BusLayout, PluginDescriptor};
use crate::error::{PluginError, Result};
use crate::process::{ProcessContext, ProcessStatus};
use crate::state::StateBlob;

/// Parameter identifier, unique within one plugin instance.
pub type ParamId = u32;

/// How the plugin will be driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessMode {
    Realtime,
    /// Faster or slower than real time; the plugin may wait for its own disk streaming.
    Offline,
}

/// Arguments to [`PluginInstance::activate`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActivateConfig {
    pub sample_rate: f64,
    /// Largest block `process` will ever be called with.
    pub max_block: u32,
    pub mode: ProcessMode,
}

/// Static description of one parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    pub id: ParamId,
    pub name: String,
    pub unit: String,
    /// Default value, normalised to 0..=1.
    pub default: f64,
    /// 0 for continuous parameters, otherwise the number of steps (1 is a toggle).
    pub steps: u32,
    pub flags: ParamFlags,
}

/// Parameter capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParamFlags {
    pub automatable: bool,
    pub read_only: bool,
    pub hidden: bool,
    pub bypass: bool,
}

/// How long a plugin keeps sounding after its input stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    Samples(u32),
    Infinite,
}

/// Something the plugin told the host outside of `process`. Collected with
/// [`PluginInstance::poll_events`] on the main thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PluginEvent {
    /// The user grabbed a control in the plugin's editor.
    ParamGestureBegin { id: ParamId },
    /// The editor changed a parameter. The host must forward the value to the processor.
    ParamChanged { id: ParamId, value: f64 },
    ParamGestureEnd { id: ParamId },
    /// Several parameter values changed at once (a preset load, say); re-read what you cache.
    ParamValuesChanged,
    /// Parameter names, ranges or the list itself changed.
    ParamInfoChanged,
    /// [`PluginInstance::latency_samples`] has a new value.
    LatencyChanged,
    /// Bus layout changed; the instance must be deactivated and reactivated.
    IoChanged,
    /// The plugin asks to be deactivated and reactivated.
    RestartRequested,
    /// State changed in a way parameters do not capture; the project is dirty.
    StateDirty,
    /// The editor wants this size (in points). The window host answers with
    /// [`PluginInstance::set_editor_size`].
    EditorResizeRequest { width: u32, height: u32 },
}

/// Editor size in points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditorSize {
    pub width: u32,
    pub height: u32,
}

/// The native view a plugin editor is embedded in.
#[derive(Clone, Copy, Debug)]
pub enum ParentWindow {
    /// An `NSView*` on macOS.
    AppKit(NonNull<c_void>),
    /// An `HWND` on Windows.
    Win32(NonNull<c_void>),
    /// An X11 window id.
    X11(u32),
}

/// The control side of a loaded plugin.
///
/// # Threading
///
/// An instance belongs to the thread that created it, which must be the application's main
/// thread whenever an editor will be opened (AppKit and the VST3 edit controller both insist).
/// It is deliberately not `Send`. The only thing that crosses to the audio thread is the
/// [`PluginProcessor`] returned by [`activate`](Self::activate).
pub trait PluginInstance {
    fn descriptor(&self) -> &PluginDescriptor;

    /// Current buses, including which are active.
    fn bus_layout(&self) -> BusLayout;

    /// Turns a bus on or off. Only valid while the instance is not active.
    fn set_bus_active(&mut self, direction: BusDirection, index: usize, active: bool)
        -> Result<()>;

    /// Prepares the plugin for processing and returns its audio side.
    fn activate(&mut self, config: &ActivateConfig) -> Result<Box<dyn PluginProcessor>>;

    /// Takes the audio side back and stops processing. The processor must no longer be in use by
    /// the audio thread.
    fn deactivate(&mut self, processor: Box<dyn PluginProcessor>);

    fn is_active(&self) -> bool;

    /// Enumerates parameters. Allocates; cache the result.
    fn params(&self) -> Vec<ParamInfo>;

    /// Current normalised value as the plugin's control side sees it.
    fn param_value(&self, id: ParamId) -> Option<f64>;

    /// Tells the plugin's control side (and so its editor) about a value the host applied.
    /// This does *not* reach the processor: send an [`EventBody::Param`](crate::EventBody::Param)
    /// through `process` for that.
    fn set_param_value(&mut self, id: ParamId, value: f64);

    /// Display text for a normalised value, e.g. "-6.0 dB".
    fn param_text(&self, id: ParamId, value: f64) -> Option<String>;

    fn save_state(&mut self) -> Result<StateBlob>;

    fn restore_state(&mut self, state: &StateBlob) -> Result<()>;

    /// Processing latency in samples at the active sample rate.
    fn latency_samples(&self) -> u32;

    fn tail(&self) -> Tail;

    /// Drains notifications the plugin has posted since the last call.
    fn poll_events(&mut self, out: &mut Vec<PluginEvent>);

    fn has_editor(&self) -> bool {
        false
    }

    /// Creates the editor inside `parent` and returns its initial size. Main thread only.
    fn open_editor(&mut self, _parent: ParentWindow) -> Result<EditorSize> {
        Err(PluginError::Unsupported("this plugin has no editor"))
    }

    /// Destroys the editor view. Main thread only. A no-op if no editor is open.
    fn close_editor(&mut self) {}

    fn is_editor_open(&self) -> bool {
        false
    }

    fn editor_resizable(&self) -> bool {
        false
    }

    /// Asks the editor to take `size`. Returns the size it actually accepted, which the window
    /// must then adopt.
    fn set_editor_size(&mut self, _size: EditorSize) -> Result<EditorSize> {
        Err(PluginError::Unsupported("this plugin has no editor"))
    }
}

/// The audio side of an active plugin.
///
/// Everything here is called on the audio thread (or a render worker). Our side of each
/// implementation neither allocates nor locks; what the plugin's own code does is its business.
pub trait PluginProcessor: Send {
    /// Renders one block.
    fn process(&mut self, context: &mut ProcessContext<'_>) -> ProcessStatus;

    /// Clears voices, delay lines and tails, as after a transport jump.
    fn reset(&mut self) {}

    /// Upcast used by formats to recognise their own processor in
    /// [`PluginInstance::deactivate`].
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}
