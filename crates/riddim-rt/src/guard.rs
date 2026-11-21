//! Real-time section guards and an allocation-checking global allocator.
//!
//! The audio thread wraps each callback in [`rt_scope`]. When a binary (or a test harness)
//! installs [`RtCheckAllocator`] as its `#[global_allocator]`, every allocation, reallocation and
//! free that happens inside such a scope is counted as a *violation*. Tests assert the count is
//! zero; debugging runs can turn on [`set_trace`] to print a backtrace for each one, or
//! [`set_abort_on_violation`] to stop at the first.
//!
//! This is our own take on the `assert_no_alloc` idea. It differs in two ways that matter to us:
//! violations are counted rather than fatal by default (so a test can run a whole scenario and
//! then assert on the total), and counts are kept per thread as well as globally (so parallel
//! tests do not see each other's deliberate violations).
//!
//! The allocator only sees heap traffic. Locks and blocking system calls are caught by LLVM's
//! RealtimeSanitizer: with the `rtsan` feature (and `RTSAN_ENABLE=1` at build time) every
//! real-time section is also an RTSan real-time context, and [`permit`] also suspends RTSan.
//! `scripts/rtsan.sh` runs the test suite that way.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

thread_local! {
    static RT_DEPTH: Cell<u32> = const { Cell::new(0) };
    static PERMIT_DEPTH: Cell<u32> = const { Cell::new(0) };
    static THREAD_VIOLATIONS: Cell<u64> = const { Cell::new(0) };
}

static TOTAL_VIOLATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATOR_SEEN: AtomicBool = AtomicBool::new(false);
static TRACE: AtomicBool = AtomicBool::new(false);
static ABORT: AtomicBool = AtomicBool::new(false);

/// RAII marker for a real-time section on the current thread. Created by [`enter`].
#[must_use = "the real-time section ends when this guard is dropped"]
pub struct RtSection {
    _not_send: PhantomData<*const ()>,
}

/// Marks the start of a real-time section on the current thread. Sections nest.
#[inline]
pub fn enter() -> RtSection {
    RT_DEPTH.with(|d| d.set(d.get() + 1));
    #[cfg(feature = "rtsan")]
    {
        // The standalone runtime has to be initialised once, before the first real-time
        // context; without this its interceptors are never installed and it reports nothing.
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(rtsan_standalone::ensure_initialized);
        rtsan_standalone::realtime_enter();
    }
    RtSection {
        _not_send: PhantomData,
    }
}

impl Drop for RtSection {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "rtsan")]
        rtsan_standalone::realtime_exit();
        RT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Runs `f` as a real-time section.
#[inline]
pub fn rt_scope<R>(f: impl FnOnce() -> R) -> R {
    let _section = enter();
    f()
}

/// Runs `f` as a real-time section and returns how many violations this thread recorded in it.
pub fn rt_scope_counted<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = thread_violations();
    let out = rt_scope(f);
    (out, thread_violations() - before)
}

/// Temporarily lifts the checks on this thread.
///
/// Use this only around code we do not control and have decided to tolerate (a third-party
/// plugin's `process`, for instance), never around our own.
#[inline]
pub fn permit<R>(f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            #[cfg(feature = "rtsan")]
            rtsan_standalone::enable();
            PERMIT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
    PERMIT_DEPTH.with(|d| d.set(d.get() + 1));
    #[cfg(feature = "rtsan")]
    rtsan_standalone::disable();
    let _reset = Reset;
    f()
}

/// True if this build ties real-time sections to RealtimeSanitizer. (Whether the sanitizer is
/// actually linked in also depends on `RTSAN_ENABLE` having been set at build time.)
pub const fn rtsan_feature() -> bool {
    cfg!(feature = "rtsan")
}

/// True while the current thread is inside a real-time section.
#[inline]
pub fn in_rt() -> bool {
    RT_DEPTH.with(|d| d.get()) > 0
}

/// Violations recorded on the current thread since it started.
pub fn thread_violations() -> u64 {
    THREAD_VIOLATIONS.with(|v| v.get())
}

/// Violations recorded on all threads since the process started.
pub fn total_violations() -> u64 {
    TOTAL_VIOLATIONS.load(Ordering::Relaxed)
}

/// True once [`RtCheckAllocator`] has served at least one request, i.e. it really is the global
/// allocator. Tests use this to make sure a "zero violations" result is not vacuous.
pub fn allocator_installed() -> bool {
    // Force one allocation through the global allocator so the answer does not depend on
    // whether anything happened to allocate before us.
    drop(std::hint::black_box(Box::new(0u8)));
    ALLOCATOR_SEEN.load(Ordering::Relaxed)
}

/// Print a backtrace to stderr for every violation (slow; for debugging).
pub fn set_trace(on: bool) {
    TRACE.store(on, Ordering::Relaxed);
}

/// Abort the process at the first violation.
pub fn set_abort_on_violation(on: bool) {
    ABORT.store(on, Ordering::Relaxed);
}

#[cold]
fn violation(what: &str, size: usize) {
    TOTAL_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    let _ = THREAD_VIOLATIONS.try_with(|v| v.set(v.get() + 1));
    let trace = TRACE.load(Ordering::Relaxed);
    let abort = ABORT.load(Ordering::Relaxed);
    if trace || abort {
        // Reporting allocates; lift the checks so we do not recurse.
        permit(|| {
            eprintln!("riddim-rt: {what} of {size} bytes inside a real-time section");
            if trace {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
        });
        if abort {
            std::process::abort();
        }
    }
}

#[inline]
fn check(what: &str, size: usize) {
    let hot = RT_DEPTH.try_with(|d| d.get()).unwrap_or(0) > 0
        && PERMIT_DEPTH.try_with(|d| d.get()).unwrap_or(0) == 0;
    if hot {
        violation(what, size);
    }
}

/// A global allocator that forwards to `A` and counts heap traffic inside real-time sections.
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: riddim_rt::guard::RtCheckAllocator = riddim_rt::guard::RtCheckAllocator::system();
/// ```
pub struct RtCheckAllocator<A = System>(pub A);

impl RtCheckAllocator<System> {
    pub const fn system() -> Self {
        RtCheckAllocator(System)
    }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for RtCheckAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATOR_SEEN.store(true, Ordering::Relaxed);
        check("allocation", layout.size());
        unsafe { self.0.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check("free", layout.size());
        unsafe { self.0.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATOR_SEEN.store(true, Ordering::Relaxed);
        check("allocation", layout.size());
        unsafe { self.0.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check("reallocation", new_size);
        unsafe { self.0.realloc(ptr, layout, new_size) }
    }
}
