//! Real-time building blocks for Riddim.
//!
//! Everything in this crate is safe to call from the audio thread unless its documentation says
//! otherwise. The crate also provides the tools we use to *prove* that: [`guard`] marks real-time
//! sections and counts heap traffic inside them when [`guard::RtCheckAllocator`] is installed as
//! the global allocator.

pub mod atomic;
pub mod denormal;
pub mod garbage;
pub mod guard;
pub mod smooth;

/// Wait-free single-producer single-consumer ring buffer (re-export of `rtrb`).
pub mod spsc {
    pub use rtrb::{Consumer, PopError, Producer, PushError, RingBuffer};

    /// Creates a ring with room for `capacity` items.
    pub fn channel<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
        RingBuffer::new(capacity)
    }
}
