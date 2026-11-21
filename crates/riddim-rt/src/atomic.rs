//! Atomic floats, stored as their bit patterns.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// An `f32` that can be shared between the audio thread and everyone else.
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub const fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn load(&self, order: Ordering) -> f32 {
        f32::from_bits(self.0.load(order))
    }
    #[inline]
    pub fn store(&self, v: f32, order: Ordering) {
        self.0.store(v.to_bits(), order)
    }
    #[inline]
    pub fn swap(&self, v: f32, order: Ordering) -> f32 {
        f32::from_bits(self.0.swap(v.to_bits(), order))
    }
}

/// An `f64` that can be shared between the audio thread and everyone else.
#[derive(Debug, Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub const fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    #[inline]
    pub fn load(&self, order: Ordering) -> f64 {
        f64::from_bits(self.0.load(order))
    }
    #[inline]
    pub fn store(&self, v: f64, order: Ordering) {
        self.0.store(v.to_bits(), order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let a = AtomicF32::new(0.25);
        assert_eq!(a.load(Ordering::Relaxed), 0.25);
        assert_eq!(a.swap(-1.5, Ordering::Relaxed), 0.25);
        assert_eq!(a.load(Ordering::Relaxed), -1.5);
        let b = AtomicF64::new(f64::MIN_POSITIVE);
        b.store(1e300, Ordering::Relaxed);
        assert_eq!(b.load(Ordering::Relaxed), 1e300);
    }
}
