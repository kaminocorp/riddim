//! CoreAudio HAL device I/O.
//!
//! We talk to the HAL directly (an IOProc on the device) rather than through an output Audio
//! Unit or a portability layer: a DAW needs the device's real buffer size, sample rate,
//! latency figures and overload notifications, and later its audio workgroup.
//!
//! **UNVERIFIED ON MACOS.** Type-checked from Linux; not yet run against real hardware.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener, AudioObjectSetPropertyData,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::{CFRetained, CFString};
use riddim_rt::planar::PlanarScratch;

use crate::{
    AudioCallback, AudioStream, DeviceId, DeviceInfo, IoBlock, IoError, StreamConfig, StreamInfo,
};

const fn fourcc(code: &[u8; 4]) -> u32 {
    ((code[0] as u32) << 24) | ((code[1] as u32) << 16) | ((code[2] as u32) << 8) | code[3] as u32
}

const SYSTEM_OBJECT: AudioObjectID = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_INPUT: u32 = fourcc(b"inpt");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const ELEMENT_MAIN: u32 = 0;

const PROP_DEVICES: u32 = fourcc(b"dev#");
const PROP_DEFAULT_INPUT: u32 = fourcc(b"dIn ");
const PROP_DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const PROP_NAME: u32 = fourcc(b"lnam");
const PROP_DEVICE_UID: u32 = fourcc(b"uid ");
const PROP_NOMINAL_SAMPLE_RATE: u32 = fourcc(b"nsrt");
const PROP_AVAILABLE_SAMPLE_RATES: u32 = fourcc(b"nsr#");
const PROP_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const PROP_BUFFER_FRAME_SIZE_RANGE: u32 = fourcc(b"fsz#");
const PROP_STREAM_CONFIGURATION: u32 = fourcc(b"slay");
const PROP_LATENCY: u32 = fourcc(b"ltnc");
const PROP_SAFETY_OFFSET: u32 = fourcc(b"saft");
const PROP_STREAMS: u32 = fourcc(b"stm#");
const PROP_PROCESSOR_OVERLOAD: u32 = fourcc(b"over");

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ValueRange {
    minimum: f64,
    maximum: f64,
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: ELEMENT_MAIN,
    }
}

/// Reads a fixed-size property.
///
/// # Safety
/// `T` must be the type CoreAudio documents for the property.
unsafe fn get<T: Copy>(object: AudioObjectID, selector: u32, scope: u32) -> Option<T> {
    let mut addr = address(selector, scope);
    let mut size = std::mem::size_of::<T>() as u32;
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    // SAFETY: the out-pointer has room for `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(value.as_mut_ptr() as *mut c_void),
        )
    };
    // SAFETY: zero-initialised, then written by CoreAudio on success.
    (status == 0).then(|| unsafe { value.assume_init() })
}

/// Reads a variable-length property as an array of `T`.
///
/// # Safety
/// `T` must be the element type CoreAudio documents for the property.
unsafe fn get_array<T: Copy + Default>(object: AudioObjectID, selector: u32, scope: u32) -> Vec<T> {
    let mut addr = address(selector, scope);
    let mut size = 0u32;
    // SAFETY: valid address and out-pointer.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = size as usize / std::mem::size_of::<T>();
    let mut values = vec![T::default(); count];
    // SAFETY: the buffer holds `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(values.as_mut_ptr() as *mut c_void),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    values.truncate(size as usize / std::mem::size_of::<T>());
    values
}

/// Writes a fixed-size property.
///
/// # Safety
/// `T` must be the type CoreAudio documents for the property.
unsafe fn set<T>(object: AudioObjectID, selector: u32, scope: u32, value: &T) -> i32 {
    let mut addr = address(selector, scope);
    // SAFETY: the pointer and size describe `value`.
    unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            std::mem::size_of::<T>() as u32,
            NonNull::from(value).cast(),
        )
    }
}

fn string_property(object: AudioObjectID, selector: u32) -> Option<String> {
    // SAFETY: both properties yield a CFString reference that the caller owns.
    unsafe {
        let string: *const CFString = get(object, selector, SCOPE_GLOBAL)?;
        let string = NonNull::new(string as *mut CFString)?;
        Some(CFRetained::from_raw(string).to_string())
    }
}

/// Total channels on one side of a device.
fn channel_count(device: AudioObjectID, scope: u32) -> u32 {
    let mut addr = address(PROP_STREAM_CONFIGURATION, scope);
    let mut size = 0u32;
    // SAFETY: the configuration is an AudioBufferList of the reported size; it is read into
    // 8-byte-aligned storage of at least that size.
    unsafe {
        if AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        ) != 0
            || (size as usize) < std::mem::size_of::<u32>()
        {
            return 0;
        }
        let mut storage = vec![0u64; (size as usize).div_ceil(8)];
        if AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(storage.as_mut_ptr() as *mut c_void),
        ) != 0
        {
            return 0;
        }
        let list = storage.as_ptr() as *const AudioBufferList;
        buffers(list).iter().map(|b| b.mNumberChannels).sum()
    }
}

/// The buffers of a list.
///
/// # Safety
/// `list` must point to a valid `AudioBufferList`.
unsafe fn buffers<'a>(list: *const AudioBufferList) -> &'a [AudioBuffer] {
    // SAFETY: per the caller's contract.
    unsafe {
        let count = (*list).mNumberBuffers as usize;
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*list).mBuffers) as *const AudioBuffer,
            count,
        )
    }
}

fn default_device(selector: u32) -> Option<AudioObjectID> {
    // SAFETY: the default-device properties are AudioObjectIDs.
    unsafe { get::<AudioObjectID>(SYSTEM_OBJECT, selector, SCOPE_GLOBAL) }.filter(|&id| id != 0)
}

fn describe(device: AudioObjectID) -> Option<DeviceInfo> {
    let id = DeviceId(string_property(device, PROP_DEVICE_UID)?);
    // SAFETY: each property is read as its documented type.
    let (sample_rate, rates, range) = unsafe {
        (
            get::<f64>(device, PROP_NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL).unwrap_or(0.0),
            get_array::<ValueRange>(device, PROP_AVAILABLE_SAMPLE_RATES, SCOPE_GLOBAL),
            get::<ValueRange>(device, PROP_BUFFER_FRAME_SIZE_RANGE, SCOPE_GLOBAL).unwrap_or(
                ValueRange {
                    minimum: 16.0,
                    maximum: 4096.0,
                },
            ),
        )
    };
    Some(DeviceInfo {
        id,
        name: string_property(device, PROP_NAME).unwrap_or_else(|| "Audio device".to_string()),
        input_channels: channel_count(device, SCOPE_INPUT),
        output_channels: channel_count(device, SCOPE_OUTPUT),
        sample_rate,
        sample_rates: rates.iter().map(|r| (r.minimum, r.maximum)).collect(),
        buffer_range: (range.minimum as u32, range.maximum as u32),
        is_default_input: default_device(PROP_DEFAULT_INPUT) == Some(device),
        is_default_output: default_device(PROP_DEFAULT_OUTPUT) == Some(device),
    })
}

fn device_ids() -> Vec<AudioObjectID> {
    // SAFETY: the device list is an array of AudioObjectIDs.
    unsafe { get_array::<AudioObjectID>(SYSTEM_OBJECT, PROP_DEVICES, SCOPE_GLOBAL) }
}

/// Lists the audio devices on this Mac.
pub fn devices() -> Vec<DeviceInfo> {
    device_ids().into_iter().filter_map(describe).collect()
}

/// Device latency + safety offset + the first stream's latency, for one direction.
fn fixed_latency(device: AudioObjectID, scope: u32) -> u32 {
    // SAFETY: all three are UInt32 frame counts; the stream list is an array of object ids.
    unsafe {
        let device_latency = get::<u32>(device, PROP_LATENCY, scope).unwrap_or(0);
        let safety = get::<u32>(device, PROP_SAFETY_OFFSET, scope).unwrap_or(0);
        let stream_latency = get_array::<AudioObjectID>(device, PROP_STREAMS, scope)
            .first()
            .and_then(|&stream| get::<u32>(stream, PROP_LATENCY, SCOPE_GLOBAL))
            .unwrap_or(0);
        device_latency + safety + stream_latency
    }
}

/// Everything the IOProc touches, allocated before the stream starts.
struct Context {
    callback: Box<dyn AudioCallback>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    scratch: PlanarScratch,
    max_frames: usize,
    capture: bool,
    ticks_to_ns: (u32, u32),
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// The HAL's I/O callback: de-interleave, run the engine, interleave.
unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input_data: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output_data: NonNull<AudioBufferList>,
    output_time: NonNull<AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    // SAFETY: `client_data` is the Context owned by the stream, alive until the IOProc has been
    // destroyed; the HAL calls this from one thread at a time. Buffer lists are valid for the
    // duration of the call and hold interleaved 32-bit floats (the HAL's canonical format).
    unsafe {
        let context = &mut *(client_data as *mut Context);
        let _denormals = riddim_rt::denormal::ScopedNoDenormals::new();
        let _section = riddim_rt::guard::enter();

        let out_buffers = buffers(output_data.as_ptr());
        let frames = out_buffers
            .first()
            .filter(|b| b.mNumberChannels > 0)
            .map_or(0, |b| {
                b.mDataByteSize as usize / (4 * b.mNumberChannels as usize)
            })
            .min(context.max_frames);

        // De-interleave the inputs.
        let mut channel = 0;
        if context.capture {
            for buffer in buffers(input_data.as_ptr()) {
                let stride = buffer.mNumberChannels as usize;
                let available = if stride == 0 {
                    0
                } else {
                    buffer.mDataByteSize as usize / (4 * stride)
                };
                let n = frames.min(available);
                let data = buffer.mData as *const f32;
                for c in 0..stride {
                    let Some(dst) = context.inputs.get_mut(channel) else {
                        break;
                    };
                    if !data.is_null() {
                        for (frame, sample) in dst[..n].iter_mut().enumerate() {
                            *sample = *data.add(frame * stride + c);
                        }
                    }
                    dst[n..frames].fill(0.0);
                    channel += 1;
                }
            }
        }
        for dst in context.inputs.iter_mut().skip(channel) {
            dst[..frames].fill(0.0);
        }
        for output in context.outputs.iter_mut() {
            output[..frames].fill(0.0);
        }

        let host_ticks = output_time.as_ref().mHostTime;
        let (numer, denom) = context.ticks_to_ns;
        let output_time_ns = if denom == 0 {
            0
        } else {
            (host_ticks as u128 * numer as u128 / denom as u128) as u64
        };

        {
            let (inputs, outputs) = context.scratch.fill(
                context.inputs.iter().map(|c| c.as_slice()),
                context.outputs.iter_mut().map(|c| c.as_mut_slice()),
                frames,
            );
            let mut block = IoBlock {
                frames,
                inputs,
                outputs,
                output_time_ns,
            };
            context.callback.process(&mut block);
        }

        // Interleave the outputs.
        let mut channel = 0;
        for buffer in out_buffers {
            let stride = buffer.mNumberChannels as usize;
            let data = buffer.mData as *mut f32;
            if data.is_null() || stride == 0 {
                channel += stride;
                continue;
            }
            let n = frames.min(buffer.mDataByteSize as usize / (4 * stride));
            for c in 0..stride {
                match context.outputs.get(channel) {
                    Some(src) => {
                        for (frame, sample) in src[..n].iter().enumerate() {
                            *data.add(frame * stride + c) = *sample;
                        }
                    }
                    None => {
                        for frame in 0..n {
                            *data.add(frame * stride + c) = 0.0;
                        }
                    }
                }
                channel += 1;
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn overload_listener(
    _object: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    // SAFETY: `client_data` is the counter the stream keeps alive until the listener is removed.
    unsafe { (*(client_data as *const AtomicU64)).fetch_add(1, Ordering::Relaxed) };
    0
}

/// An open CoreAudio device with our IOProc installed.
pub struct CoreAudioStream {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    context: *mut Context,
    xruns: Arc<AtomicU64>,
    info: StreamInfo,
    started: bool,
}

// SAFETY: the raw context pointer is owned by the stream and only dereferenced by the HAL's
// I/O thread while the IOProc exists.
unsafe impl Send for CoreAudioStream {}

impl CoreAudioStream {
    /// Opens the configured device (or the default output device) and prepares it to run
    /// `callback`. The stream does not start until [`AudioStream::start`].
    pub fn open(config: &StreamConfig, callback: impl AudioCallback) -> Result<Self, IoError> {
        let device = match &config.device {
            Some(wanted) => device_ids()
                .into_iter()
                .find(|&id| string_property(id, PROP_DEVICE_UID).as_deref() == Some(&wanted.0))
                .ok_or_else(|| IoError::NoDevice(format!("no device with UID {}", wanted.0)))?,
            None => default_device(PROP_DEFAULT_OUTPUT)
                .ok_or_else(|| IoError::NoDevice("there is no default output device".into()))?,
        };
        let name = string_property(device, PROP_NAME).unwrap_or_else(|| "Audio device".to_string());

        // Sample rate changes are asynchronous: request, then wait for the device to report it.
        // SAFETY: the property is a Float64.
        unsafe {
            let current = get::<f64>(device, PROP_NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL).unwrap_or(0.0);
            if (current - config.sample_rate).abs() > 0.5 {
                let status = set(
                    device,
                    PROP_NOMINAL_SAMPLE_RATE,
                    SCOPE_GLOBAL,
                    &config.sample_rate,
                );
                if status != 0 {
                    return Err(IoError::Device(
                        format!("{name} would not switch to {} Hz", config.sample_rate),
                        status,
                    ));
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let now =
                        get::<f64>(device, PROP_NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL).unwrap_or(0.0);
                    if (now - config.sample_rate).abs() <= 0.5 {
                        break;
                    }
                    if Instant::now() > deadline {
                        return Err(IoError::Device(
                            format!(
                                "{name} did not reach {} Hz (still {now})",
                                config.sample_rate
                            ),
                            0,
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        // Buffer size, clamped to what the device allows.
        // SAFETY: the range is an AudioValueRange, the size a UInt32.
        let buffer_frames = unsafe {
            let range = get::<ValueRange>(device, PROP_BUFFER_FRAME_SIZE_RANGE, SCOPE_GLOBAL)
                .unwrap_or(ValueRange {
                    minimum: 16.0,
                    maximum: 4096.0,
                });
            let wanted = config
                .buffer_frames
                .clamp(range.minimum as u32, range.maximum as u32);
            let status = set(device, PROP_BUFFER_FRAME_SIZE, SCOPE_GLOBAL, &wanted);
            if status != 0 {
                return Err(IoError::Device(
                    format!("{name} refused a {wanted}-frame buffer"),
                    status,
                ));
            }
            get::<u32>(device, PROP_BUFFER_FRAME_SIZE, SCOPE_GLOBAL).unwrap_or(wanted)
        };

        let output_channels = channel_count(device, SCOPE_OUTPUT);
        let input_channels = if config.capture {
            channel_count(device, SCOPE_INPUT)
        } else {
            0
        };
        if output_channels == 0 {
            return Err(IoError::NoDevice(format!("{name} has no outputs")));
        }

        // The HAL may deliver more frames than the nominal buffer size in rare cases; leave
        // headroom rather than drop audio.
        let max_frames = (buffer_frames as usize * 2).max(4096);
        let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: valid out-pointer.
        unsafe { mach_timebase_info(&mut timebase) };
        let xruns = Arc::new(AtomicU64::new(0));
        let context = Box::into_raw(Box::new(Context {
            callback: Box::new(callback),
            inputs: vec![vec![0.0; max_frames]; input_channels as usize],
            outputs: vec![vec![0.0; max_frames]; output_channels as usize],
            scratch: PlanarScratch::new(input_channels as usize, output_channels as usize),
            max_frames,
            capture: config.capture,
            ticks_to_ns: (timebase.numer, timebase.denom),
        }));

        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: the context outlives the IOProc (destroyed in Drop before the context is
        // freed).
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(io_proc),
                context as *mut c_void,
                NonNull::from(&mut proc_id),
            )
        };
        if status != 0 {
            // SAFETY: the HAL did not take the context.
            drop(unsafe { Box::from_raw(context) });
            return Err(IoError::Device(
                format!("could not install an IOProc on {name}"),
                status,
            ));
        }

        // SAFETY: the counter lives in an Arc held by the stream until the listener is removed.
        unsafe {
            let mut addr = address(PROP_PROCESSOR_OVERLOAD, SCOPE_GLOBAL);
            AudioObjectAddPropertyListener(
                device,
                NonNull::from(&mut addr),
                Some(overload_listener),
                Arc::as_ptr(&xruns) as *mut c_void,
            );
        }

        let info = StreamInfo {
            device_name: name,
            sample_rate: config.sample_rate,
            max_frames: buffer_frames,
            input_channels,
            output_channels,
            input_latency_frames: if config.capture {
                fixed_latency(device, SCOPE_INPUT) + buffer_frames
            } else {
                0
            },
            output_latency_frames: fixed_latency(device, SCOPE_OUTPUT) + buffer_frames,
        };
        Ok(CoreAudioStream {
            device,
            proc_id,
            context,
            xruns,
            info,
            started: false,
        })
    }
}

impl AudioStream for CoreAudioStream {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn start(&mut self) -> Result<(), IoError> {
        if self.started {
            return Ok(());
        }
        // SAFETY: the IOProc was created on this device.
        let status = unsafe { AudioDeviceStart(self.device, self.proc_id) };
        if status != 0 {
            return Err(IoError::Device(
                format!("could not start {}", self.info.device_name),
                status,
            ));
        }
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) {
        if self.started {
            // SAFETY: as for `start`.
            unsafe { AudioDeviceStop(self.device, self.proc_id) };
            self.started = false;
        }
    }

    fn xruns(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }
}

impl Drop for CoreAudioStream {
    fn drop(&mut self) {
        self.stop();
        // SAFETY: stop, remove the listener and destroy the IOProc before freeing what they
        // point at. AudioDeviceDestroyIOProcID returns only once the IOProc is no longer
        // running.
        unsafe {
            let mut addr = address(PROP_PROCESSOR_OVERLOAD, SCOPE_GLOBAL);
            AudioObjectRemovePropertyListener(
                self.device,
                NonNull::from(&mut addr),
                Some(overload_listener),
                Arc::as_ptr(&self.xruns) as *mut c_void,
            );
            AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            drop(Box::from_raw(self.context));
        }
    }
}
