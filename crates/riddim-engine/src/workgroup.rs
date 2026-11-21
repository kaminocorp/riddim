//! Getting the render threads into the audio device's workgroup.
//!
//! On macOS a device's I/O thread belongs to an *audio workgroup*, and threads that share its
//! deadline are meant to join it, so that the scheduler treats them as one unit of real-time
//! work (and, on Apple Silicon, keeps them on the performance cores together). The render
//! threads are started before the device is opened, so they wait here until the engine knows
//! whether there is a workgroup to join.
//!
//! **Unverified on macOS**: this compiles for Apple Silicon and has never run.

use std::sync::{Arc, OnceLock};

#[cfg(target_os = "macos")]
type Membership = (riddim_io::coreaudio::Workgroup, f64);
#[cfg(not(target_os = "macos"))]
type Membership = std::convert::Infallible;

/// Where the engine leaves the device's workgroup (or the news that there is none) for the
/// render threads to pick up.
#[derive(Clone, Default)]
pub(crate) struct Slot(Arc<OnceLock<Option<Membership>>>);

impl Slot {
    /// The device is open: here is its workgroup (if it has one) and how long a block lasts.
    #[cfg(target_os = "macos")]
    pub fn set(
        &self,
        workgroup: Option<riddim_io::coreaudio::Workgroup>,
        buffer_frames: u32,
        sample_rate: f64,
    ) {
        let period = buffer_frames as f64 / sample_rate.max(1.0);
        let _ = self.0.set(workgroup.map(|workgroup| (workgroup, period)));
    }

    /// There is no workgroup to join (no hardware, or the device could not be opened). Does
    /// nothing if one has already been set.
    pub fn close(&self) {
        let _ = self.0.set(None);
    }

    /// Called on a render thread as it starts: waits for the engine to say whether there is a
    /// workgroup, and joins it if so.
    pub fn join_when_known(&self) {
        let membership = loop {
            if let Some(known) = self.0.get() {
                break known;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        #[cfg(target_os = "macos")]
        if let Some((workgroup, period)) = membership {
            // A thread that cannot join still renders; it is just scheduled like any other.
            let _ = workgroup.join_current_thread(*period);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = membership;
    }
}
