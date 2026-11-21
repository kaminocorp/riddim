//! Audio and MIDI device I/O.
//!
//! The engine sees a device as an [`AudioStream`] that calls an [`AudioCallback`] once per
//! block with planar `f32` buffers. Two implementations exist:
//!
//! * [`null`]: no hardware. Either paced by a timer thread or stepped by hand, optionally with
//!   its output looped back to its input after a fixed delay. This is what tests, CI and
//!   offline tools run on, on every platform.
//! * `coreaudio` (macOS): the CoreAudio HAL, driven directly. **Unverified on macOS** until the
//!   phase notes say otherwise.
//!
//! MIDI input is in [`midi`].

pub mod midi;
pub mod null;

#[cfg(target_os = "macos")]
pub mod coreaudio;

use std::fmt;

/// Identifies a device across runs (the CoreAudio device UID on macOS).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

/// A device that can be opened.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub input_channels: u32,
    pub output_channels: u32,
    pub sample_rate: f64,
    /// Nominal sample rates the device accepts (as min/max ranges).
    pub sample_rates: Vec<(f64, f64)>,
    /// Smallest and largest buffer size in frames.
    pub buffer_range: (u32, u32),
    pub is_default_input: bool,
    pub is_default_output: bool,
}

/// What to open.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamConfig {
    /// `None` opens the system default output device.
    pub device: Option<DeviceId>,
    pub sample_rate: f64,
    pub buffer_frames: u32,
    /// Capture from the same device as well. (Different input and output devices need an
    /// aggregate device, which is not built here yet.)
    pub capture: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            device: None,
            sample_rate: 48_000.0,
            buffer_frames: 128,
            capture: false,
        }
    }
}

/// What was actually opened.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct StreamInfo {
    pub device_name: String,
    pub sample_rate: f64,
    /// The largest block the callback will see.
    pub max_frames: u32,
    pub input_channels: u32,
    pub output_channels: u32,
    /// Frames between a sound arriving at the input and the callback that carries it.
    pub input_latency_frames: u32,
    /// Frames between a callback producing a sample and it leaving the output.
    pub output_latency_frames: u32,
}

/// One block of device I/O. Every channel slice is `frames` long.
pub struct IoBlock<'a> {
    pub frames: usize,
    pub inputs: &'a [&'a [f32]],
    pub outputs: &'a mut [&'a mut [f32]],
    /// Host clock time (nanoseconds, monotonic) at which the first output frame of this block
    /// will play, or 0 if the backend does not know.
    pub output_time_ns: u64,
}

/// The engine's side of a stream. `process` runs on the device's real-time thread: it must not
/// allocate, lock, or block.
pub trait AudioCallback: Send + 'static {
    fn process(&mut self, block: &mut IoBlock<'_>);
}

impl<F: FnMut(&mut IoBlock<'_>) + Send + 'static> AudioCallback for F {
    fn process(&mut self, block: &mut IoBlock<'_>) {
        self(block)
    }
}

/// A running (or startable) device stream.
pub trait AudioStream {
    fn info(&self) -> &StreamInfo;
    fn start(&mut self) -> Result<(), IoError>;
    fn stop(&mut self);
    /// Overloads (missed deadlines) the device has reported since the stream opened.
    fn xruns(&self) -> u64;
    /// The counter behind [`xruns`](Self::xruns), for something that wants to watch it from
    /// another thread.
    fn xrun_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64>;
    /// Changes the device's buffer size. The stream pauses for the change and carries on if
    /// it was running; [`info`](Self::info) then describes what the device settled on, which
    /// may be a clamped value. The callback stays the same object.
    fn set_buffer_frames(&mut self, frames: u32) -> Result<(), IoError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IoError {
    NoDevice(String),
    /// The device refused a setting or an operation; the code is the platform's status.
    Device(String, i32),
    Unsupported(&'static str),
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoError::NoDevice(what) => write!(f, "no audio device: {what}"),
            IoError::Device(what, code) => write!(f, "audio device error: {what} (status {code})"),
            IoError::Unsupported(what) => write!(f, "not supported: {what}"),
        }
    }
}

impl std::error::Error for IoError {}
