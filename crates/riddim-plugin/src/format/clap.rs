//! A CLAP host on the `clack-host` bindings.
//!
//! CLAP has no separate controller object and no bus activation: one plugin object, driven
//! from the main thread and from an audio thread, with every audio port always present.
//! Parameter values are plain numbers with a range, which are mapped here to the contract's
//! 0 to 1; a change made in the plugin's own editor comes back as an output event from
//! `process`, like a change the plugin made itself.
//!
//! Run on Linux against Surge XT's CLAP build. The editor (the `gui` extension) is written
//! for macOS and X11 and is **unverified**: nothing here has opened a window.

use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortRescanFlags, HostAudioPorts, HostAudioPortsImpl,
    PluginAudioPorts,
};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
use clack_extensions::latency::{HostLatency, HostLatencyImpl, PluginLatency};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::note_ports::{
    HostNotePorts, HostNotePortsImpl, NoteDialects, NotePortInfoBuffer, NotePortRescanFlags,
    PluginNotePorts,
};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
use clack_extensions::render::{PluginRender, RenderMode};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_host::events::event_types::{
    MidiEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent, TransportEvent, TransportFlags,
};
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::{EventFlags, EventHeader, Match, Pckn};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, ClapId, SecondsTime};
use riddim_rt::spsc;

use crate::descriptor::{
    BusDirection, BusInfo, BusKind, BusLayout, PluginDescriptor, PluginFormat, PluginId, PluginKind,
};
use crate::error::{PluginError, Result};
use crate::event::{Event, EventBody};
use crate::instance::{
    ActivateConfig, EditorSize, ParamFlags, ParamId, ParamInfo, ParentWindow, PluginEvent,
    PluginInstance, PluginProcessor, ProcessMode, Tail,
};
use crate::process::{ProcessContext, ProcessStatus as OurStatus};
use crate::state::StateBlob;

/// Events per block our buffers have room for without allocating.
const EVENT_CAPACITY: usize = 2048;

/// Where CLAP plugins are installed.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(extra) = std::env::var_os("CLAP_PATH") {
        paths.extend(std::env::split_paths(&extra));
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            paths.push(home.join("Library/Audio/Plug-Ins/CLAP"));
        }
        paths.push("/Library/Audio/Plug-Ins/CLAP".into());
    } else if cfg!(target_os = "windows") {
        if let Some(common) = std::env::var_os("COMMONPROGRAMFILES") {
            paths.push(PathBuf::from(common).join("CLAP"));
        }
    } else {
        if let Some(home) = &home {
            paths.push(home.join(".clap"));
        }
        paths.push("/usr/lib/clap".into());
        paths.push("/usr/local/lib/clap".into());
    }
    paths
}

/// Finds `.clap` files and bundles under `dirs`, without loading anything.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_clap = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("clap"));
            if is_clap {
                out.push(path);
            } else if path.is_dir() && depth < 8 {
                walk(&path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for dir in dirs {
        walk(dir, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn load_entry(path: &Path) -> Result<PluginEntry> {
    // SAFETY: loading a plugin runs its code. That is what a host does; a file that is not a
    // well-behaved CLAP plugin can do anything, which is why scanning happens in a child process.
    unsafe { PluginEntry::load(path) }
        .map_err(|e| PluginError::Load(format!("{}: {e}", path.display())))
}

fn text(value: Option<&std::ffi::CStr>) -> String {
    value.map_or_else(String::new, |s| s.to_string_lossy().into_owned())
}

/// Lists the plugins in one `.clap` file. Loads the file: call it from the scanner process.
pub fn scan_bundle(path: &Path) -> Result<Vec<PluginDescriptor>> {
    let entry = load_entry(path)?;
    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| PluginError::Load(format!("{} has no plugin factory", path.display())))?;
    let mut plugins = Vec::new();
    for descriptor in factory.plugin_descriptors() {
        let Some(id) = descriptor.id() else { continue };
        let categories: Vec<String> = descriptor
            .features()
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        let has = |feature: &str| categories.iter().any(|c| c == feature);
        let kind = if has("instrument") {
            PluginKind::Instrument
        } else if has("audio-effect") {
            PluginKind::Effect
        } else if has("note-effect") {
            PluginKind::MidiEffect
        } else {
            PluginKind::Other
        };
        plugins.push(PluginDescriptor {
            id: PluginId::new(PluginFormat::Clap, id.to_string_lossy()),
            name: text(descriptor.name()),
            vendor: text(descriptor.vendor()),
            version: text(descriptor.version()),
            kind,
            categories,
            path: Some(path.to_path_buf()),
        });
    }
    Ok(plugins)
}

// ---------------------------------------------------------------------------------------------
// Host callbacks
// ---------------------------------------------------------------------------------------------

/// The plugin's extensions, looked up once while it initialises.
#[derive(Default)]
struct Extensions {
    audio_ports: Option<PluginAudioPorts>,
    note_ports: Option<PluginNotePorts>,
    params: Option<PluginParams>,
    state: Option<PluginState>,
    latency: Option<PluginLatency>,
    render: Option<PluginRender>,
    gui: Option<PluginGui>,
}

/// What any thread may touch.
#[derive(Default)]
struct Shared {
    extensions: OnceLock<Extensions>,
    restart_requested: AtomicBool,
    callback_requested: AtomicBool,
}

impl<'a> SharedHandler<'a> for Shared {
    fn initializing(&self, instance: InitializingPluginHandle<'a>) {
        let _ = self.extensions.set(Extensions {
            audio_ports: instance.get_extension(),
            note_ports: instance.get_extension(),
            params: instance.get_extension(),
            state: instance.get_extension(),
            latency: instance.get_extension(),
            render: instance.get_extension(),
            gui: instance.get_extension(),
        });
    }

    fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Release);
    }

    fn request_process(&self) {
        // The engine processes every plugin on every block; there is nothing to wake.
    }

    fn request_callback(&self) {
        self.callback_requested.store(true, Ordering::Release);
    }
}

impl HostLogImpl for Shared {
    fn log(&self, _severity: LogSeverity, _message: &str) {
        // Plugins may log from the audio thread; taking it and doing nothing is the only
        // thing that is safe to do there.
    }
}

impl HostParamsImplShared for Shared {
    fn request_flush(&self) {
        // Parameters are flushed by `process`, which always runs.
    }
}

/// What only the main thread touches: notifications waiting for `poll_events`.
struct MainThread<'a> {
    _shared: &'a Shared,
    pending: RefCell<Vec<PluginEvent>>,
    latency_changed: Cell<bool>,
}

impl<'a> MainThreadHandler<'a> for MainThread<'a> {}

impl HostParamsImplMainThread for MainThread<'_> {
    fn rescan(&self, flags: ParamRescanFlags) {
        let mut pending = self.pending.borrow_mut();
        if flags.intersects(ParamRescanFlags::INFO | ParamRescanFlags::ALL) {
            pending.push(PluginEvent::ParamInfoChanged);
        }
        if flags
            .intersects(ParamRescanFlags::VALUES | ParamRescanFlags::TEXT | ParamRescanFlags::ALL)
        {
            pending.push(PluginEvent::ParamValuesChanged);
        }
    }

    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostLatencyImpl for MainThread<'_> {
    fn changed(&self) {
        self.latency_changed.set(true);
        self.pending.borrow_mut().push(PluginEvent::LatencyChanged);
    }
}

impl HostStateImpl for MainThread<'_> {
    fn mark_dirty(&self) {
        self.pending.borrow_mut().push(PluginEvent::StateDirty);
    }
}

impl HostAudioPortsImpl for MainThread<'_> {
    fn is_rescan_flag_supported(&self, _flag: AudioPortRescanFlags) -> bool {
        true
    }

    fn rescan(&self, _flags: AudioPortRescanFlags) {
        self.pending.borrow_mut().push(PluginEvent::IoChanged);
    }
}

impl HostNotePortsImpl for MainThread<'_> {
    fn supported_dialects(&self) -> NoteDialects {
        NoteDialects::CLAP | NoteDialects::MIDI
    }

    fn rescan(&self, _flags: NotePortRescanFlags) {
        self.pending.borrow_mut().push(PluginEvent::IoChanged);
    }
}

impl HostGuiImpl for Shared {
    fn resize_hints_changed(&self) {}

    fn request_resize(&self, _new_size: GuiSize) -> std::result::Result<(), HostError> {
        // Resizing on the plugin's request is done from the window host's side, which asks
        // the editor its size; a request arriving on another thread cannot be acted on here.
        Err(HostError::Message("resize from the window host"))
    }

    fn request_show(&self) -> std::result::Result<(), HostError> {
        Err(HostError::Message("the host shows editors"))
    }

    fn request_hide(&self) -> std::result::Result<(), HostError> {
        Err(HostError::Message("the host hides editors"))
    }

    fn closed(&self, _was_destroyed: bool) {}
}

struct Host;

impl HostHandlers for Host {
    type Shared<'a> = Shared;
    type MainThread<'a> = MainThread<'a>;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostParams>()
            .register::<HostLatency>()
            .register::<HostState>()
            .register::<HostAudioPorts>()
            .register::<HostNotePorts>()
            .register::<HostGui>();
    }
}

// ---------------------------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------------------------

/// A parameter's id and range, for mapping between CLAP's plain values and 0 to 1.
#[derive(Clone, Copy, Debug)]
struct ParamRange {
    id: u32,
    min: f64,
    max: f64,
}

impl ParamRange {
    fn normalise(&self, plain: f64) -> f64 {
        if self.max > self.min {
            ((plain - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    fn plain(&self, normalised: f64) -> f64 {
        self.min + (self.max - self.min) * normalised.clamp(0.0, 1.0)
    }
}

fn range_of(ranges: &[ParamRange], id: u32) -> Option<ParamRange> {
    ranges
        .binary_search_by_key(&id, |r| r.id)
        .ok()
        .map(|i| ranges[i])
}

/// A gesture seen by the audio side, on its way to the main thread.
#[derive(Clone, Copy)]
enum Gesture {
    Begin(u32),
    End(u32),
}

/// A loaded CLAP plugin (control side).
pub struct ClapInstance {
    descriptor: PluginDescriptor,
    instance: RefCell<clack_host::plugin::PluginInstance<Host>>,
    layout: BusLayout,
    /// Whether note input is sent as MIDI because the plugin does not take CLAP note events.
    notes_as_midi: bool,
    ranges: Arc<Vec<ParamRange>>,
    latency: Cell<u32>,
    active: bool,
    gestures: Option<spsc::Consumer<Gesture>>,
    editor_open: bool,
    // Declared last: the entry must outlive the instance made from it.
    _entry: PluginEntry,
}

/// Creates an instance of the described plugin on the calling thread.
pub fn instantiate(descriptor: &PluginDescriptor, host_name: &str) -> Result<ClapInstance> {
    let path = descriptor
        .path
        .as_deref()
        .ok_or_else(|| PluginError::NotFound(format!("{} has no path", descriptor.id)))?;
    let entry = load_entry(path)?;
    let id = CString::new(descriptor.id.uid.as_str())
        .map_err(|_| PluginError::NotFound(descriptor.id.to_string()))?;
    let host_info = HostInfo::new(
        host_name,
        "Kamino",
        "https://github.com/kaminocorp/riddim",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|e| PluginError::Load(e.to_string()))?;
    let instance = clack_host::plugin::PluginInstance::<Host>::new(
        |_| Shared::default(),
        |shared| MainThread {
            _shared: shared,
            pending: RefCell::new(Vec::new()),
            latency_changed: Cell::new(false),
        },
        &entry,
        &id,
        &host_info,
    )
    .map_err(|e| PluginError::Load(format!("{}: {e}", descriptor.id)))?;

    let mut clap = ClapInstance {
        descriptor: descriptor.clone(),
        instance: RefCell::new(instance),
        layout: BusLayout::default(),
        notes_as_midi: false,
        ranges: Arc::new(Vec::new()),
        latency: Cell::new(0),
        active: false,
        gestures: None,
        editor_open: false,
        _entry: entry,
    };
    clap.read_layout();
    clap.read_ranges();
    Ok(clap)
}

impl ClapInstance {
    fn extensions(&self) -> &Extensions {
        static NONE: OnceLock<Extensions> = OnceLock::new();
        let instance = self.instance.borrow();
        let found = instance.access_shared_handler(|shared| shared.extensions.get());
        // SAFETY of the lifetime: the extensions live in the shared handler, which lives as
        // long as the instance, which lives as long as `self`.
        match found {
            Some(extensions) => unsafe { &*(extensions as *const Extensions) },
            None => NONE.get_or_init(Extensions::default),
        }
    }

    fn read_layout(&mut self) {
        let extensions = self.extensions();
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let mut layout = BusLayout::default();
        if let Some(ports) = &extensions.audio_ports {
            let mut buffer = AudioPortInfoBuffer::new();
            for (is_input, buses) in [
                (true, &mut layout.audio_inputs),
                (false, &mut layout.audio_outputs),
            ] {
                for index in 0..ports.count(&handle, is_input) {
                    let Some(info) = ports.get(&handle, index, is_input, &mut buffer) else {
                        continue;
                    };
                    buses.push(BusInfo {
                        name: String::from_utf8_lossy(info.name).into_owned(),
                        channels: info.channel_count,
                        kind: if info.flags.contains(AudioPortFlags::IS_MAIN) {
                            BusKind::Main
                        } else {
                            BusKind::Aux
                        },
                        // CLAP ports cannot be switched off: every one is always processed.
                        active: true,
                    });
                }
            }
        }
        let mut notes_as_midi = false;
        if let Some(ports) = &extensions.note_ports {
            layout.event_inputs = ports.count(&handle, true);
            layout.event_outputs = ports.count(&handle, false);
            let mut buffer = NotePortInfoBuffer::new();
            if let Some(info) = ports.get(&handle, 0, true, &mut buffer) {
                notes_as_midi = !info.supported_dialects.contains(NoteDialects::CLAP)
                    && info.supported_dialects.contains(NoteDialects::MIDI);
            }
        }
        drop(instance);
        self.layout = layout;
        self.notes_as_midi = notes_as_midi;
    }

    fn read_ranges(&mut self) {
        let extensions = self.extensions();
        let Some(params) = &extensions.params else {
            return;
        };
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let mut buffer = ParamInfoBuffer::new();
        let mut ranges: Vec<ParamRange> = (0..params.count(&handle))
            .filter_map(|index| {
                params
                    .get_info(&handle, index, &mut buffer)
                    .map(|info| ParamRange {
                        id: info.id.get(),
                        min: info.min_value,
                        max: info.max_value,
                    })
            })
            .collect();
        ranges.sort_by_key(|r| r.id);
        ranges.dedup_by_key(|r| r.id);
        drop(instance);
        self.ranges = Arc::new(ranges);
    }

    fn read_latency(&self) {
        if let Some(latency) = &self.extensions().latency {
            let mut instance = self.instance.borrow_mut();
            let handle = instance.plugin_handle();
            self.latency.set(latency.get(&handle));
        }
    }
}

impl PluginInstance for ClapInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    fn bus_layout(&self) -> BusLayout {
        self.layout.clone()
    }

    fn set_bus_active(&mut self, _: BusDirection, _: usize, _: bool) -> Result<()> {
        Err(PluginError::Unsupported(
            "CLAP audio ports cannot be switched on and off",
        ))
    }

    fn activate(&mut self, config: &ActivateConfig) -> Result<Box<dyn PluginProcessor>> {
        if self.active {
            return Err(PluginError::InvalidState("already active"));
        }
        // The layout and the parameter list may have changed while inactive.
        self.read_layout();
        self.read_ranges();
        // Tell a plugin that wants to know whether this is a render with no clock behind it:
        // a sampler may then wait for its disk instead of dropping notes, and anything may
        // choose its better, slower algorithms. A plugin that declines is run as it is.
        if let Some(render) = &self.extensions().render {
            let mode = match config.mode {
                ProcessMode::Offline => RenderMode::Offline,
                ProcessMode::Realtime => RenderMode::Realtime,
            };
            let mut instance = self.instance.borrow_mut();
            let _ = render.set(&instance.plugin_handle(), mode);
        }
        let stopped = self
            .instance
            .borrow_mut()
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate: config.sample_rate,
                    min_frames_count: 1,
                    max_frames_count: config.max_block.max(1),
                },
            )
            .map_err(|e| PluginError::Plugin(format!("activate: {e}")))?;
        self.active = true;
        // Latency may only be asked for while the plugin is active.
        self.read_latency();

        let ports = |buses: &[BusInfo]| {
            buses
                .iter()
                .map(|b| b.channels as usize)
                .collect::<Vec<_>>()
        };
        let inputs = ports(&self.layout.audio_inputs);
        let outputs = ports(&self.layout.audio_outputs);
        let in_channels: usize = inputs.iter().sum();
        let out_channels: usize = outputs.iter().sum();
        let (gesture_tx, gesture_rx) = spsc::channel(256);
        self.gestures = Some(gesture_rx);
        Ok(Box::new(ClapProcessor {
            processor: Some(stopped.into()),
            input_ports: AudioPorts::with_capacity(in_channels, inputs.len()),
            output_ports: AudioPorts::with_capacity(out_channels, outputs.len()),
            input_scratch: vec![vec![0.0; config.max_block.max(1) as usize]; in_channels],
            inputs,
            outputs,
            events_in: EventBuffer::with_capacity(EVENT_CAPACITY),
            events_out: EventBuffer::with_capacity(EVENT_CAPACITY),
            ranges: self.ranges.clone(),
            notes_as_midi: self.notes_as_midi,
            gestures: gesture_tx,
            sample_rate: config.sample_rate,
            max_block: config.max_block.max(1) as usize,
        }))
    }

    fn deactivate(&mut self, mut processor: Box<dyn PluginProcessor>) {
        let Some(ours) = processor.as_any_mut().downcast_mut::<ClapProcessor>() else {
            return;
        };
        if let Some(audio) = ours.processor.take() {
            // CLAP asks for processing to be stopped from the audio thread. The engine hands
            // the processor back only once the audio thread has let go of it, so it is
            // stopped here, on the thread that deactivates, with nothing else running it.
            let stopped = audio.into_stopped();
            self.instance.borrow_mut().deactivate(stopped);
        }
        self.active = false;
        self.gestures = None;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn params(&self) -> Vec<ParamInfo> {
        let Some(params) = &self.extensions().params else {
            return Vec::new();
        };
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let mut buffer = ParamInfoBuffer::new();
        (0..params.count(&handle))
            .filter_map(|index| {
                let info = params.get_info(&handle, index, &mut buffer)?;
                let range = ParamRange {
                    id: info.id.get(),
                    min: info.min_value,
                    max: info.max_value,
                };
                let stepped = info.flags.contains(ParamInfoFlags::IS_STEPPED);
                Some(ParamInfo {
                    id: info.id.get(),
                    name: String::from_utf8_lossy(info.name).into_owned(),
                    unit: String::new(),
                    default: range.normalise(info.default_value),
                    steps: if stepped {
                        (info.max_value - info.min_value).round().max(1.0) as u32
                    } else {
                        0
                    },
                    flags: ParamFlags {
                        automatable: info.flags.contains(ParamInfoFlags::IS_AUTOMATABLE),
                        read_only: info.flags.contains(ParamInfoFlags::IS_READONLY),
                        hidden: info.flags.contains(ParamInfoFlags::IS_HIDDEN),
                        bypass: info.flags.contains(ParamInfoFlags::IS_BYPASS),
                    },
                })
            })
            .collect()
    }

    fn param_value(&self, id: ParamId) -> Option<f64> {
        let params = self.extensions().params.as_ref()?;
        let range = range_of(&self.ranges, id)?;
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        params
            .get_value(&handle, ClapId::from_raw(id)?)
            .map(|plain| range.normalise(plain))
    }

    fn set_param_value(&mut self, _id: ParamId, _value: f64) {
        // A CLAP plugin has one set of parameters, which `process` sets through events; there
        // is no separate control side to tell.
    }

    fn param_text(&self, id: ParamId, value: f64) -> Option<String> {
        let params = self.extensions().params.as_ref()?;
        let range = range_of(&self.ranges, id)?;
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let mut buffer = [0u8; 128];
        let text = params
            .value_to_text(
                &handle,
                ClapId::from_raw(id)?,
                range.plain(value),
                &mut buffer,
            )
            .ok()?;
        let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        Some(String::from_utf8_lossy(&text[..end]).into_owned())
    }

    fn save_state(&mut self) -> Result<StateBlob> {
        let state = self
            .extensions()
            .state
            .as_ref()
            .ok_or(PluginError::Unsupported("this plugin has no state to save"))?;
        let mut data = Vec::new();
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        state
            .save(&handle, &mut data)
            .map_err(|e| PluginError::State(e.to_string()))?;
        Ok(StateBlob::new(self.descriptor.id.clone(), data))
    }

    fn restore_state(&mut self, state: &StateBlob) -> Result<()> {
        state.expect_plugin(&self.descriptor.id)?;
        let extension = self
            .extensions()
            .state
            .as_ref()
            .ok_or(PluginError::Unsupported("this plugin takes no state"))?;
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let mut reader = state.data.as_slice();
        extension
            .load(&handle, &mut reader)
            .map_err(|e| PluginError::State(e.to_string()))
    }

    fn latency_samples(&self) -> u32 {
        let changed = self
            .instance
            .borrow()
            .access_handler(|main| main.latency_changed.replace(false));
        if changed && self.active {
            self.read_latency();
        }
        self.latency.get()
    }

    fn tail(&self) -> Tail {
        Tail::Samples(0)
    }

    fn poll_events(&mut self, out: &mut Vec<PluginEvent>) {
        let mut instance = self.instance.borrow_mut();
        let (callback, restart) = instance.access_shared_handler(|shared| {
            (
                shared.callback_requested.swap(false, Ordering::AcqRel),
                shared.restart_requested.swap(false, Ordering::AcqRel),
            )
        });
        if callback {
            instance.call_on_main_thread_callback();
        }
        instance.access_handler(|main| out.append(&mut main.pending.borrow_mut()));
        drop(instance);
        if restart {
            out.push(PluginEvent::RestartRequested);
        }
        if let Some(gestures) = self.gestures.as_mut() {
            while let Ok(gesture) = gestures.pop() {
                out.push(match gesture {
                    Gesture::Begin(id) => PluginEvent::ParamGestureBegin { id },
                    Gesture::End(id) => PluginEvent::ParamGestureEnd { id },
                });
            }
        }
    }

    fn has_editor(&self) -> bool {
        let Some(gui) = &self.extensions().gui else {
            return false;
        };
        let Some(api) = platform_api() else {
            return false;
        };
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        gui.is_api_supported(
            &handle,
            GuiConfiguration {
                api_type: api,
                is_floating: false,
            },
        )
    }

    fn open_editor(&mut self, parent: ParentWindow) -> Result<EditorSize> {
        let gui = self
            .extensions()
            .gui
            .as_ref()
            .ok_or(PluginError::Unsupported("this plugin has no editor"))?;
        let api = platform_api().ok_or(PluginError::Unsupported("no editor on this platform"))?;
        let window = match parent {
            ParentWindow::AppKit(view) => {
                Window::from_generic_ptr(GuiApiType::COCOA, view.as_ptr())
            }
            ParentWindow::Win32(hwnd) => Window::from_generic_ptr(GuiApiType::WIN32, hwnd.as_ptr()),
            ParentWindow::X11(id) => Window::from_x11_handle(id as std::ffi::c_ulong),
        };
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let fail = |what: &str| PluginError::Plugin(format!("editor: {what}"));
        gui.create(
            &handle,
            GuiConfiguration {
                api_type: api,
                is_floating: false,
            },
        )
        .map_err(|e| fail(&e.to_string()))?;
        // SAFETY: the parent is a live native view handed to us by the window host, of the
        // kind the chosen API names, and it outlives the editor (closed before the window).
        let attached = unsafe { gui.set_parent(&handle, window) };
        if let Err(error) = attached {
            gui.destroy(&handle);
            return Err(fail(&error.to_string()));
        }
        let size = gui.get_size(&handle).unwrap_or(GuiSize {
            width: 640,
            height: 480,
        });
        let _ = gui.show(&handle);
        drop(instance);
        self.editor_open = true;
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self) {
        if !self.editor_open {
            return;
        }
        if let Some(gui) = &self.extensions().gui {
            let mut instance = self.instance.borrow_mut();
            let handle = instance.plugin_handle();
            gui.destroy(&handle);
        }
        self.editor_open = false;
    }

    fn is_editor_open(&self) -> bool {
        self.editor_open
    }

    fn editor_resizable(&self) -> bool {
        let Some(gui) = &self.extensions().gui else {
            return false;
        };
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        self.editor_open && gui.can_resize(&handle)
    }

    fn set_editor_size(&mut self, size: EditorSize) -> Result<EditorSize> {
        let gui = self
            .extensions()
            .gui
            .as_ref()
            .ok_or(PluginError::Unsupported("this plugin has no editor"))?;
        let mut instance = self.instance.borrow_mut();
        let handle = instance.plugin_handle();
        let wanted = GuiSize {
            width: size.width,
            height: size.height,
        };
        let accepted = gui.adjust_size(&handle, wanted).unwrap_or(wanted);
        gui.set_size(&handle, accepted)
            .map_err(|e| PluginError::Plugin(format!("editor: {e}")))?;
        Ok(EditorSize {
            width: accepted.width,
            height: accepted.height,
        })
    }
}

impl Drop for ClapInstance {
    fn drop(&mut self) {
        self.close_editor();
    }
}

/// The window API an embedded editor uses on this platform.
fn platform_api() -> Option<GuiApiType<'static>> {
    if cfg!(target_os = "macos") {
        Some(GuiApiType::COCOA)
    } else if cfg!(target_os = "windows") {
        Some(GuiApiType::WIN32)
    } else {
        Some(GuiApiType::X11)
    }
}

// ---------------------------------------------------------------------------------------------
// The audio side
// ---------------------------------------------------------------------------------------------

struct ClapProcessor {
    processor: Option<PluginAudioProcessor<Host>>,
    input_ports: AudioPorts,
    output_ports: AudioPorts,
    /// Inputs are copied here: CLAP takes them as mutable buffers.
    input_scratch: Vec<Vec<f32>>,
    /// Channels of each input and output port.
    inputs: Vec<usize>,
    outputs: Vec<usize>,
    events_in: EventBuffer,
    events_out: EventBuffer,
    ranges: Arc<Vec<ParamRange>>,
    notes_as_midi: bool,
    gestures: spsc::Producer<Gesture>,
    sample_rate: f64,
    /// The largest block the plugin was activated for.
    max_block: usize,
}

/// Hands out a slice in consecutive pieces of the given sizes.
fn split_ports<'a, T>(
    mut rest: &'a mut [T],
    sizes: &'a [usize],
) -> impl ExactSizeIterator<Item = &'a mut [T]> + 'a {
    sizes.iter().map(move |&size| {
        // (The length has to be read before the slice is taken out of `rest`.)
        let size = size.min(rest.len());
        let (head, tail) = std::mem::take(&mut rest).split_at_mut(size);
        rest = tail;
        head
    })
}

impl ClapProcessor {
    fn push_input(&mut self, event: &Event) {
        if self.events_in.len() as usize >= EVENT_CAPACITY {
            return;
        }
        let time = event.offset;
        let note = |channel: u8, key: u8, note_id: i32| {
            Pckn::new(
                0u16,
                channel as u16,
                key as u16,
                if note_id >= 0 {
                    Match::Specific(note_id as u32)
                } else {
                    Match::All
                },
            )
        };
        match event.body {
            EventBody::NoteOn { .. } | EventBody::NoteOff { .. } if self.notes_as_midi => {
                if let Some((data, _)) = event.to_midi() {
                    self.events_in.push(&MidiEvent::new(time, 0, data));
                }
            }
            EventBody::NoteOn {
                channel,
                key,
                velocity,
                note_id,
            } => self.events_in.push(&NoteOnEvent::new(
                time,
                note(channel, key, note_id),
                velocity as f64,
            )),
            EventBody::NoteOff {
                channel,
                key,
                velocity,
                note_id,
            } => self.events_in.push(&NoteOffEvent::new(
                time,
                note(channel, key, note_id),
                velocity as f64,
            )),
            EventBody::Midi { data, .. } => self.events_in.push(&MidiEvent::new(time, 0, data)),
            EventBody::Param { id, value } => {
                let (Some(range), Some(clap_id)) =
                    (range_of(&self.ranges, id), ClapId::from_raw(id))
                else {
                    return;
                };
                self.events_in.push(&ParamValueEvent::new(
                    time,
                    clap_id,
                    Pckn::match_all(),
                    range.plain(value),
                ));
            }
        }
    }
}

impl PluginProcessor for ClapProcessor {
    fn process(&mut self, context: &mut ProcessContext<'_>) -> OurStatus {
        let frames = context.frames;
        if frames > self.max_block {
            // More frames than the plugin was activated for: it was promised it would never
            // see that, so it does not.
            for channel in context.outputs.iter_mut() {
                channel.fill(0.0);
            }
            return OurStatus::Failed;
        }
        self.events_in.clear();
        for event in context.events_in {
            self.push_input(event);
        }
        self.events_out.clear();
        for (scratch, input) in self.input_scratch.iter_mut().zip(context.inputs.iter()) {
            scratch[..frames].copy_from_slice(&input[..frames]);
        }

        let info = context.transport;
        let mut flags = TransportFlags::HAS_TEMPO
            | TransportFlags::HAS_BEATS_TIMELINE
            | TransportFlags::HAS_SECONDS_TIMELINE
            | TransportFlags::HAS_TIME_SIGNATURE;
        flags.set(TransportFlags::IS_PLAYING, info.playing);
        flags.set(TransportFlags::IS_RECORDING, info.recording);
        flags.set(TransportFlags::IS_LOOP_ACTIVE, info.looping);
        let seconds =
            |quarters: f64| SecondsTime::from_float(quarters * 60.0 / info.tempo_bpm.max(1.0));
        let bar_quarters =
            info.time_sig_numerator as f64 * 4.0 / info.time_sig_denominator.max(1) as f64;
        let transport = TransportEvent {
            header: EventHeader::new_core(0, EventFlags::empty()),
            flags,
            song_pos_beats: BeatTime::from_float(info.position_quarters),
            song_pos_seconds: SecondsTime::from_float(
                info.position_samples as f64 / self.sample_rate.max(1.0),
            ),
            tempo: info.tempo_bpm,
            tempo_inc: 0.0,
            loop_start_beats: BeatTime::from_float(info.loop_start_quarters),
            loop_end_beats: BeatTime::from_float(info.loop_end_quarters),
            loop_start_seconds: seconds(info.loop_start_quarters),
            loop_end_seconds: seconds(info.loop_end_quarters),
            bar_start: BeatTime::from_float(info.bar_start_quarters),
            bar_number: (info.bar_start_quarters / bar_quarters.max(1e-9)).round() as i32,
            time_signature_numerator: info.time_sig_numerator,
            time_signature_denominator: info.time_sig_denominator,
        };

        let Some(processor) = self.processor.as_mut() else {
            return OurStatus::Failed;
        };
        let status = {
            let input_audio = self.input_ports.with_input_buffers(
                split_ports(&mut self.input_scratch, &self.inputs).map(|port| AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        port.iter_mut()
                            .map(|channel| InputChannel::variable(&mut channel[..frames])),
                    ),
                }),
            );
            let mut output_audio = self.output_ports.with_output_buffers(
                split_ports(&mut *context.outputs, &self.outputs).map(|port| AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(
                        port.iter_mut().map(|channel| &mut channel[..frames]),
                    ),
                }),
            );
            let input_events = self.events_in.as_input();
            let mut output_events = self.events_out.as_output();
            // What the plugin does inside these calls is the plugin's business; our own
            // work on either side of them stays under the real-time checks.
            riddim_rt::guard::permit(|| match processor.ensure_processing_started() {
                Ok(started) => started
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events,
                        &mut output_events,
                        Some(info.continuous_samples.max(0) as u64),
                        Some(&transport),
                    )
                    .is_ok(),
                Err(_) => false,
            })
        };
        if !status {
            for output in context.outputs.iter_mut() {
                output[..frames].fill(0.0);
            }
            return OurStatus::Failed;
        }

        // What the plugin sent back: notes for whatever comes after it, parameter changes it
        // made itself (or its editor did), and the gestures around them.
        for event in &self.events_out {
            let offset = event.header().time().min(frames.saturating_sub(1) as u32);
            let Some(core) = event.as_core_event() else {
                continue;
            };
            let key = |pckn: Pckn| match (pckn.channel, pckn.key) {
                (Match::Specific(channel), Match::Specific(key)) => {
                    Some((channel as u8, key as u8))
                }
                _ => None,
            };
            let id = |pckn: Pckn| match pckn.note_id {
                Match::Specific(id) => id as i32,
                Match::All => -1,
            };
            let body = match core {
                CoreEventSpace::NoteOn(note) => {
                    key(note.pckn()).map(|(channel, key)| EventBody::NoteOn {
                        channel,
                        key,
                        velocity: note.velocity() as f32,
                        note_id: id(note.pckn()),
                    })
                }
                CoreEventSpace::NoteOff(note) => {
                    key(note.pckn()).map(|(channel, key)| EventBody::NoteOff {
                        channel,
                        key,
                        velocity: note.velocity() as f32,
                        note_id: id(note.pckn()),
                    })
                }
                CoreEventSpace::Midi(midi) => {
                    Event::from_midi(offset, &midi.data()).map(|e| e.body)
                }
                CoreEventSpace::ParamValue(value) => value.param_id().and_then(|param| {
                    range_of(&self.ranges, param.get()).map(|range| EventBody::Param {
                        id: param.get(),
                        value: range.normalise(value.value()),
                    })
                }),
                CoreEventSpace::ParamGestureBegin(gesture) => {
                    if let Some(param) = gesture.param_id() {
                        let _ = self.gestures.push(Gesture::Begin(param.get()));
                    }
                    None
                }
                CoreEventSpace::ParamGestureEnd(gesture) => {
                    if let Some(param) = gesture.param_id() {
                        let _ = self.gestures.push(Gesture::End(param.get()));
                    }
                    None
                }
                _ => None,
            };
            if let Some(body) = body {
                context.events_out.push(Event { offset, body });
            }
        }
        OurStatus::Ok
    }

    fn reset(&mut self) {
        if let Some(processor) = self.processor.as_mut() {
            riddim_rt::guard::permit(|| processor.reset());
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
