//! Constants, a few declarations the bindings lack, and small helpers for the AudioUnit C API.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_audio_toolbox::{AudioUnit, AudioUnitParameter, AudioUnitProperty};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioStreamBasicDescription};

pub type OSStatus = i32;
pub const NO_ERR: OSStatus = 0;

// AudioUnitScope
pub const SCOPE_GLOBAL: u32 = 0;
pub const SCOPE_INPUT: u32 = 1;
pub const SCOPE_OUTPUT: u32 = 2;

// AudioUnitPropertyID (AudioUnitProperties.h)
pub const PROP_CLASS_INFO: u32 = 0;
pub const PROP_PARAMETER_LIST: u32 = 3;
pub const PROP_PARAMETER_INFO: u32 = 4;
pub const PROP_STREAM_FORMAT: u32 = 8;
pub const PROP_ELEMENT_COUNT: u32 = 11;
pub const PROP_LATENCY: u32 = 12;
pub const PROP_MAXIMUM_FRAMES_PER_SLICE: u32 = 14;
pub const PROP_TAIL_TIME: u32 = 20;
pub const PROP_SET_RENDER_CALLBACK: u32 = 23;
pub const PROP_HOST_CALLBACKS: u32 = 27;
pub const PROP_ELEMENT_NAME: u32 = 30;
pub const PROP_COCOA_UI: u32 = 31;
pub const PROP_PARAMETER_STRING_FROM_VALUE: u32 = 33;
pub const PROP_PRESENT_PRESET: u32 = 36;
pub const PROP_OFFLINE_RENDER: u32 = 37;

// AudioComponentDescription.componentType
pub const TYPE_MUSIC_DEVICE: u32 = fourcc(b"aumu");
pub const TYPE_MUSIC_EFFECT: u32 = fourcc(b"aumf");
pub const TYPE_EFFECT: u32 = fourcc(b"aufx");
pub const TYPE_GENERATOR: u32 = fourcc(b"augn");

// AudioComponentFlags
pub const FLAG_REQUIRES_ASYNC_INSTANTIATION: u32 = 2;

// AudioUnitParameterOptions
pub const PARAM_FLAG_CF_NAME_RELEASE: u32 = 1 << 4;
pub const PARAM_FLAG_HAS_CF_NAME_STRING: u32 = 1 << 27;
pub const PARAM_FLAG_IS_READABLE: u32 = 1 << 30;
pub const PARAM_FLAG_IS_WRITABLE: u32 = 1 << 31;

// AudioUnitParameterUnit
pub const UNIT_INDEXED: u32 = 1;
pub const UNIT_BOOLEAN: u32 = 2;
pub const UNIT_CUSTOM: u32 = 26;

// AudioUnitEventType
pub const EVENT_PARAMETER_VALUE_CHANGE: u32 = 0;
pub const EVENT_BEGIN_PARAMETER_CHANGE_GESTURE: u32 = 1;
pub const EVENT_END_PARAMETER_CHANGE_GESTURE: u32 = 2;
pub const EVENT_PROPERTY_CHANGE: u32 = 3;

/// `kAUParameterListener_AnyParameter`
pub const ANY_PARAMETER: u32 = 0xFFFF_FFFF;

pub const fn fourcc(code: &[u8; 4]) -> u32 {
    ((code[0] as u32) << 24) | ((code[1] as u32) << 16) | ((code[2] as u32) << 8) | code[3] as u32
}

/// Four-character code as text. Non-printable bytes become `?` so ids stay plain ASCII.
pub fn fourcc_to_string(code: u32) -> String {
    code.to_be_bytes()
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) && b != b':' {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

pub fn fourcc_from_str(text: &str) -> Option<u32> {
    let bytes: [u8; 4] = text.as_bytes().try_into().ok()?;
    Some(fourcc(&bytes))
}

/// `AudioUnitEvent` from AudioUnitUtilities.h (the bindings omit it because of its union).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitEvent {
    pub event_type: u32,
    pub argument: AudioUnitEventArgument,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union AudioUnitEventArgument {
    pub parameter: AudioUnitParameter,
    pub property: AudioUnitProperty,
}

#[repr(C)]
pub struct OpaqueListener {
    _private: [u8; 0],
}
pub type AUEventListenerRef = *mut OpaqueListener;

pub type AUEventListenerProc = unsafe extern "C-unwind" fn(
    user_data: *mut c_void,
    object: *mut c_void,
    event: *const AudioUnitEvent,
    host_time: u64,
    value: f32,
);

#[link(name = "AudioToolbox", kind = "framework")]
extern "C-unwind" {
    pub fn AUEventListenerCreate(
        proc_: AUEventListenerProc,
        user_data: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        notification_interval: f32,
        value_change_granularity: f32,
        out_listener: *mut AUEventListenerRef,
    ) -> OSStatus;

    pub fn AUEventListenerAddEventType(
        listener: AUEventListenerRef,
        object: *mut c_void,
        event: *const AudioUnitEvent,
    ) -> OSStatus;

    pub fn AUListenerDispose(listener: AUEventListenerRef) -> OSStatus;

    pub fn AUParameterSet(
        listener: AUEventListenerRef,
        object: *mut c_void,
        parameter: *const AudioUnitParameter,
        value: f32,
        buffer_offset: u32,
    ) -> OSStatus;

    pub fn AUParameterListenerNotify(
        listener: AUEventListenerRef,
        object: *mut c_void,
        parameter: *const AudioUnitParameter,
    ) -> OSStatus;
}

// CoreFoundation, declared with untyped references: these few C entry points are stable and
// this keeps the property-list and run-loop plumbing independent of binding details.
pub type CFTypeRef = *const c_void;

pub const PLIST_FORMAT_XML: isize = 100;
pub const PLIST_IMMUTABLE: usize = 0;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C-unwind" {
    pub static kCFRunLoopDefaultMode: CFTypeRef;

    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRunLoopGetCurrent() -> CFTypeRef;
    pub fn CFRunLoopRunInMode(
        mode: CFTypeRef,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
    pub fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> CFTypeRef;
    pub fn CFDataGetLength(data: CFTypeRef) -> isize;
    pub fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    pub fn CFPropertyListCreateData(
        allocator: CFTypeRef,
        property_list: CFTypeRef,
        format: isize,
        options: usize,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
    pub fn CFPropertyListCreateWithData(
        allocator: CFTypeRef,
        data: CFTypeRef,
        options: usize,
        format: *mut isize,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
}

/// Lets the current thread's run loop deliver whatever is pending, without blocking.
pub fn pump_run_loop() {
    // SAFETY: plain CoreFoundation calls with a valid mode constant.
    unsafe {
        while CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1) == 4 {
            // 4 is kCFRunLoopRunHandledSource: keep going until nothing is left.
        }
    }
}

/// The stream format Riddim uses everywhere: 32-bit float, native endian, one buffer per
/// channel.
pub fn float_format(sample_rate: f64, channels: u32) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: fourcc(b"lpcm"),
        // kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved
        mFormatFlags: 1 | (1 << 3) | (1 << 5),
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Reads a fixed-size property value.
///
/// # Safety
/// `T` must be the type the AudioUnit API documents for `id`.
pub unsafe fn get_property<T: Copy>(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: the out-pointer has room for `size` bytes, as the API requires.
    let status = unsafe {
        objc2_audio_toolbox::AudioUnitGetProperty(
            unit,
            id,
            scope,
            element,
            NonNull::new_unchecked(value.as_mut_ptr() as *mut c_void),
            NonNull::from(&mut size),
        )
    };
    // SAFETY: on success the AU wrote a `T`; the buffer was zeroed, so a short write is still
    // initialised memory.
    (status == NO_ERR).then(|| unsafe { value.assume_init() })
}

/// Writes a fixed-size property value.
///
/// # Safety
/// `T` must be the type the AudioUnit API documents for `id`.
pub unsafe fn set_property<T>(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
    value: &T,
) -> OSStatus {
    // SAFETY: the pointer and size describe `value` exactly.
    unsafe {
        objc2_audio_toolbox::AudioUnitSetProperty(
            unit,
            id,
            scope,
            element,
            value as *const T as *const c_void,
            std::mem::size_of::<T>() as u32,
        )
    }
}

/// An `AudioBufferList` with room for any number of single-channel buffers, allocated once.
pub struct BufferList {
    // u64 storage keeps the pointer fields aligned.
    storage: Vec<u64>,
    capacity: usize,
}

impl BufferList {
    pub fn with_capacity(channels: usize) -> Self {
        let channels = channels.max(1);
        let bytes = std::mem::size_of::<AudioBufferList>()
            + (channels - 1) * std::mem::size_of::<AudioBuffer>();
        Self {
            storage: vec![0u64; bytes.div_ceil(8)],
            capacity: channels,
        }
    }

    pub fn as_ptr(&mut self) -> NonNull<AudioBufferList> {
        // SAFETY: a Vec's data pointer is never null.
        unsafe { NonNull::new_unchecked(self.storage.as_mut_ptr() as *mut AudioBufferList) }
    }

    /// Sets up `channels` mono buffers of `frames` samples, with pointers from `pointer(i)`.
    pub fn prepare(
        &mut self,
        channels: usize,
        frames: usize,
        mut pointer: impl FnMut(usize) -> *mut f32,
    ) {
        let channels = channels.min(self.capacity);
        let list = self.as_ptr().as_ptr();
        // SAFETY: the storage holds `capacity` buffers, and `channels <= capacity`.
        unsafe {
            (*list).mNumberBuffers = channels as u32;
            let buffers = std::ptr::addr_of_mut!((*list).mBuffers) as *mut AudioBuffer;
            for i in 0..channels {
                *buffers.add(i) = AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: (frames * std::mem::size_of::<f32>()) as u32,
                    mData: pointer(i) as *mut c_void,
                };
            }
        }
    }

    /// The data pointer the AU left in buffer `index` (it may have swapped in its own).
    pub fn data(&mut self, index: usize) -> *mut f32 {
        debug_assert!(index < self.capacity);
        let list = self.as_ptr().as_ptr();
        // SAFETY: `index` is within the allocated buffers.
        unsafe {
            let buffers = std::ptr::addr_of!((*list).mBuffers) as *const AudioBuffer;
            (*buffers.add(index)).mData as *mut f32
        }
    }
}

/// Iterates the buffers of a list handed to us by an AU.
///
/// # Safety
/// `list` must point to a valid `AudioBufferList`.
pub unsafe fn buffers_mut<'a>(list: *mut AudioBufferList) -> &'a mut [AudioBuffer] {
    // SAFETY: per the caller's contract the list holds `mNumberBuffers` buffers.
    unsafe {
        let count = (*list).mNumberBuffers as usize;
        let first = std::ptr::addr_of_mut!((*list).mBuffers) as *mut AudioBuffer;
        std::slice::from_raw_parts_mut(first, count)
    }
}
