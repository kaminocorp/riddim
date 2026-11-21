//! Deferred drop: the audio thread never frees memory, it hands it back.
//!
//! Anything heap-owned that the audio thread stops using (an old render plan, a replaced clip
//! buffer, a retired plugin processor) goes down a [`GarbageChute`]. A non-real-time thread owns
//! the matching [`GarbageCollector`] and drops the items there.

use std::any::Any;
use std::sync::Arc;

use crate::spsc::{self, Consumer, Producer};

/// One retired heap object.
pub enum Garbage {
    Boxed(Box<dyn Any + Send>),
    Shared(Arc<dyn Any + Send + Sync>),
}

/// Audio-side handle: accepts garbage without freeing it.
pub struct GarbageChute {
    tx: Producer<Garbage>,
    leaked: u64,
}

/// Control-side handle: drops whatever the audio thread has retired.
pub struct GarbageCollector {
    rx: Consumer<Garbage>,
}

/// Creates a chute/collector pair with room for `capacity` pending items.
pub fn garbage_channel(capacity: usize) -> (GarbageChute, GarbageCollector) {
    let (tx, rx) = spsc::channel(capacity);
    (GarbageChute { tx, leaked: 0 }, GarbageCollector { rx })
}

impl GarbageChute {
    /// Retires a boxed value. Never allocates or frees.
    ///
    /// If the chute is full the value is leaked rather than dropped here; [`leaked`](Self::leaked)
    /// counts how often that happened so the control side can report it.
    #[inline]
    pub fn discard<T: Any + Send>(&mut self, value: Box<T>) {
        self.push(Garbage::Boxed(value));
    }

    /// Retires one reference to a shared value.
    #[inline]
    pub fn discard_arc<T: Any + Send + Sync>(&mut self, value: Arc<T>) {
        self.push(Garbage::Shared(value));
    }

    /// Retires an already type-erased box.
    #[inline]
    pub fn discard_any(&mut self, value: Box<dyn Any + Send>) {
        self.push(Garbage::Boxed(value));
    }

    #[inline]
    fn push(&mut self, garbage: Garbage) {
        if let Err(rtrb::PushError::Full(garbage)) = self.tx.push(garbage) {
            self.leaked += 1;
            std::mem::forget(garbage);
        }
    }

    /// Number of items leaked because the chute was full.
    pub fn leaked(&self) -> u64 {
        self.leaked
    }
}

impl GarbageCollector {
    /// Drops everything retired so far and returns how many items that was.
    pub fn collect(&mut self) -> usize {
        let mut n = 0;
        while let Ok(item) = self.rx.pop() {
            drop(item);
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountsDrops(Arc<AtomicUsize>);
    impl Drop for CountsDrops {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn drops_happen_on_collect_not_on_discard() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (mut chute, mut collector) = garbage_channel(4);
        chute.discard(Box::new(CountsDrops(drops.clone())));
        chute.discard_arc(Arc::new(CountsDrops(drops.clone())));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(collector.collect(), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn full_chute_leaks_instead_of_dropping() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (mut chute, mut collector) = garbage_channel(1);
        chute.discard(Box::new(CountsDrops(drops.clone())));
        chute.discard(Box::new(CountsDrops(drops.clone())));
        assert_eq!(chute.leaked(), 1);
        assert_eq!(collector.collect(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
