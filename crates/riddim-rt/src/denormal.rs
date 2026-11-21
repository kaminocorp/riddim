//! Denormal handling for audio and worker threads.
//!
//! Denormal floats make some CPUs fall off a performance cliff, so real-time threads set
//! flush-to-zero (and denormals-are-zero on x86) for as long as they process audio.

/// Sets FTZ/DAZ on the current thread for its lifetime and restores the previous mode on drop.
#[must_use = "denormals are only flushed while this guard is alive"]
pub struct ScopedNoDenormals {
    saved: u64,
}

impl ScopedNoDenormals {
    #[inline]
    pub fn new() -> Self {
        let saved = read_fp_control();
        write_fp_control(saved | FLUSH_BITS);
        Self { saved }
    }
}

impl Default for ScopedNoDenormals {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScopedNoDenormals {
    #[inline]
    fn drop(&mut self) {
        write_fp_control(self.saved);
    }
}

/// True if the current thread flushes denormal results to zero.
pub fn denormals_flushed() -> bool {
    FLUSH_BITS != 0 && read_fp_control() & FLUSH_BITS == FLUSH_BITS
}

#[cfg(target_arch = "x86_64")]
const FLUSH_BITS: u64 = 0x8000 | 0x0040; // FTZ | DAZ in MXCSR

#[cfg(target_arch = "aarch64")]
const FLUSH_BITS: u64 = 1 << 24; // FZ in FPCR

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const FLUSH_BITS: u64 = 0;

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_fp_control() -> u64 {
    let mut mxcsr: u32 = 0;
    // SAFETY: stmxcsr stores the 32-bit MXCSR register to the given memory location.
    unsafe {
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut mxcsr, options(nostack, preserves_flags));
    }
    mxcsr as u64
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn write_fp_control(value: u64) {
    let mxcsr = value as u32;
    // SAFETY: ldmxcsr loads MXCSR from memory; we only ever add the FTZ/DAZ bits to a value we
    // previously read, so no exception masks are disturbed.
    unsafe {
        std::arch::asm!("ldmxcsr [{}]", in(reg) &mxcsr, options(nostack, readonly));
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn read_fp_control() -> u64 {
    let fpcr: u64;
    // SAFETY: reading FPCR has no side effects.
    unsafe {
        std::arch::asm!("mrs {}, fpcr", out(reg) fpcr, options(nomem, nostack, preserves_flags));
    }
    fpcr
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn write_fp_control(value: u64) {
    // SAFETY: we only toggle the FZ bit on a value previously read from FPCR.
    unsafe {
        std::arch::asm!("msr fpcr, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn read_fp_control() -> u64 {
    0
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn write_fp_control(_value: u64) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn flushes_and_restores() {
        assert!(!denormals_flushed());
        {
            let _guard = ScopedNoDenormals::new();
            assert!(denormals_flushed());
            let tiny = std::hint::black_box(f32::MIN_POSITIVE);
            let halved = std::hint::black_box(tiny * 0.5);
            assert_eq!(halved, 0.0, "a denormal result must flush to zero");
        }
        assert!(!denormals_flushed());
        let tiny = std::hint::black_box(f32::MIN_POSITIVE);
        assert!(std::hint::black_box(tiny * 0.5) > 0.0);
    }
}
