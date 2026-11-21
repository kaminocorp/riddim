//! A tiny plugin framework: a generic processor and controller driven by a [`Kernel`].

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vst3::Steinberg::Vst::{
    BusDirection, BusInfo, CtrlNumber, IAudioProcessor, IAudioProcessorTrait, IComponent,
    IComponentHandler, IComponentHandlerTrait, IComponentTrait, IEditController,
    IEditControllerTrait, IMidiMapping, IMidiMappingTrait, IParamValueQueueTrait,
    IParameterChangesTrait, IoMode, MediaType, ParamID, ParameterInfo, ProcessData, ProcessSetup,
    RoutingInfo, SpeakerArrangement, String128, TChar,
};
use vst3::Steinberg::{
    int16, int32, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue, tresult,
    FUnknown, IBStream, IBStreamTrait, IPlugView, IPluginBaseTrait, TBool, TUID,
};
use vst3::{Class, ComPtr, ComRef};

pub const MEDIA_AUDIO: i32 = 0;
pub const MEDIA_EVENT: i32 = 1;
pub const DIR_INPUT: i32 = 0;
pub const DIR_OUTPUT: i32 = 1;

pub const PARAM_CAN_AUTOMATE: i32 = 1;
pub const PARAM_IS_READ_ONLY: i32 = 1 << 1;

pub const RESTART_LATENCY_CHANGED: i32 = 1 << 3;

pub struct BusSpec {
    pub name: &'static str,
    pub channels: i32,
    pub main: bool,
    pub default_active: bool,
}

pub struct ParamSpec {
    pub id: u32,
    pub title: &'static str,
    pub units: &'static str,
    pub default: f64,
    pub steps: i32,
    pub flags: i32,
    /// Whether the value is part of the saved component state.
    pub stored: bool,
}

/// The plugin-specific part: layout, parameters and DSP.
pub trait Kernel: Send + Sync + 'static {
    const NAME: &'static str;
    const SUB_CATEGORIES: &'static str;
    const CID: TUID;
    const CONTROLLER_CID: TUID;
    const STATE_MAGIC: [u8; 4];
    const AUDIO_INS: &'static [BusSpec];
    const AUDIO_OUTS: &'static [BusSpec];
    const EVENT_INS: i32;
    const EVENT_OUTS: i32;
    const PARAMS: &'static [ParamSpec];
    /// (MIDI controller number, parameter id) pairs exposed through `IMidiMapping`.
    const MIDI_MAP: &'static [(i16, u32)];
    /// A parameter whose change alters the reported latency.
    const LATENCY_PARAM: Option<u32>;

    fn new() -> Self;
    fn setup(&self, sample_rate: f64, max_block: i32);
    fn latency(&self, shared: &Shared) -> u32;
    unsafe fn process(&self, shared: &Shared, data: &ProcessData);
}

/// State shared between a processor's main-thread and audio-thread halves, all atomics.
pub struct Shared {
    values: Vec<AtomicU64>,
    ids: Vec<u32>,
    in_active: [AtomicBool; 8],
    out_active: [AtomicBool; 8],
}

impl Shared {
    fn new(params: &[ParamSpec], ins: &[BusSpec], outs: &[BusSpec]) -> Self {
        let flags = |buses: &[BusSpec]| {
            std::array::from_fn(|i| {
                AtomicBool::new(buses.get(i).is_some_and(|b| b.default_active || i == 0))
            })
        };
        Self {
            values: params
                .iter()
                .map(|p| AtomicU64::new(p.default.to_bits()))
                .collect(),
            ids: params.iter().map(|p| p.id).collect(),
            in_active: flags(ins),
            out_active: flags(outs),
        }
    }

    pub fn value(&self, id: u32) -> f64 {
        match self.ids.iter().position(|&i| i == id) {
            Some(index) => f64::from_bits(self.values[index].load(Ordering::Relaxed)),
            None => 0.0,
        }
    }

    pub fn set_value(&self, id: u32, value: f64) {
        if let Some(index) = self.ids.iter().position(|&i| i == id) {
            self.values[index].store(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn input_active(&self, bus: usize) -> bool {
        self.in_active
            .get(bus)
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }

    pub fn output_active(&self, bus: usize) -> bool {
        self.out_active
            .get(bus)
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }
}

pub fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let n = src.len().min(dst.len().saturating_sub(1));
    for (d, s) in dst.iter_mut().zip(src.bytes().take(n)) {
        *d = s as c_char;
    }
    if let Some(end) = dst.get_mut(n) {
        *end = 0;
    }
}

pub fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for unit in src.encode_utf16() {
        if n + 1 >= dst.len() {
            break;
        }
        dst[n] = unit as TChar;
        n += 1;
    }
    dst[n] = 0;
}

unsafe fn read_exact(stream: *mut IBStream, buf: &mut [u8]) -> bool {
    let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else {
        return false;
    };
    let mut got: int32 = 0;
    let result = unsafe {
        stream.read(
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as int32,
            &mut got,
        )
    };
    result == kResultOk && got as usize == buf.len()
}

unsafe fn write_all(stream: *mut IBStream, buf: &[u8]) -> bool {
    let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else {
        return false;
    };
    let mut put: int32 = 0;
    let result = unsafe { stream.write(buf.as_ptr() as *mut c_void, buf.len() as int32, &mut put) };
    result == kResultOk && put as usize == buf.len()
}

/// Reads `magic` followed by one f64 per stored parameter.
unsafe fn read_component_state<K: Kernel>(stream: *mut IBStream) -> Option<Vec<(u32, f64)>> {
    let mut magic = [0u8; 4];
    if !unsafe { read_exact(stream, &mut magic) } || magic != K::STATE_MAGIC {
        return None;
    }
    let mut out = Vec::new();
    for param in K::PARAMS.iter().filter(|p| p.stored) {
        let mut bytes = [0u8; 8];
        if !unsafe { read_exact(stream, &mut bytes) } {
            return None;
        }
        out.push((param.id, f64::from_le_bytes(bytes)));
    }
    Some(out)
}

fn bus_info(spec: &BusSpec, direction: i32, bus: &mut BusInfo) {
    bus.mediaType = MEDIA_AUDIO as MediaType;
    bus.direction = direction as BusDirection;
    bus.channelCount = spec.channels;
    copy_wstring(spec.name, &mut bus.name);
    bus.busType = if spec.main { 0 } else { 1 };
    bus.flags = if spec.default_active { 1 } else { 0 };
}

fn speaker_arrangement(channels: i32) -> SpeakerArrangement {
    match channels {
        1 => 1, // mono (L)
        2 => 3, // stereo (L | R)
        n => (1u64 << n.clamp(0, 63)) - 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Processor
// ---------------------------------------------------------------------------------------------

pub struct Processor<K: Kernel> {
    kernel: K,
    shared: Shared,
}

impl<K: Kernel> Processor<K> {
    pub fn new() -> Self {
        Self {
            kernel: K::new(),
            shared: Shared::new(K::PARAMS, K::AUDIO_INS, K::AUDIO_OUTS),
        }
    }
}

impl<K: Kernel> Class for Processor<K> {
    type Interfaces = (IComponent, IAudioProcessor);
}

impl<K: Kernel> IPluginBaseTrait for Processor<K> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl<K: Kernel> IComponentTrait for Processor<K> {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        unsafe { *class_id = K::CONTROLLER_CID };
        kResultOk
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media_type: MediaType, dir: BusDirection) -> int32 {
        match (media_type as i32, dir as i32) {
            (MEDIA_AUDIO, DIR_INPUT) => K::AUDIO_INS.len() as int32,
            (MEDIA_AUDIO, DIR_OUTPUT) => K::AUDIO_OUTS.len() as int32,
            (MEDIA_EVENT, DIR_INPUT) => K::EVENT_INS,
            (MEDIA_EVENT, DIR_OUTPUT) => K::EVENT_OUTS,
            _ => 0,
        }
    }

    unsafe fn getBusInfo(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: int32,
        bus: *mut BusInfo,
    ) -> tresult {
        let bus = unsafe { &mut *bus };
        match (media_type as i32, dir as i32) {
            (MEDIA_AUDIO, DIR_INPUT) => match K::AUDIO_INS.get(index as usize) {
                Some(spec) => bus_info(spec, DIR_INPUT, bus),
                None => return kInvalidArgument,
            },
            (MEDIA_AUDIO, DIR_OUTPUT) => match K::AUDIO_OUTS.get(index as usize) {
                Some(spec) => bus_info(spec, DIR_OUTPUT, bus),
                None => return kInvalidArgument,
            },
            (MEDIA_EVENT, d) => {
                let count = if d == DIR_INPUT {
                    K::EVENT_INS
                } else {
                    K::EVENT_OUTS
                };
                if index < 0 || index >= count {
                    return kInvalidArgument;
                }
                bus.mediaType = MEDIA_EVENT as MediaType;
                bus.direction = d as BusDirection;
                bus.channelCount = 16;
                copy_wstring(
                    if d == DIR_INPUT {
                        "MIDI In"
                    } else {
                        "MIDI Out"
                    },
                    &mut bus.name,
                );
                bus.busType = 0;
                bus.flags = 1;
            }
            _ => return kInvalidArgument,
        }
        kResultOk
    }

    unsafe fn getRoutingInfo(&self, _in: *mut RoutingInfo, _out: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: int32,
        state: TBool,
    ) -> tresult {
        if media_type as i32 == MEDIA_AUDIO {
            let flags = if dir as i32 == DIR_INPUT {
                &self.shared.in_active
            } else {
                &self.shared.out_active
            };
            match flags.get(index as usize) {
                Some(flag) => flag.store(state != 0, Ordering::Relaxed),
                None => return kInvalidArgument,
            }
        }
        kResultOk
    }

    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        match unsafe { read_component_state::<K>(state) } {
            Some(values) => {
                for (id, value) in values {
                    self.shared.set_value(id, value);
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        if !unsafe { write_all(state, &K::STATE_MAGIC) } {
            return kResultFalse;
        }
        for param in K::PARAMS.iter().filter(|p| p.stored) {
            let bytes = self.shared.value(param.id).to_le_bytes();
            if !unsafe { write_all(state, &bytes) } {
                return kResultFalse;
            }
        }
        kResultOk
    }
}

impl<K: Kernel> IAudioProcessorTrait for Processor<K> {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: int32,
        outputs: *mut SpeakerArrangement,
        num_outs: int32,
    ) -> tresult {
        if num_ins as usize != K::AUDIO_INS.len() || num_outs as usize != K::AUDIO_OUTS.len() {
            return kResultFalse;
        }
        let matches = |ptr: *mut SpeakerArrangement, specs: &[BusSpec]| {
            specs
                .iter()
                .enumerate()
                .all(|(i, spec)| (unsafe { *ptr.add(i) }) == speaker_arrangement(spec.channels))
        };
        if matches(inputs, K::AUDIO_INS) && matches(outputs, K::AUDIO_OUTS) {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn getBusArrangement(
        &self,
        dir: BusDirection,
        index: int32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        let specs = if dir as i32 == DIR_INPUT {
            K::AUDIO_INS
        } else {
            K::AUDIO_OUTS
        };
        match specs.get(index as usize) {
            Some(spec) => {
                unsafe { *arr = speaker_arrangement(spec.channels) };
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn canProcessSampleSize(&self, symbolic_sample_size: int32) -> tresult {
        if symbolic_sample_size == 0 {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        self.kernel.latency(&self.shared)
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let setup = unsafe { &*setup };
        self.kernel
            .setup(setup.sampleRate, setup.maxSamplesPerBlock);
        kResultOk
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let data = unsafe { &*data };
        if let Some(changes) = unsafe { ComRef::from_raw(data.inputParameterChanges) } {
            for index in 0..unsafe { changes.getParameterCount() } {
                let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(index)) })
                else {
                    continue;
                };
                let points = unsafe { queue.getPointCount() };
                if points <= 0 {
                    continue;
                }
                let mut offset = 0;
                let mut value = 0.0;
                if unsafe { queue.getPoint(points - 1, &mut offset, &mut value) } == kResultOk {
                    self.shared
                        .set_value(unsafe { queue.getParameterId() }, value);
                }
            }
        }
        unsafe { self.kernel.process(&self.shared, data) };
        kResultOk
    }

    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------------------------

pub struct Controller<K: Kernel> {
    values: Vec<Cell<f64>>,
    handler: RefCell<Option<ComPtr<IComponentHandler>>>,
    /// Controller-only state (think "which page of the UI is showing").
    page: Cell<u32>,
    _kernel: PhantomData<K>,
}

impl<K: Kernel> Controller<K> {
    pub fn new() -> Self {
        Self {
            values: K::PARAMS.iter().map(|p| Cell::new(p.default)).collect(),
            handler: RefCell::new(None),
            page: Cell::new(0),
            _kernel: PhantomData,
        }
    }

    fn index(id: u32) -> Option<usize> {
        K::PARAMS.iter().position(|p| p.id == id)
    }
}

impl<K: Kernel> Class for Controller<K> {
    type Interfaces = (IEditController, IMidiMapping);
}

impl<K: Kernel> IPluginBaseTrait for Controller<K> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        self.handler.borrow_mut().take();
        kResultOk
    }
}

impl<K: Kernel> IEditControllerTrait for Controller<K> {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        match unsafe { read_component_state::<K>(state) } {
            Some(values) => {
                for (id, value) in values {
                    if let Some(index) = Self::index(id) {
                        self.values[index].set(value);
                    }
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let mut bytes = [0u8; 4];
        if unsafe { read_exact(state, &mut bytes) } {
            self.page.set(u32::from_le_bytes(bytes));
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        if unsafe { write_all(state, &self.page.get().to_le_bytes()) } {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getParameterCount(&self) -> int32 {
        K::PARAMS.len() as int32
    }

    unsafe fn getParameterInfo(&self, param_index: int32, info: *mut ParameterInfo) -> tresult {
        let Some(spec) = K::PARAMS.get(param_index as usize) else {
            return kInvalidArgument;
        };
        let info = unsafe { &mut *info };
        info.id = spec.id;
        copy_wstring(spec.title, &mut info.title);
        copy_wstring(spec.title, &mut info.shortTitle);
        copy_wstring(spec.units, &mut info.units);
        info.stepCount = spec.steps;
        info.defaultNormalizedValue = spec.default;
        info.unitId = 0;
        info.flags = spec.flags;
        kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        id: u32,
        value_normalized: f64,
        string: *mut String128,
    ) -> tresult {
        if Self::index(id).is_none() {
            return kInvalidArgument;
        }
        copy_wstring(&format!("{value_normalized:.3}"), unsafe { &mut *string });
        kResultOk
    }

    unsafe fn getParamValueByString(
        &self,
        _id: u32,
        _string: *mut TChar,
        _value_normalized: *mut f64,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn normalizedParamToPlain(&self, _id: u32, value_normalized: f64) -> f64 {
        value_normalized
    }

    unsafe fn plainParamToNormalized(&self, _id: u32, plain_value: f64) -> f64 {
        plain_value
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        Self::index(id).map_or(0.0, |i| self.values[i].get())
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        let Some(index) = Self::index(id) else {
            return kInvalidArgument;
        };
        let changed = self.values[index].replace(value) != value;
        if changed && K::LATENCY_PARAM == Some(id) {
            if let Some(handler) = &*self.handler.borrow() {
                unsafe { handler.restartComponent(RESTART_LATENCY_CHANGED) };
            }
        }
        kResultOk
    }

    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        *self.handler.borrow_mut() = unsafe { ComRef::from_raw(handler) }.map(|h| h.to_com_ptr());
        kResultOk
    }

    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        std::ptr::null_mut()
    }
}

impl<K: Kernel> IMidiMappingTrait for Controller<K> {
    unsafe fn getMidiControllerAssignment(
        &self,
        bus_index: int32,
        _channel: int16,
        midi_controller_number: CtrlNumber,
        id: *mut ParamID,
    ) -> tresult {
        if bus_index != 0 {
            return kResultFalse;
        }
        match K::MIDI_MAP
            .iter()
            .find(|(cc, _)| *cc == midi_controller_number)
        {
            Some((_, param)) => {
                unsafe { *id = *param };
                kResultTrue
            }
            None => kResultFalse,
        }
    }
}
