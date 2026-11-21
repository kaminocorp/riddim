//! A single-component effect: one object is both the processor and the edit controller.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

use vst3::Steinberg::Vst::{
    BusDirection, BusInfo, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler,
    IComponentTrait, IEditController, IEditControllerTrait, IParamValueQueueTrait,
    IParameterChangesTrait, IoMode, MediaType, ParameterInfo, ProcessData, ProcessSetup,
    RoutingInfo, SpeakerArrangement, String128, TChar,
};
use vst3::Steinberg::{
    int32, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue, tresult,
    FUnknown, IBStream, IBStreamTrait, IPlugView, IPluginBaseTrait, TBool, TUID,
};
use vst3::{uid, Class, ComRef};

use crate::framework::{copy_wstring, DIR_INPUT, DIR_OUTPUT, MEDIA_AUDIO, PARAM_CAN_AUTOMATE};

const STEREO: SpeakerArrangement = 3;

pub struct GainSingle {
    gain: AtomicU64,
    /// Take a long time to initialise, like a sampler loading its library.
    slow: bool,
}

impl GainSingle {
    pub const NAME: &'static str = "Riddim Test Gain";
    pub const CID: TUID = uid(0x52444D54, 0x47414E31, 0x53494E47, 0x00000001);
    pub const GAIN: u32 = 7;
    /// The same plugin under a second class id, with a 400 ms `initialize`.
    pub const SLOW_NAME: &'static str = "Riddim Test Slow Loader";
    pub const SLOW_CID: TUID = uid(0x52444D54, 0x534C4F57, 0x53494E47, 0x00000001);
    pub const SLOW_LOAD: std::time::Duration = std::time::Duration::from_millis(400);

    pub fn new() -> Self {
        Self {
            gain: AtomicU64::new(0.5f64.to_bits()),
            slow: false,
        }
    }

    pub fn new_slow() -> Self {
        Self {
            slow: true,
            ..Self::new()
        }
    }

    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(Ordering::Relaxed))
    }
}

impl Class for GainSingle {
    type Interfaces = (IComponent, IAudioProcessor, IEditController);
}

impl IPluginBaseTrait for GainSingle {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        if self.slow {
            std::thread::sleep(Self::SLOW_LOAD);
        }
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for GainSingle {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        kNotImplemented
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media_type: MediaType, _dir: BusDirection) -> int32 {
        if media_type as i32 == MEDIA_AUDIO {
            1
        } else {
            0
        }
    }

    unsafe fn getBusInfo(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: int32,
        bus: *mut BusInfo,
    ) -> tresult {
        if media_type as i32 != MEDIA_AUDIO || index != 0 {
            return kInvalidArgument;
        }
        let bus = unsafe { &mut *bus };
        bus.mediaType = media_type;
        bus.direction = dir;
        bus.channelCount = 2;
        copy_wstring(
            if dir as i32 == DIR_INPUT {
                "Input"
            } else {
                "Output"
            },
            &mut bus.name,
        );
        bus.busType = 0;
        bus.flags = 1;
        kResultOk
    }

    unsafe fn getRoutingInfo(&self, _in: *mut RoutingInfo, _out: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        _media_type: MediaType,
        _dir: BusDirection,
        _index: int32,
        _state: TBool,
    ) -> tresult {
        kResultOk
    }

    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let Some(stream) = (unsafe { ComRef::from_raw(state) }) else {
            return kInvalidArgument;
        };
        let mut bytes = [0u8; 8];
        let mut got = 0;
        let result = unsafe { stream.read(bytes.as_mut_ptr() as *mut c_void, 8, &mut got) };
        if result != kResultOk || got != 8 {
            return kResultFalse;
        }
        self.gain
            .store(f64::from_le_bytes(bytes).to_bits(), Ordering::Relaxed);
        kResultOk
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let Some(stream) = (unsafe { ComRef::from_raw(state) }) else {
            return kInvalidArgument;
        };
        let bytes = self.gain().to_le_bytes();
        let mut put = 0;
        unsafe { stream.write(bytes.as_ptr() as *mut c_void, 8, &mut put) }
    }
}

impl IAudioProcessorTrait for GainSingle {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: int32,
        outputs: *mut SpeakerArrangement,
        num_outs: int32,
    ) -> tresult {
        let ok = num_ins == 1
            && num_outs == 1
            && unsafe { *inputs } == STEREO
            && unsafe { *outputs } == STEREO;
        if ok {
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
        if index != 0 || !(dir as i32 == DIR_INPUT || dir as i32 == DIR_OUTPUT) {
            return kInvalidArgument;
        }
        unsafe { *arr = STEREO };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, symbolic_sample_size: int32) -> tresult {
        if symbolic_sample_size == 0 {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }

    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        // Deliberately "not implemented": some real plugins answer this way and hosts must cope.
        kNotImplemented
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
                let mut offset = 0;
                let mut value = 0.0;
                if unsafe { queue.getParameterId() } == Self::GAIN
                    && points > 0
                    && unsafe { queue.getPoint(points - 1, &mut offset, &mut value) } == kResultOk
                {
                    self.gain.store(value.to_bits(), Ordering::Relaxed);
                }
            }
        }
        if data.numInputs < 1 || data.numOutputs < 1 {
            return kResultOk;
        }
        let frames = data.numSamples.max(0) as usize;
        let gain = (self.gain() * 2.0) as f32;
        unsafe {
            let input = &*data.inputs;
            let output = &*data.outputs;
            for c in 0..input.numChannels.min(output.numChannels).max(0) as usize {
                let src =
                    std::slice::from_raw_parts(*input.__field0.channelBuffers32.add(c), frames);
                let dst = std::slice::from_raw_parts_mut(
                    *output.__field0.channelBuffers32.add(c),
                    frames,
                );
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = *s * gain;
                }
            }
        }
        kResultOk
    }

    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl IEditControllerTrait for GainSingle {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        kNotImplemented
    }

    // The controller half has no state of its own; the component half saves the gain.
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> int32 {
        1
    }

    unsafe fn getParameterInfo(&self, param_index: int32, info: *mut ParameterInfo) -> tresult {
        if param_index != 0 {
            return kInvalidArgument;
        }
        let info = unsafe { &mut *info };
        info.id = Self::GAIN;
        copy_wstring("Gain", &mut info.title);
        copy_wstring("Gain", &mut info.shortTitle);
        copy_wstring("", &mut info.units);
        info.stepCount = 0;
        info.defaultNormalizedValue = 0.5;
        info.unitId = 0;
        info.flags = PARAM_CAN_AUTOMATE;
        kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        id: u32,
        value_normalized: f64,
        string: *mut String128,
    ) -> tresult {
        if id != Self::GAIN {
            return kInvalidArgument;
        }
        copy_wstring(&format!("x{:.2}", value_normalized * 2.0), unsafe {
            &mut *string
        });
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
        value_normalized * 2.0
    }

    unsafe fn plainParamToNormalized(&self, _id: u32, plain_value: f64) -> f64 {
        plain_value / 2.0
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        if id == Self::GAIN {
            self.gain()
        } else {
            0.0
        }
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        if id != Self::GAIN {
            return kInvalidArgument;
        }
        self.gain.store(value.to_bits(), Ordering::Relaxed);
        kResultOk
    }

    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }

    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        std::ptr::null_mut()
    }
}
