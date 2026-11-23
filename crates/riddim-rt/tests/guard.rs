//! The allocation guard, exercised with the checking allocator installed for real.

use riddim_rt::garbage::garbage_channel;
use riddim_rt::guard::{self, RtCheckAllocator};
use riddim_rt::spsc;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

#[test]
fn allocator_is_installed() {
    assert!(guard::allocator_installed());
}

#[test]
fn allocation_outside_rt_is_fine() {
    let before = guard::thread_violations();
    let v: Vec<u32> = (0..100).collect();
    assert_eq!(v.len(), 100);
    assert_eq!(guard::thread_violations(), before);
}

// Under RealtimeSanitizer an allocation in a real-time section ends the process, which is the
// sanitizer doing its job; the counting behaviour is tested in ordinary builds.
#[cfg(not(feature = "rtsan"))]
#[test]
fn allocation_inside_rt_is_counted() {
    let ((), n) = guard::rt_scope_counted(|| {
        let b = std::hint::black_box(Box::new(7u64)); // one allocation
        drop(b); // one free
    });
    assert_eq!(n, 2);
}

#[test]
fn permit_lifts_the_check() {
    let ((), n) = guard::rt_scope_counted(|| {
        guard::permit(|| drop(std::hint::black_box(vec![1u8; 64])));
    });
    assert_eq!(n, 0);
}

#[test]
fn sections_nest_and_end() {
    assert!(!guard::in_rt());
    let outer = guard::enter();
    {
        let _inner = guard::enter();
        assert!(guard::in_rt());
    }
    assert!(guard::in_rt());
    drop(outer);
    assert!(!guard::in_rt());
}

#[test]
fn queues_and_garbage_are_allocation_free() {
    let (mut tx, mut rx) = spsc::channel::<u64>(64);
    let (mut chute, mut collector) = garbage_channel(8);
    let retired = Box::new([0u8; 4096]);
    let ((), n) = guard::rt_scope_counted(|| {
        for i in 0..64 {
            tx.push(i).unwrap();
        }
        let mut sum = 0;
        while let Ok(v) = rx.pop() {
            sum += v;
        }
        assert_eq!(sum, 2016);
        chute.discard(retired);
    });
    assert_eq!(
        n, 0,
        "SPSC traffic and deferred drop must not touch the heap"
    );
    assert_eq!(collector.collect(), 1);
}
