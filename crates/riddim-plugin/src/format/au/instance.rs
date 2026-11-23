//! The control side of a hosted Audio Unit.

use std::cell::UnsafeCell;
use std::ffi::{c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};
use objc2_audio_toolbox::{
    AURenderCallbackStruct, AudioComponent, AudioComponentInstanceNew, AudioComponentInstantiate,
    AudioComponentInstantiationOptions, AudioUnit, AudioUnitGetParameter, AudioUnitGetProperty,
    AudioUnitGetPropertyInfo, AudioUnitInitialize, AudioUnitParameter, AudioUnitParameterInfo,
    AudioUnitParameterStringFromValue, AudioUnitProperty, AudioUnitUninitialize, HostCallbackInfo,
};
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::{NSPoint, NSSize};

use super::processor::{
    beat_and_tempo, input_callback, musical_time_location, transport_state, transport_state2,
    AuProcessor, RenderState, Shared,
};
use super::sys::{self, *};
use super::view::create_view;
use crate::descriptor::{BusDirection, BusInfo, BusKind, BusLayout, PluginDescriptor};
use crate::error::{PluginError, Result};
use crate::instance::{
    ActivateConfig, EditorSize, ParamFlags, ParamId, ParamInfo, ParentWindow, PluginEvent,
    PluginInstance, PluginProcessor, ProcessMode, Tail,
};
use crate::process::TransportInfo;
use crate::state::StateBlob;

#[derive(Clone, Copy)]
struct ParamMeta {
    id: u32,
    min: f32,
    max: f32,
}

impl ParamMeta {
    fn normalize(&self, plain: f32) -> f64 {
        if self.max > self.min {
            ((plain - self.min) / (self.max - self.min)).clamp(0.0, 1.0) as f64
        } else {
            0.0
        }
    }

    fn plain(&self, normalized: f64) -> f32 {
        self.min + (self.max - self.min) * normalized.clamp(0.0, 1.0) as f32
    }
}

/// What the event listener needs: where to post, and the ranges to normalise values with.
struct ListenerContext {
    queue: ArrayQueue<PluginEvent>,
    params: Mutex<Vec<ParamMeta>>,
}

/// A loaded Audio Unit.
pub struct AuInstance {
    descriptor: PluginDescriptor,
    shared: Arc<Shared>,
    layout: BusLayout,
    params: Vec<ParamMeta>,
    listener: AUEventListenerRef,
    listener_context: Arc<ListenerContext>,
    active: bool,
    sample_rate: f64,
    view: Option<Retained<NSView>>,
    last_view_size: EditorSize,
}

/// Delivered on the instance's run loop for parameter and property changes made by the unit or
/// its view.
unsafe extern "C-unwind" fn event_listener(
    user_data: *mut c_void,
    _object: *mut c_void,
    event: *const AudioUnitEvent,
    _host_time: u64,
    value: f32,
) {
    if user_data.is_null() || event.is_null() {
        return;
    }
    // SAFETY: `user_data` is the ListenerContext kept alive by the instance until the listener
    // is disposed; the union member read matches the event type.
    unsafe {
        let context = &*(user_data as *const ListenerContext);
        let event = &*event;
        let posted = match event.event_type {
            EVENT_PARAMETER_VALUE_CHANGE => {
                let id = event.argument.parameter.mParameterID;
                let params = context.params.lock().unwrap();
                params
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| PluginEvent::ParamChanged {
                        id,
                        value: p.normalize(value),
                    })
            }
            EVENT_BEGIN_PARAMETER_CHANGE_GESTURE => Some(PluginEvent::ParamGestureBegin {
                id: event.argument.parameter.mParameterID,
            }),
            EVENT_END_PARAMETER_CHANGE_GESTURE => Some(PluginEvent::ParamGestureEnd {
                id: event.argument.parameter.mParameterID,
            }),
            EVENT_PROPERTY_CHANGE => match event.argument.property.mPropertyID {
                PROP_LATENCY => Some(PluginEvent::LatencyChanged),
                PROP_PARAMETER_LIST => Some(PluginEvent::ParamInfoChanged),
                PROP_PRESENT_PRESET | PROP_CLASS_INFO => Some(PluginEvent::ParamValuesChanged),
                _ => None,
            },
            _ => None,
        };
        if let Some(posted) = posted {
            let _ = context.queue.push(posted);
        }
    }
}

/// Instantiates through the asynchronous API, which version 3 units may require.
fn instantiate_async(component: AudioComponent) -> Result<AudioUnit> {
    let result: Arc<Mutex<Option<(usize, OSStatus)>>> = Arc::new(Mutex::new(None));
    let slot = result.clone();
    let block = block2::RcBlock::new(move |instance: AudioUnit, status: OSStatus| {
        *slot.lock().unwrap() = Some((instance as usize, status));
    });
    // SAFETY: the block stays alive until the completion handler has run or we give up.
    unsafe {
        AudioComponentInstantiate(component, AudioComponentInstantiationOptions(0), &block);
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some((instance, status)) = *result.lock().unwrap() {
            if status != NO_ERR || instance == 0 {
                return Err(PluginError::Load(format!(
                    "AudioComponentInstantiate failed ({status})"
                )));
            }
            return Ok(instance as AudioUnit);
        }
        if Instant::now() > deadline {
            return Err(PluginError::Load(
                "AudioComponentInstantiate timed out".into(),
            ));
        }
        // The completion handler may be delivered on this thread's run loop.
        sys::pump_run_loop();
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl AuInstance {
    pub(super) fn new(
        component: AudioComponent,
        component_flags: u32,
        descriptor: PluginDescriptor,
    ) -> Result<Self> {
        let unit = if component_flags & FLAG_REQUIRES_ASYNC_INSTANTIATION != 0 {
            instantiate_async(component)?
        } else {
            let mut unit: AudioUnit = std::ptr::null_mut();
            // SAFETY: valid component and out-pointer.
            let status = unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) };
            if status != NO_ERR || unit.is_null() {
                return Err(PluginError::Load(format!(
                    "{}: AudioComponentInstanceNew failed ({status})",
                    descriptor.name
                )));
            }
            unit
        };

        let shared = Arc::new(Shared {
            unit,
            render: UnsafeCell::new(RenderState {
                transport: TransportInfo::stopped(48_000.0),
                frames: 0,
                input_buses: Vec::new(),
                input_ptrs: Vec::new(),
                input_copies: Vec::new(),
                silence: Vec::new(),
            }),
            initialized: AtomicBool::new(false),
        });
        let listener_context = Arc::new(ListenerContext {
            queue: ArrayQueue::new(4096),
            params: Mutex::new(Vec::new()),
        });

        let mut instance = AuInstance {
            descriptor,
            shared,
            layout: BusLayout::default(),
            params: Vec::new(),
            listener: std::ptr::null_mut(),
            listener_context,
            active: false,
            sample_rate: 48_000.0,
            view: None,
            last_view_size: EditorSize {
                width: 0,
                height: 0,
            },
        };
        instance.layout = instance.read_bus_layout();
        instance.refresh_params();
        instance.install_listener();
        Ok(instance)
    }

    fn unit(&self) -> AudioUnit {
        self.shared.unit
    }

    fn read_buses(&self, scope: u32) -> Vec<BusInfo> {
        // SAFETY: each property is read as the type the API documents for it.
        unsafe {
            let count: u32 = get_property(self.unit(), PROP_ELEMENT_COUNT, scope, 0).unwrap_or(0);
            (0..count)
                .map(|bus| {
                    let format: Option<AudioStreamBasicDescription> =
                        get_property(self.unit(), PROP_STREAM_FORMAT, scope, bus);
                    let name: Option<*const CFString> =
                        get_property(self.unit(), PROP_ELEMENT_NAME, scope, bus);
                    let name = name
                        .and_then(|n| NonNull::new(n as *mut CFString))
                        // A returned CoreFoundation object is ours to release.
                        .map(|n| CFRetained::from_raw(n).to_string())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| {
                            let direction = if scope == SCOPE_INPUT {
                                "Input"
                            } else {
                                "Output"
                            };
                            format!("{direction} {}", bus + 1)
                        });
                    BusInfo {
                        name,
                        channels: format.map_or(0, |f| f.mChannelsPerFrame),
                        kind: if bus == 0 {
                            BusKind::Main
                        } else {
                            BusKind::Aux
                        },
                        // Audio Units have no activation flag: a bus is "active" when the host
                        // feeds or pulls it. Only the first of each starts that way.
                        active: bus == 0,
                    }
                })
                .collect()
        }
    }

    fn read_bus_layout(&self) -> BusLayout {
        let mut layout = BusLayout {
            audio_inputs: self.read_buses(SCOPE_INPUT),
            audio_outputs: self.read_buses(SCOPE_OUTPUT),
            // Instruments and music effects take MIDI; MIDI output is wired up in a later phase.
            event_inputs: matches!(
                self.descriptor.kind,
                crate::PluginKind::Instrument | crate::PluginKind::MidiEffect
            ) as u32,
            event_outputs: 0,
        };
        for (new, old) in layout
            .audio_inputs
            .iter_mut()
            .zip(&self.layout.audio_inputs)
        {
            new.active = old.active;
        }
        for (new, old) in layout
            .audio_outputs
            .iter_mut()
            .zip(&self.layout.audio_outputs)
        {
            new.active = old.active;
        }
        layout
    }

    fn parameter_ids(&self) -> Vec<u32> {
        let mut size = 0u32;
        let mut writable = 0u8;
        // SAFETY: the list is read into a buffer of exactly the size the unit reported.
        unsafe {
            if AudioUnitGetPropertyInfo(
                self.unit(),
                PROP_PARAMETER_LIST,
                SCOPE_GLOBAL,
                0,
                &mut size,
                &mut writable,
            ) != NO_ERR
                || size == 0
            {
                return Vec::new();
            }
            let mut ids = vec![0u32; size as usize / 4];
            let status = AudioUnitGetProperty(
                self.unit(),
                PROP_PARAMETER_LIST,
                SCOPE_GLOBAL,
                0,
                NonNull::new_unchecked(ids.as_mut_ptr() as *mut c_void),
                NonNull::from(&mut size),
            );
            if status != NO_ERR {
                return Vec::new();
            }
            ids.truncate(size as usize / 4);
            ids
        }
    }

    fn parameter_info(&self, id: u32) -> Option<AudioUnitParameterInfo> {
        // SAFETY: PROP_PARAMETER_INFO yields an AudioUnitParameterInfo.
        unsafe { get_property(self.unit(), PROP_PARAMETER_INFO, SCOPE_GLOBAL, id) }
    }

    /// Re-reads parameter ranges and shares them with the event listener.
    fn refresh_params(&mut self) {
        self.params = self
            .parameter_ids()
            .into_iter()
            .filter_map(|id| {
                let info = self.parameter_info(id)?;
                release_param_strings(&info);
                Some(ParamMeta {
                    id,
                    min: info.minValue,
                    max: info.maxValue,
                })
            })
            .collect();
        *self.listener_context.params.lock().unwrap() = self.params.clone();
    }

    fn meta(&self, id: u32) -> Option<ParamMeta> {
        self.params.iter().find(|p| p.id == id).copied()
    }

    fn install_listener(&mut self) {
        let user_data = Arc::as_ptr(&self.listener_context) as *mut c_void;
        // SAFETY: the context outlives the listener (disposed in Drop before the Arc goes); the
        // listener delivers on this thread's run loop.
        unsafe {
            let mut listener: AUEventListenerRef = std::ptr::null_mut();
            let status = AUEventListenerCreate(
                event_listener,
                user_data,
                CFRunLoopGetCurrent(),
                kCFRunLoopDefaultMode,
                0.02,
                0.0,
                &mut listener,
            );
            if status != NO_ERR || listener.is_null() {
                return;
            }
            self.listener = listener;
            for param in &self.params {
                let parameter = AudioUnitParameter {
                    mAudioUnit: self.unit(),
                    mParameterID: param.id,
                    mScope: SCOPE_GLOBAL,
                    mElement: 0,
                };
                for event_type in [
                    EVENT_PARAMETER_VALUE_CHANGE,
                    EVENT_BEGIN_PARAMETER_CHANGE_GESTURE,
                    EVENT_END_PARAMETER_CHANGE_GESTURE,
                ] {
                    let event = AudioUnitEvent {
                        event_type,
                        argument: AudioUnitEventArgument { parameter },
                    };
                    AUEventListenerAddEventType(listener, std::ptr::null_mut(), &event);
                }
            }
            for property in [PROP_LATENCY, PROP_PARAMETER_LIST, PROP_PRESENT_PRESET] {
                let property = AudioUnitProperty {
                    mAudioUnit: self.unit(),
                    mPropertyID: property,
                    mScope: SCOPE_GLOBAL,
                    mElement: 0,
                };
                let event = AudioUnitEvent {
                    event_type: EVENT_PROPERTY_CHANGE,
                    argument: AudioUnitEventArgument { property },
                };
                AUEventListenerAddEventType(listener, std::ptr::null_mut(), &event);
            }
        }
    }

    fn uninitialize(&mut self) {
        if self.shared.initialized.swap(false, Ordering::AcqRel) {
            // SAFETY: plain lifecycle call on a live unit.
            unsafe { AudioUnitUninitialize(self.unit()) };
        }
    }

    fn view_size(view: &NSView) -> EditorSize {
        let size = view.frame().size;
        EditorSize {
            width: size.width.max(0.0) as u32,
            height: size.height.max(0.0) as u32,
        }
    }
}

/// Releases the CoreFoundation strings a parameter-info query may hand back.
fn release_param_strings(info: &AudioUnitParameterInfo) {
    // SAFETY: with CFNameRelease set, the host owns the name string; the unit string follows
    // the same rule for custom units.
    unsafe {
        if info.flags.0 & PARAM_FLAG_CF_NAME_RELEASE != 0 {
            if info.flags.0 & PARAM_FLAG_HAS_CF_NAME_STRING != 0 && !info.cfNameString.is_null() {
                CFRelease(info.cfNameString as CFTypeRef);
            }
            if info.unit.0 == UNIT_CUSTOM && !info.unitName.is_null() {
                CFRelease(info.unitName as CFTypeRef);
            }
        }
    }
}

fn unit_label(unit: u32) -> &'static str {
    match unit {
        3 => "%",
        4 => "s",
        8 => "Hz",
        9 => "cents",
        10 => "semitones",
        13 => "dB",
        24 => "ms",
        _ => "",
    }
}

impl PluginInstance for AuInstance {
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
            return Err(PluginError::InvalidState(
                "buses can only change while inactive",
            ));
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
        if Arc::strong_count(&self.shared) > 1 {
            return Err(PluginError::InvalidState(
                "the previous processor is still alive",
            ));
        }
        self.uninitialize();
        let unit = self.unit();
        let max_block = config.max_block as usize;
        let fail = |what: &str, status: OSStatus| {
            PluginError::Plugin(format!(
                "{}: {what} failed ({status})",
                self.descriptor.name
            ))
        };

        // SAFETY: each property is written with the type the API documents for it, while the
        // unit is uninitialised.
        unsafe {
            for (scope, buses) in [
                (SCOPE_INPUT, &self.layout.audio_inputs),
                (SCOPE_OUTPUT, &self.layout.audio_outputs),
            ] {
                for (bus, info) in buses.iter().enumerate() {
                    let format = float_format(config.sample_rate, info.channels);
                    let status = set_property(unit, PROP_STREAM_FORMAT, scope, bus as u32, &format);
                    // Units may refuse a format on buses we are not going to use.
                    if status != NO_ERR && info.active {
                        return Err(fail("setting the stream format", status));
                    }
                }
            }
            let status = set_property(
                unit,
                PROP_MAXIMUM_FRAMES_PER_SLICE,
                SCOPE_GLOBAL,
                0,
                &config.max_block,
            );
            if status != NO_ERR {
                return Err(fail("setting the maximum block size", status));
            }
            let offline = (config.mode == ProcessMode::Offline) as u32;
            set_property(unit, PROP_OFFLINE_RENDER, SCOPE_GLOBAL, 0, &offline);
        }

        // Size everything the render callbacks will touch.
        let mut input_buses = Vec::new();
        let mut first = 0;
        for bus in &self.layout.audio_inputs {
            let channels = if bus.active { bus.channels as usize } else { 0 };
            input_buses.push((first, channels));
            first += channels;
        }
        // SAFETY: no processor exists (checked above) and the unit is uninitialised, so nothing
        // else can be reading the render state.
        unsafe {
            *self.shared.render.get() = RenderState {
                transport: TransportInfo::stopped(config.sample_rate),
                frames: 0,
                input_buses,
                input_ptrs: vec![std::ptr::null(); first],
                input_copies: vec![vec![0.0; max_block.max(1)]; first],
                silence: vec![0.0; max_block.max(1)],
            };
        }

        let ref_con = Arc::as_ptr(&self.shared) as *mut c_void;
        // SAFETY: the callbacks' context is the Shared block, which stays alive for as long as
        // the unit exists.
        unsafe {
            let host = HostCallbackInfo {
                hostUserData: ref_con,
                beatAndTempoProc: Some(beat_and_tempo),
                musicalTimeLocationProc: Some(musical_time_location),
                transportStateProc: Some(transport_state),
                transportStateProc2: Some(transport_state2),
            };
            set_property(unit, PROP_HOST_CALLBACKS, SCOPE_GLOBAL, 0, &host);

            for (bus, info) in self.layout.audio_inputs.iter().enumerate() {
                if !info.active {
                    continue;
                }
                let callback = AURenderCallbackStruct {
                    inputProc: Some(input_callback),
                    inputProcRefCon: ref_con,
                };
                let status = set_property(
                    unit,
                    PROP_SET_RENDER_CALLBACK,
                    SCOPE_INPUT,
                    bus as u32,
                    &callback,
                );
                if status != NO_ERR {
                    return Err(fail("installing the input callback", status));
                }
            }

            let status = AudioUnitInitialize(unit);
            if status != NO_ERR {
                return Err(fail("AudioUnitInitialize", status));
            }
        }
        self.shared.initialized.store(true, Ordering::Release);
        self.sample_rate = config.sample_rate;
        self.layout = self.read_bus_layout();
        self.active = true;

        let output_buses = self
            .layout
            .audio_outputs
            .iter()
            .enumerate()
            .filter(|(_, bus)| bus.active)
            .map(|(index, bus)| (index as u32, bus.channels as usize))
            .collect();
        let ranges = self.params.iter().map(|p| (p.id, p.min, p.max)).collect();
        Ok(Box::new(AuProcessor::new(
            self.shared.clone(),
            output_buses,
            ranges,
            max_block,
        )))
    }

    fn deactivate(&mut self, mut processor: Box<dyn PluginProcessor>) {
        debug_assert!(
            processor.as_any_mut().is::<AuProcessor>(),
            "deactivate was given another format's processor"
        );
        drop(processor);
        self.uninitialize();
        self.active = false;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn params(&self) -> Vec<ParamInfo> {
        self.parameter_ids()
            .into_iter()
            .filter_map(|id| {
                let info = self.parameter_info(id)?;
                let flags = info.flags.0;
                // SAFETY: the name fields are valid as described by the flags.
                let name = unsafe {
                    if flags & PARAM_FLAG_HAS_CF_NAME_STRING != 0 && !info.cfNameString.is_null() {
                        (*info.cfNameString).to_string()
                    } else {
                        CStr::from_ptr(info.name.as_ptr())
                            .to_string_lossy()
                            .into_owned()
                    }
                };
                // SAFETY: a custom unit carries its own label string.
                let unit = unsafe {
                    if info.unit.0 == UNIT_CUSTOM && !info.unitName.is_null() {
                        (*info.unitName).to_string()
                    } else {
                        unit_label(info.unit.0).to_string()
                    }
                };
                release_param_strings(&info);
                let meta = ParamMeta {
                    id,
                    min: info.minValue,
                    max: info.maxValue,
                };
                let stepped = matches!(info.unit.0, UNIT_INDEXED | UNIT_BOOLEAN);
                let writable = flags & PARAM_FLAG_IS_WRITABLE != 0;
                Some(ParamInfo {
                    id,
                    name,
                    unit,
                    default: meta.normalize(info.defaultValue),
                    steps: if stepped {
                        (info.maxValue - info.minValue).max(0.0) as u32
                    } else {
                        0
                    },
                    flags: ParamFlags {
                        automatable: writable,
                        read_only: !writable,
                        hidden: flags & PARAM_FLAG_IS_READABLE == 0 && !writable,
                        bypass: false,
                    },
                })
            })
            .collect()
    }

    fn param_value(&self, id: ParamId) -> Option<f64> {
        let meta = self.meta(id)?;
        let mut value = 0.0f32;
        // SAFETY: valid out-pointer.
        let status = unsafe {
            AudioUnitGetParameter(self.unit(), id, SCOPE_GLOBAL, 0, NonNull::from(&mut value))
        };
        (status == NO_ERR).then(|| meta.normalize(value))
    }

    fn set_param_value(&mut self, id: ParamId, value: f64) {
        let Some(meta) = self.meta(id) else { return };
        let parameter = AudioUnitParameter {
            mAudioUnit: self.unit(),
            mParameterID: id,
            mScope: SCOPE_GLOBAL,
            mElement: 0,
        };
        // SAFETY: AUParameterSet sets the value and notifies every listener except ours, so the
        // unit's view follows without the change echoing back to us.
        unsafe {
            AUParameterSet(
                self.listener,
                std::ptr::null_mut(),
                &parameter,
                meta.plain(value),
                0,
            )
        };
    }

    fn param_text(&self, id: ParamId, value: f64) -> Option<String> {
        let meta = self.meta(id)?;
        let mut plain = meta.plain(value);
        // SAFETY: the unit fills `outString` with a string we then own.
        unsafe {
            let mut request = AudioUnitParameterStringFromValue {
                inParamID: id,
                inValue: NonNull::from(&mut plain),
                outString: std::ptr::null(),
            };
            let mut size = std::mem::size_of::<AudioUnitParameterStringFromValue>() as u32;
            let status = AudioUnitGetProperty(
                self.unit(),
                PROP_PARAMETER_STRING_FROM_VALUE,
                SCOPE_GLOBAL,
                0,
                NonNull::from(&mut request).cast(),
                NonNull::from(&mut size),
            );
            if status == NO_ERR {
                if let Some(text) = NonNull::new(request.outString as *mut CFString) {
                    return Some(CFRetained::from_raw(text).to_string());
                }
            }
        }
        Some(format!("{plain:.2}"))
    }

    fn save_state(&mut self) -> Result<StateBlob> {
        // SAFETY: ClassInfo yields a property list we own; it is serialised and released.
        unsafe {
            let plist: CFTypeRef = get_property(self.unit(), PROP_CLASS_INFO, SCOPE_GLOBAL, 0)
                .filter(|p: &CFTypeRef| !p.is_null())
                .ok_or_else(|| PluginError::State("the unit would not save its state".into()))?;
            // XML rather than binary: the XML writer orders dictionary keys, which is what makes
            // save -> restore -> save comparable byte for byte.
            let data = CFPropertyListCreateData(
                std::ptr::null(),
                plist,
                PLIST_FORMAT_XML,
                0,
                std::ptr::null_mut(),
            );
            CFRelease(plist);
            if data.is_null() {
                return Err(PluginError::State(
                    "the unit's state is not a property list".into(),
                ));
            }
            let bytes = std::slice::from_raw_parts(
                CFDataGetBytePtr(data),
                CFDataGetLength(data).max(0) as usize,
            )
            .to_vec();
            CFRelease(data);
            Ok(StateBlob::new(self.descriptor.id.clone(), bytes))
        }
    }

    fn restore_state(&mut self, state: &StateBlob) -> Result<()> {
        state.expect_plugin(&self.descriptor.id)?;
        // SAFETY: the data and property list are created, used and released here.
        unsafe {
            let data = CFDataCreate(
                std::ptr::null(),
                state.data.as_ptr(),
                state.data.len() as isize,
            );
            if data.is_null() {
                return Err(PluginError::State("could not wrap the state bytes".into()));
            }
            let plist = CFPropertyListCreateWithData(
                std::ptr::null(),
                data,
                PLIST_IMMUTABLE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            CFRelease(data);
            if plist.is_null() {
                return Err(PluginError::State(
                    "the state is not a property list".into(),
                ));
            }
            let status = set_property(self.unit(), PROP_CLASS_INFO, SCOPE_GLOBAL, 0, &plist);
            CFRelease(plist);
            if status != NO_ERR {
                return Err(PluginError::State(format!(
                    "{} rejected its state ({status})",
                    self.descriptor.name
                )));
            }
            // Tell every listener (the unit's view included) that all parameters may have moved.
            let any = AudioUnitParameter {
                mAudioUnit: self.unit(),
                mParameterID: ANY_PARAMETER,
                mScope: SCOPE_GLOBAL,
                mElement: 0,
            };
            AUParameterListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &any);
        }
        let _ = self
            .listener_context
            .queue
            .push(PluginEvent::ParamValuesChanged);
        Ok(())
    }

    fn latency_samples(&self) -> u32 {
        // SAFETY: Latency is a Float64 in seconds.
        let seconds: f64 =
            unsafe { get_property(self.unit(), PROP_LATENCY, SCOPE_GLOBAL, 0) }.unwrap_or(0.0);
        (seconds.max(0.0) * self.sample_rate).round() as u32
    }

    fn tail(&self) -> Tail {
        // SAFETY: TailTime is a Float64 in seconds.
        let seconds: f64 =
            unsafe { get_property(self.unit(), PROP_TAIL_TIME, SCOPE_GLOBAL, 0) }.unwrap_or(0.0);
        if seconds.is_infinite() {
            Tail::Infinite
        } else {
            Tail::Samples((seconds.max(0.0) * self.sample_rate).round() as u32)
        }
    }

    fn poll_events(&mut self, out: &mut Vec<PluginEvent>) {
        // Listener callbacks arrive through this thread's run loop; give it a turn so headless
        // hosts (no application event loop) still see them.
        sys::pump_run_loop();
        let mut params_changed = false;
        while let Some(event) = self.listener_context.queue.pop() {
            params_changed |= event == PluginEvent::ParamInfoChanged;
            out.push(event);
        }
        if params_changed {
            self.refresh_params();
        }
        if let Some(view) = &self.view {
            let size = Self::view_size(view);
            if size != self.last_view_size {
                self.last_view_size = size;
                out.push(PluginEvent::EditorResizeRequest {
                    width: size.width,
                    height: size.height,
                });
            }
        }
    }

    fn has_editor(&self) -> bool {
        // Every unit has at least the generic parameter view.
        true
    }

    fn open_editor(&mut self, parent: ParentWindow) -> Result<EditorSize> {
        if self.view.is_some() {
            return Err(PluginError::InvalidState("the editor is already open"));
        }
        let ParentWindow::AppKit(parent) = parent else {
            return Err(PluginError::Unsupported(
                "Audio Unit editors need an NSView parent",
            ));
        };
        let mtm = MainThreadMarker::new().ok_or(PluginError::Unsupported(
            "editors can only be opened on the main thread",
        ))?;
        let view = create_view(self.unit(), mtm).ok_or(PluginError::Unsupported(
            "this Audio Unit has no usable view",
        ))?;
        // SAFETY: the caller passes a live NSView; we are on the main thread.
        let parent: &NSView = unsafe { parent.cast::<NSView>().as_ref() };
        view.setFrameOrigin(NSPoint::new(0.0, 0.0));
        parent.addSubview(&view);
        let size = Self::view_size(&view);
        self.last_view_size = size;
        self.view = Some(view);
        Ok(size)
    }

    fn close_editor(&mut self) {
        if let Some(view) = self.view.take() {
            view.removeFromSuperview();
        }
    }

    fn is_editor_open(&self) -> bool {
        self.view.is_some()
    }

    fn editor_resizable(&self) -> bool {
        self.view.as_ref().is_some_and(|view| {
            let mask = view.autoresizingMask();
            mask.contains(NSAutoresizingMaskOptions::ViewWidthSizable)
                || mask.contains(NSAutoresizingMaskOptions::ViewHeightSizable)
        })
    }

    fn set_editor_size(&mut self, size: EditorSize) -> Result<EditorSize> {
        let view = self
            .view
            .as_ref()
            .ok_or(PluginError::InvalidState("the editor is not open"))?;
        if self.editor_resizable() {
            view.setFrameSize(NSSize::new(size.width as f64, size.height as f64));
        }
        let accepted = Self::view_size(view);
        self.last_view_size = accepted;
        Ok(accepted)
    }
}

impl Drop for AuInstance {
    fn drop(&mut self) {
        self.close_editor();
        if !self.listener.is_null() {
            // SAFETY: the listener was created by us and is disposed exactly once, before its
            // context is freed.
            unsafe { AUListenerDispose(self.listener) };
        }
        // The unit itself is uninitialised and disposed when the last reference to `shared`
        // goes, which is here unless a processor is still alive.
    }
}
