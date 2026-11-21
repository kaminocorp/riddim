//! The control side of a hosted VST3 plugin.

use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use vst3::Steinberg::Vst::{
    BusInfo as VstBusInfo, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler,
    IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
    IEditControllerTrait, IHostApplication, IMidiMapping, IMidiMappingTrait, ParamID,
    ParameterInfo, ProcessSetup, SpeakerArrangement, String128,
};
use vst3::Steinberg::{
    kResultOk, kResultTrue, FUnknown, IBStream, IPlugFrame, IPlugView, IPlugViewTrait,
    IPluginBaseTrait, ViewRect, TUID,
};
use vst3::{ComPtr, ComWrapper};

use super::com::{ComponentHandler, HostApplication, MemoryStream, Notifications, PlugFrame};
use super::module::{ClassEntry, Module};
use super::processor::{MidiMap, Vst3Processor};
use super::strings::from_char16;
use crate::descriptor::{BusDirection, BusInfo, BusKind, BusLayout, PluginDescriptor};
use crate::error::{PluginError, Result};
use crate::instance::{
    ActivateConfig, EditorSize, ParamFlags, ParamId, ParamInfo, ParentWindow, PluginEvent,
    PluginInstance, PluginProcessor, ProcessMode, Tail,
};
use crate::state::{Reader, StateBlob};

// Media types, bus directions and flags, as plain integers so they compare on every platform.
const MEDIA_AUDIO: i32 = 0;
const MEDIA_EVENT: i32 = 1;
const DIR_INPUT: i32 = 0;
const DIR_OUTPUT: i32 = 1;
const BUS_TYPE_MAIN: i32 = 0;
const BUS_FLAG_DEFAULT_ACTIVE: u32 = 1;

const RESTART_RELOAD_COMPONENT: i32 = 1;
const RESTART_IO_CHANGED: i32 = 1 << 1;
const RESTART_PARAM_VALUES_CHANGED: i32 = 1 << 2;
const RESTART_LATENCY_CHANGED: i32 = 1 << 3;
const RESTART_PARAM_TITLES_CHANGED: i32 = 1 << 4;
const RESTART_MIDI_CC_ASSIGNMENT_CHANGED: i32 = 1 << 5;

const PARAM_CAN_AUTOMATE: i32 = 1;
const PARAM_IS_READ_ONLY: i32 = 1 << 1;
const PARAM_IS_HIDDEN: i32 = 1 << 4;
const PARAM_IS_BYPASS: i32 = 1 << 16;

const PROCESS_MODE_REALTIME: i32 = 0;
const PROCESS_MODE_OFFLINE: i32 = 2;
const SAMPLE_32: i32 = 0;

const INFINITE_TAIL: u32 = u32::MAX;

const STATE_MAGIC: &[u8; 4] = b"RDV3";

/// A loaded VST3 plugin.
pub struct Vst3Instance {
    descriptor: PluginDescriptor,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// True when the controller is its own object and needs its own initialize/terminate.
    controller_is_separate: bool,
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    notifications: Arc<Notifications>,
    plug_frame: ComWrapper<PlugFrame>,
    layout: BusLayout,
    midi_map: Arc<MidiMap>,
    active: bool,
    view: Option<ComPtr<IPlugView>>,
    // Kept alive for as long as the plugin might call them. Declared last so they are dropped
    // after the plugin objects above.
    _handler: ComWrapper<ComponentHandler>,
    _host: ComWrapper<HostApplication>,
    _module: Rc<Module>,
}

impl Vst3Instance {
    pub(super) fn new(module: Rc<Module>, class: ClassEntry, host_name: &str) -> Result<Self> {
        let fail = |what: &str| PluginError::Plugin(format!("{}: {what}", class.descriptor.name));
        let host = HostApplication::new(host_name);
        let host_ptr = host.to_com_ptr::<IHostApplication>().unwrap();
        let host_unknown = host_ptr.as_ptr() as *mut FUnknown;
        let notifications = Notifications::new();
        let handler = ComponentHandler::new(notifications.clone());

        let component: ComPtr<IComponent> = module
            .create_instance(&class.cid)
            .ok_or_else(|| fail("the factory could not create the component"))?;
        // SAFETY: host_unknown stays alive (held in `_host`) for the life of the instance.
        if unsafe { component.initialize(host_unknown) } != kResultOk {
            return Err(fail("component initialize failed"));
        }
        let Some(processor) = component.cast::<IAudioProcessor>() else {
            // SAFETY: balanced with the successful initialize above.
            unsafe { component.terminate() };
            return Err(fail("the component is not an audio processor"));
        };

        // The edit controller is either the component itself or a second class named by it.
        let mut controller_is_separate = false;
        let mut controller = component.cast::<IEditController>();
        if controller.is_none() {
            let mut controller_cid: TUID = [0; 16];
            // SAFETY: valid out-pointer to a 16-byte id.
            let has_cid = unsafe { component.getControllerClassId(&mut controller_cid) } == kResultOk;
            if has_cid {
                if let Some(c) = module.create_instance::<IEditController>(&controller_cid) {
                    // SAFETY: as for the component.
                    if unsafe { c.initialize(host_unknown) } == kResultOk {
                        controller = Some(c);
                        controller_is_separate = true;
                    }
                }
            }
        }

        let mut connection = None;
        if let Some(controller) = &controller {
            if controller_is_separate {
                let a = component.cast::<IConnectionPoint>();
                let b = controller.cast::<IConnectionPoint>();
                if let (Some(a), Some(b)) = (a, b) {
                    // SAFETY: both pointers are live COM objects.
                    unsafe {
                        a.connect(b.as_ptr());
                        b.connect(a.as_ptr());
                    }
                    connection = Some((a, b));
                }
            }
            let handler_ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
            // SAFETY: the handler outlives the controller (held in `_handler`).
            unsafe { controller.setComponentHandler(handler_ptr.as_ptr()) };
        }

        let mut instance = Vst3Instance {
            descriptor: class.descriptor,
            component,
            processor,
            controller,
            controller_is_separate,
            connection,
            plug_frame: PlugFrame::new(notifications.clone()),
            notifications,
            layout: BusLayout::default(),
            midi_map: Arc::new(MidiMap::new()),
            active: false,
            view: None,
            _handler: handler,
            _host: host,
            _module: module,
        };
        instance.sync_controller_to_component();
        instance.layout = instance.read_bus_layout();
        Ok(instance)
    }

    /// Hands the component's state to the controller so both halves agree.
    fn sync_controller_to_component(&mut self) {
        let Some(controller) = &self.controller else { return };
        if !self.controller_is_separate {
            return;
        }
        let stream = MemoryStream::empty();
        let ptr = stream.to_com_ptr::<IBStream>().unwrap();
        // SAFETY: the stream is a live host object for the duration of both calls.
        unsafe {
            if self.component.getState(ptr.as_ptr()) == kResultOk {
                stream.rewind();
                controller.setComponentState(ptr.as_ptr());
            }
        }
    }

    fn read_buses(&self, direction: i32) -> Vec<BusInfo> {
        let mut out = Vec::new();
        // SAFETY: valid out-pointer to a zero-initialised POD struct.
        unsafe {
            for index in 0..self.component.getBusCount(MEDIA_AUDIO, direction) {
                let mut info: VstBusInfo = std::mem::zeroed();
                if self.component.getBusInfo(MEDIA_AUDIO, direction, index, &mut info) != kResultOk {
                    continue;
                }
                let is_main = info.busType as i32 == BUS_TYPE_MAIN;
                out.push(BusInfo {
                    name: from_char16(&info.name),
                    channels: info.channelCount.max(0) as u32,
                    kind: if is_main { BusKind::Main } else { BusKind::Aux },
                    // A main bus in first position is always on; everything else (sidechains,
                    // extra outputs) follows the plugin's default until the host says otherwise.
                    active: (index == 0 && is_main) || info.flags & BUS_FLAG_DEFAULT_ACTIVE != 0,
                });
            }
        }
        out
    }

    fn read_bus_layout(&self) -> BusLayout {
        // SAFETY: plain queries on a live component.
        let (event_inputs, event_outputs) = unsafe {
            (
                self.component.getBusCount(MEDIA_EVENT, DIR_INPUT).max(0) as u32,
                self.component.getBusCount(MEDIA_EVENT, DIR_OUTPUT).max(0) as u32,
            )
        };
        let mut layout = BusLayout {
            audio_inputs: self.read_buses(DIR_INPUT),
            audio_outputs: self.read_buses(DIR_OUTPUT),
            event_inputs,
            event_outputs,
        };
        // Keep the user's activation choices across a re-read.
        for (new, old) in layout.audio_inputs.iter_mut().zip(&self.layout.audio_inputs) {
            new.active = old.active;
        }
        for (new, old) in layout.audio_outputs.iter_mut().zip(&self.layout.audio_outputs) {
            new.active = old.active;
        }
        layout
    }

    /// Applies bus activation and the plugin's own speaker arrangements.
    fn configure_buses(&mut self) {
        // SAFETY: plain calls on live objects with valid pointers into local vectors.
        unsafe {
            let arrangements = |direction: i32, count: usize| -> Vec<SpeakerArrangement> {
                (0..count)
                    .map(|i| {
                        let mut arr: SpeakerArrangement = 0;
                        self.processor.getBusArrangement(direction, i as i32, &mut arr);
                        arr
                    })
                    .collect()
            };
            let mut ins = arrangements(DIR_INPUT, self.layout.audio_inputs.len());
            let mut outs = arrangements(DIR_OUTPUT, self.layout.audio_outputs.len());
            // Re-stating the plugin's own arrangements is a no-op for well-behaved plugins and
            // is what other hosts do; the result is deliberately ignored.
            self.processor.setBusArrangements(
                ins.as_mut_ptr(),
                ins.len() as i32,
                outs.as_mut_ptr(),
                outs.len() as i32,
            );
            for (i, bus) in self.layout.audio_inputs.iter().enumerate() {
                self.component.activateBus(MEDIA_AUDIO, DIR_INPUT, i as i32, bus.active as u8);
            }
            for (i, bus) in self.layout.audio_outputs.iter().enumerate() {
                self.component.activateBus(MEDIA_AUDIO, DIR_OUTPUT, i as i32, bus.active as u8);
            }
            for i in 0..self.layout.event_inputs {
                self.component.activateBus(MEDIA_EVENT, DIR_INPUT, i as i32, 1);
            }
            for i in 0..self.layout.event_outputs {
                self.component.activateBus(MEDIA_EVENT, DIR_OUTPUT, i as i32, 1);
            }
        }
        // Channel counts can change once arrangements are applied.
        self.layout = self.read_bus_layout();
    }

    /// Re-reads which MIDI controllers the plugin maps to parameters.
    fn refresh_midi_map(&self) {
        self.midi_map.clear();
        let Some(mapping) = self.controller.as_ref().and_then(|c| c.cast::<IMidiMapping>()) else {
            return;
        };
        for channel in 0..MidiMap::CHANNELS {
            for controller in 0..MidiMap::CONTROLLERS {
                let mut id: ParamID = 0;
                // SAFETY: valid out-pointer.
                let mapped = unsafe {
                    mapping.getMidiControllerAssignment(0, channel as i16, controller as i16, &mut id)
                } == kResultTrue;
                if mapped {
                    self.midi_map.set(channel, controller, Some(id));
                }
            }
        }
    }

    fn stream_state(
        &self,
        get: impl FnOnce(*mut IBStream) -> vst3::Steinberg::tresult,
    ) -> Option<Vec<u8>> {
        let stream = MemoryStream::empty();
        let ptr = stream.to_com_ptr::<IBStream>().unwrap();
        (get(ptr.as_ptr()) == kResultOk).then(|| stream.bytes())
    }
}

impl PluginInstance for Vst3Instance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    fn bus_layout(&self) -> BusLayout {
        self.layout.clone()
    }

    fn set_bus_active(
        &mut self,
        direction: BusDirection,
        index: usize,
        active: bool,
    ) -> Result<()> {
        if self.active {
            return Err(PluginError::InvalidState("buses can only change while inactive"));
        }
        let buses = match direction {
            BusDirection::Input => &mut self.layout.audio_inputs,
            BusDirection::Output => &mut self.layout.audio_outputs,
        };
        let bus = buses
            .get_mut(index)
            .ok_or_else(|| PluginError::Plugin(format!("no {direction:?} bus {index}")))?;
        bus.active = active;
        Ok(())
    }

    fn activate(&mut self, config: &ActivateConfig) -> Result<Box<dyn PluginProcessor>> {
        if self.active {
            return Err(PluginError::InvalidState("already active"));
        }
        self.configure_buses();
        self.refresh_midi_map();

        let mut setup = ProcessSetup {
            processMode: match config.mode {
                ProcessMode::Realtime => PROCESS_MODE_REALTIME,
                ProcessMode::Offline => PROCESS_MODE_OFFLINE,
            },
            symbolicSampleSize: SAMPLE_32,
            maxSamplesPerBlock: config.max_block as i32,
            sampleRate: config.sample_rate,
        };
        // SAFETY: plain lifecycle calls on live objects, in the order the VST3 workflow
        // prescribes: setupProcessing, setActive, setProcessing.
        unsafe {
            if self.processor.setupProcessing(&mut setup) != kResultOk {
                return Err(PluginError::Plugin(format!(
                    "{} rejected {} Hz / {} frames",
                    self.descriptor.name, config.sample_rate, config.max_block
                )));
            }
            if self.component.setActive(1) != kResultOk {
                return Err(PluginError::Plugin(format!(
                    "{} failed to activate",
                    self.descriptor.name
                )));
            }
            // Some plugins return kNotImplemented here; that is fine.
            self.processor.setProcessing(1);
        }
        self.active = true;
        Ok(Box::new(Vst3Processor::new(
            self.processor.clone(),
            &self.layout,
            config,
            self.midi_map.clone(),
        )))
    }

    fn deactivate(&mut self, mut processor: Box<dyn PluginProcessor>) {
        debug_assert!(
            processor.as_any_mut().is::<Vst3Processor>(),
            "deactivate was given another format's processor"
        );
        drop(processor);
        if !self.active {
            return;
        }
        // SAFETY: plain lifecycle calls on live objects.
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
        }
        self.active = false;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn params(&self) -> Vec<ParamInfo> {
        let Some(controller) = &self.controller else { return Vec::new() };
        let mut out = Vec::new();
        // SAFETY: valid out-pointer to a zero-initialised POD struct.
        unsafe {
            for index in 0..controller.getParameterCount() {
                let mut info: ParameterInfo = std::mem::zeroed();
                if controller.getParameterInfo(index, &mut info) != kResultOk {
                    continue;
                }
                out.push(ParamInfo {
                    id: info.id,
                    name: from_char16(&info.title),
                    unit: from_char16(&info.units),
                    default: info.defaultNormalizedValue,
                    steps: info.stepCount.max(0) as u32,
                    flags: ParamFlags {
                        automatable: info.flags & PARAM_CAN_AUTOMATE != 0,
                        read_only: info.flags & PARAM_IS_READ_ONLY != 0,
                        hidden: info.flags & PARAM_IS_HIDDEN != 0,
                        bypass: info.flags & PARAM_IS_BYPASS != 0,
                    },
                });
            }
        }
        out
    }

    fn param_value(&self, id: ParamId) -> Option<f64> {
        let controller = self.controller.as_ref()?;
        // SAFETY: plain query.
        Some(unsafe { controller.getParamNormalized(id) })
    }

    fn set_param_value(&mut self, id: ParamId, value: f64) {
        if let Some(controller) = &self.controller {
            // SAFETY: plain call.
            unsafe { controller.setParamNormalized(id, value.clamp(0.0, 1.0)) };
        }
    }

    fn param_text(&self, id: ParamId, value: f64) -> Option<String> {
        let controller = self.controller.as_ref()?;
        let mut text: String128 = [0; 128];
        // SAFETY: valid out-pointer to a String128.
        let ok = unsafe { controller.getParamStringByValue(id, value, &mut text) } == kResultOk;
        ok.then(|| from_char16(&text))
    }

    fn save_state(&mut self) -> Result<StateBlob> {
        // SAFETY: the streams are live host objects for the duration of each call.
        let component = self
            .stream_state(|s| unsafe { self.component.getState(s) })
            .ok_or_else(|| PluginError::State("the component would not save its state".into()))?;
        let controller = match &self.controller {
            Some(c) if self.controller_is_separate => {
                // A controller with nothing of its own to save is normal.
                self.stream_state(|s| unsafe { c.getState(s) }).unwrap_or_default()
            }
            _ => Vec::new(),
        };

        let mut data = Vec::with_capacity(4 + 8 + component.len() + 8 + controller.len());
        data.extend_from_slice(STATE_MAGIC);
        data.extend_from_slice(&(component.len() as u64).to_le_bytes());
        data.extend_from_slice(&component);
        data.extend_from_slice(&(controller.len() as u64).to_le_bytes());
        data.extend_from_slice(&controller);
        Ok(StateBlob::new(self.descriptor.id.clone(), data))
    }

    fn restore_state(&mut self, state: &StateBlob) -> Result<()> {
        state.expect_plugin(&self.descriptor.id)?;
        let mut r = Reader { bytes: &state.data, pos: 0 };
        if r.take(4)? != STATE_MAGIC {
            return Err(PluginError::State("not a VST3 state payload".into()));
        }
        let component_len = r.u64()? as usize;
        let component = r.take(component_len)?.to_vec();
        let controller_len = r.u64()? as usize;
        let controller = r.take(controller_len)?.to_vec();

        let stream = MemoryStream::new(component);
        let ptr = stream.to_com_ptr::<IBStream>().unwrap();
        // SAFETY: the streams are live host objects for the duration of each call.
        unsafe {
            if self.component.setState(ptr.as_ptr()) != kResultOk {
                return Err(PluginError::State(format!(
                    "{} rejected its component state",
                    self.descriptor.name
                )));
            }
            if let Some(c) = &self.controller {
                if self.controller_is_separate {
                    stream.rewind();
                    c.setComponentState(ptr.as_ptr());
                    if !controller.is_empty() {
                        let stream = MemoryStream::new(controller);
                        let ptr = stream.to_com_ptr::<IBStream>().unwrap();
                        c.setState(ptr.as_ptr());
                    }
                }
            }
        }
        self.notifications.push(PluginEvent::ParamValuesChanged);
        Ok(())
    }

    fn latency_samples(&self) -> u32 {
        // SAFETY: plain query.
        unsafe { self.processor.getLatencySamples() }
    }

    fn tail(&self) -> Tail {
        // SAFETY: plain query.
        match unsafe { self.processor.getTailSamples() } {
            INFINITE_TAIL => Tail::Infinite,
            n => Tail::Samples(n),
        }
    }

    fn poll_events(&mut self, out: &mut Vec<PluginEvent>) {
        let flags = self.notifications.restart_flags.swap(0, Ordering::AcqRel);
        if flags & RESTART_MIDI_CC_ASSIGNMENT_CHANGED != 0 {
            self.refresh_midi_map();
        }
        if flags & RESTART_PARAM_VALUES_CHANGED != 0 {
            out.push(PluginEvent::ParamValuesChanged);
        }
        if flags & RESTART_PARAM_TITLES_CHANGED != 0 {
            out.push(PluginEvent::ParamInfoChanged);
        }
        if flags & RESTART_LATENCY_CHANGED != 0 {
            out.push(PluginEvent::LatencyChanged);
        }
        if flags & RESTART_IO_CHANGED != 0 {
            if !self.active {
                self.layout = self.read_bus_layout();
            }
            out.push(PluginEvent::IoChanged);
        }
        if flags & RESTART_RELOAD_COMPONENT != 0 {
            out.push(PluginEvent::RestartRequested);
        }
        while let Some(event) = self.notifications.pop() {
            out.push(event);
        }
    }

    fn has_editor(&self) -> bool {
        self.controller.is_some()
    }

    fn open_editor(&mut self, parent: ParentWindow) -> Result<EditorSize> {
        if self.view.is_some() {
            return Err(PluginError::InvalidState("the editor is already open"));
        }
        let controller =
            self.controller.as_ref().ok_or(PluginError::Unsupported("this plugin has no editor"))?;
        let (parent_ptr, platform_type): (*mut c_void, &[u8]) = match parent {
            ParentWindow::AppKit(view) => (view.as_ptr(), b"NSView\0"),
            ParentWindow::Win32(hwnd) => (hwnd.as_ptr(), b"HWND\0"),
            ParentWindow::X11(_) => {
                // X11 embedding needs the host to provide IRunLoop; Linux is not a target.
                return Err(PluginError::Unsupported("VST3 editors on X11"));
            }
        };
        let platform_type = platform_type.as_ptr() as *const std::ffi::c_char;

        // SAFETY: createView returns an owned reference or null; the remaining calls follow the
        // IPlugView workflow (setFrame, attached, getSize) on the thread that owns the instance.
        unsafe {
            let view = ComPtr::from_raw(controller.createView(c"editor".as_ptr()))
                .ok_or(PluginError::Unsupported("this plugin has no editor"))?;
            if view.isPlatformTypeSupported(platform_type) != kResultTrue {
                return Err(PluginError::Unsupported("the editor does not support this window type"));
            }
            let frame = self.plug_frame.to_com_ptr::<IPlugFrame>().unwrap();
            view.setFrame(frame.as_ptr());
            if view.attached(parent_ptr, platform_type) != kResultOk {
                view.setFrame(std::ptr::null_mut());
                return Err(PluginError::Plugin(format!(
                    "{}: the editor refused to attach",
                    self.descriptor.name
                )));
            }
            let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
            view.getSize(&mut rect);
            self.view = Some(view);
            Ok(EditorSize {
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
            })
        }
    }

    fn close_editor(&mut self) {
        if let Some(view) = self.view.take() {
            // SAFETY: the view is live and attached; this is the reverse of open_editor.
            unsafe {
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
        }
    }

    fn is_editor_open(&self) -> bool {
        self.view.is_some()
    }

    fn editor_resizable(&self) -> bool {
        // SAFETY: plain query on a live view.
        self.view.as_ref().is_some_and(|v| unsafe { v.canResize() } == kResultTrue)
    }

    fn set_editor_size(&mut self, size: EditorSize) -> Result<EditorSize> {
        let view = self.view.as_ref().ok_or(PluginError::InvalidState("the editor is not open"))?;
        let mut rect =
            ViewRect { left: 0, top: 0, right: size.width as i32, bottom: size.height as i32 };
        // SAFETY: valid pointer to a local rect; the view is live.
        unsafe {
            if view.canResize() == kResultTrue {
                view.checkSizeConstraint(&mut rect);
            }
            view.onSize(&mut rect);
        }
        Ok(EditorSize {
            width: (rect.right - rect.left).max(0) as u32,
            height: (rect.bottom - rect.top).max(0) as u32,
        })
    }
}

impl Drop for Vst3Instance {
    fn drop(&mut self) {
        self.close_editor();
        // SAFETY: teardown in the reverse order of construction, on live objects.
        unsafe {
            if self.active {
                self.processor.setProcessing(0);
                self.component.setActive(0);
            }
            if let Some((a, b)) = self.connection.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(controller) = &self.controller {
                controller.setComponentHandler(std::ptr::null_mut());
                if self.controller_is_separate {
                    controller.terminate();
                }
            }
            self.component.terminate();
        }
    }
}
