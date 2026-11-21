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
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
    AudioObjectSetPropertyData,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
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
const PROP_IO_THREAD_WORKGROUP: u32 = fourcc(b"oswg");

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

// The workgroup and thread-policy calls, from libSystem. Declared here because the bindings
// we use do not cover them.
extern "C" {
    fn os_workgroup_join(workgroup: *mut c_void, token: *mut JoinToken) -> i32;
    fn os_workgroup_leave(workgroup: *mut c_void, token: *mut JoinToken);
    fn os_retain(object: *mut c_void) -> *mut c_void;
    fn os_release(object: *mut c_void);
    fn mach_thread_self() -> u32;
    fn thread_policy_set(thread: u32, flavor: u32, policy: *const u32, count: u32) -> i32;
}

/// `os_workgroup_join_token_s` is 40 bytes (a 32-bit signature and 36 opaque bytes) in the
/// macOS 11 to 15 SDKs. This leaves generous room, in case it ever grows.
#[repr(C, align(16))]
struct JoinToken([u8; 128]);

/// A device's audio workgroup (`os_workgroup_t`), retained.
///
/// **Unverified on macOS.**
pub struct Workgroup(NonNull<c_void>);

// SAFETY: an os_workgroup is a reference-counted, thread-safe OS object.
unsafe impl Send for Workgroup {}
unsafe impl Sync for Workgroup {}

impl Clone for Workgroup {
    fn clone(&self) -> Self {
        // SAFETY: retaining a live OS object.
        unsafe { os_retain(self.0.as_ptr()) };
        Workgroup(self.0)
    }
}

impl Drop for Workgroup {
    fn drop(&mut self) {
        // SAFETY: balances the retain this value stands for.
        unsafe { os_release(self.0.as_ptr()) };
    }
}

struct Joined {
    workgroup: Workgroup,
    token: Box<JoinToken>,
}

impl Drop for Joined {
    fn drop(&mut self) {
        // SAFETY: the token is the one this thread joined with; a thread must leave its
        // workgroups before it exits, which is when this runs.
        unsafe { os_workgroup_leave(self.workgroup.0.as_ptr(), &mut *self.token) };
    }
}

thread_local! {
    static JOINED: std::cell::RefCell<Option<Joined>> = const { std::cell::RefCell::new(None) };
}

impl Workgroup {
    /// Makes the calling thread a real-time thread whose deadline is one device block of
    /// `period_seconds`, and joins it to the workgroup. The thread leaves the workgroup by
    /// itself when it exits. On failure returns the status of the call that failed; the
    /// thread carries on as an ordinary one.
    pub fn join_current_thread(&self, period_seconds: f64) -> Result<(), i32> {
        const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
        let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: valid out-pointer.
        unsafe { mach_timebase_info(&mut timebase) };
        let ticks = |seconds: f64| {
            let nanos = seconds * 1e9;
            (nanos * timebase.denom.max(1) as f64 / timebase.numer.max(1) as f64) as u32
        };
        // period, computation, constraint, preemptible: the shape Apple's own audio threads
        // use, with the block as the deadline and most of it as the budget.
        let period = period_seconds.clamp(0.000_1, 0.1);
        let policy = [ticks(period), ticks(period * 0.75), ticks(period), 1u32];
        // SAFETY: the policy is four 32-bit words, as THREAD_TIME_CONSTRAINT_POLICY expects.
        let status = unsafe {
            thread_policy_set(
                mach_thread_self(),
                THREAD_TIME_CONSTRAINT_POLICY,
                policy.as_ptr(),
                policy.len() as u32,
            )
        };
        if status != 0 {
            return Err(status);
        }
        let mut token = Box::new(JoinToken([0; 128]));
        // SAFETY: a live workgroup and a token buffer at least as large as the system's.
        let status = unsafe { os_workgroup_join(self.0.as_ptr(), &mut *token) };
        if status != 0 {
            return Err(status);
        }
        JOINED.with(|joined| {
            *joined.borrow_mut() = Some(Joined {
                workgroup: self.clone(),
                token,
            });
        });
        Ok(())
    }
}

/// A private aggregate device: an output device and a separate input device presented to us
/// as one, clocked by the output with the input drift-corrected to it. This is what lets the
/// built-in microphone be recorded while playing through the built-in speakers, which are two
/// devices. Destroyed with the stream. **Unverified on macOS.**
struct Aggregate(AudioObjectID);

impl Drop for Aggregate {
    fn drop(&mut self) {
        // SAFETY: an aggregate device this process created.
        unsafe { AudioHardwareDestroyAggregateDevice(self.0) };
    }
}

/// Builds the aggregate of `output` and `input` and waits for the system to bring it up.
fn create_aggregate(output: AudioObjectID, input: AudioObjectID) -> Result<Aggregate, IoError> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let uid_of = |device: AudioObjectID, what: &str| {
        string_property(device, PROP_DEVICE_UID)
            .ok_or_else(|| IoError::NoDevice(format!("the {what} device has no UID")))
    };
    let (output_uid, input_uid) = (uid_of(output, "output")?, uid_of(input, "input")?);
    let key = CFString::from_static_str;
    let number = CFNumber::new_i32;

    // One entry per sub-device; the input follows the output's clock, with drift correction.
    let sub_device = |uid: &str, drift: i32| {
        let uid = CFString::from_str(uid);
        let drift = number(drift);
        let values: [&CFType; 2] = [uid.as_ref(), drift.as_ref()];
        CFDictionary::<CFString, CFType>::from_slices(&[&*key("uid"), &*key("drift")], &values)
    };
    let sub_devices =
        CFArray::from_retained_objects(&[sub_device(&output_uid, 0), sub_device(&input_uid, 1)]);
    let name = CFString::from_static_str("Riddim input and output");
    let uid = CFString::from_str(&format!(
        "com.kaminocorp.riddim.aggregate.{}.{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let main = CFString::from_str(&output_uid);
    let (private, stacked) = (number(1), number(0));
    let values: [&CFType; 6] = [
        name.as_ref(),
        uid.as_ref(),
        private.as_ref(),
        stacked.as_ref(),
        main.as_ref(),
        sub_devices.as_ref(),
    ];
    let description = CFDictionary::<CFString, CFType>::from_slices(
        &[
            &*key("name"),
            &*key("uid"),
            // Private: visible to this process only, and gone when it exits.
            &*key("private"),
            &*key("stacked"),
            &*key("master"),
            &*key("subdevices"),
        ],
        &values,
    );

    let mut device: AudioObjectID = 0;
    // SAFETY: a dictionary of the documented keys, and a valid out-pointer.
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(description.as_opaque(), NonNull::from(&mut device))
    };
    if status != 0 || device == 0 {
        return Err(IoError::Device(
            "could not combine the input and output devices into one".into(),
            status,
        ));
    }
    let aggregate = Aggregate(device);
    // The device appears at once but its streams are published a moment later.
    let deadline = Instant::now() + Duration::from_secs(3);
    while channel_count(device, SCOPE_OUTPUT) == 0 || channel_count(device, SCOPE_INPUT) == 0 {
        if Instant::now() > deadline {
            return Err(IoError::Device(
                "the combined input and output device did not come up".into(),
                0,
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(aggregate)
}

/// The largest buffer size this backend will ask a device for.
const MAX_BUFFER_FRAMES: u32 = 4096;

/// An open CoreAudio device with our IOProc installed.
pub struct CoreAudioStream {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    context: *mut Context,
    xruns: Arc<AtomicU64>,
    info: StreamInfo,
    started: bool,
    /// Held so that the aggregate device, if one was made, outlives the IOProc on it.
    _aggregate: Option<Aggregate>,
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
        let mut name =
            string_property(device, PROP_NAME).unwrap_or_else(|| "Audio device".to_string());

        // Recording needs inputs on the device we run. An audio interface has them; the
        // built-in speakers do not, and the built-in microphone is another device. In that
        // case make one device of the two.
        let mut aggregate = None;
        let mut device = device;
        if config.capture && channel_count(device, SCOPE_INPUT) == 0 {
            if let Some(input) = default_device(PROP_DEFAULT_INPUT).filter(|&input| input != device)
            {
                let input_name =
                    string_property(input, PROP_NAME).unwrap_or_else(|| "input".to_string());
                let combined = create_aggregate(device, input)?;
                device = combined.0;
                name = format!("{name} + {input_name}");
                aggregate = Some(combined);
            }
        }

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
        // headroom rather than drop audio. Sized once for the largest buffer we ever ask for
        // (see `set_buffer_frames`), so that changing the buffer size never has to reallocate
        // what the I/O thread may still be looking at.
        let max_frames = (buffer_frames as usize * 2).max(2 * MAX_BUFFER_FRAMES as usize);
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
            _aggregate: aggregate,
        })
    }
}

impl CoreAudioStream {
    /// The workgroup of the device's I/O thread, for render threads to join.
    pub fn workgroup(&self) -> Option<Workgroup> {
        // SAFETY: the property is one os_workgroup_t, handed over retained.
        let raw =
            unsafe { get::<*mut c_void>(self.device, PROP_IO_THREAD_WORKGROUP, SCOPE_GLOBAL) }?;
        NonNull::new(raw).map(Workgroup)
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

    fn xrun_counter(&self) -> Arc<AtomicU64> {
        self.xruns.clone()
    }

    fn set_buffer_frames(&mut self, frames: u32) -> Result<(), IoError> {
        let was_started = self.started;
        self.stop();
        // SAFETY: the range is an AudioValueRange, the size a UInt32.
        let (status, wanted, now) = unsafe {
            let range = get::<ValueRange>(self.device, PROP_BUFFER_FRAME_SIZE_RANGE, SCOPE_GLOBAL)
                .unwrap_or(ValueRange {
                    minimum: 16.0,
                    maximum: MAX_BUFFER_FRAMES as f64,
                });
            let wanted = frames
                .clamp(range.minimum as u32, range.maximum as u32)
                .min(MAX_BUFFER_FRAMES);
            let status = set(self.device, PROP_BUFFER_FRAME_SIZE, SCOPE_GLOBAL, &wanted);
            let now = get::<u32>(self.device, PROP_BUFFER_FRAME_SIZE, SCOPE_GLOBAL)
                .unwrap_or(self.info.max_frames);
            (status, wanted, now)
        };
        // Describe what the device is doing now, whether or not it took the new size.
        let old = self.info.max_frames;
        self.info.max_frames = now;
        self.info.output_latency_frames = self.info.output_latency_frames - old + now;
        if self.info.input_channels > 0 {
            self.info.input_latency_frames = self.info.input_latency_frames - old + now;
        }
        if was_started {
            self.start()?;
        }
        if status != 0 {
            return Err(IoError::Device(
                format!("{} refused a {wanted}-frame buffer", self.info.device_name),
                status,
            ));
        }
        Ok(())
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
