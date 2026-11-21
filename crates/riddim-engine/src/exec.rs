//! Running a plan's steps: on the audio thread alone, or shared out among worker threads.
//!
//! Both ways run the same function for each step, [`run_step`], on the same memory. A step
//! reads the output buffers of the steps before it and writes only buffers that belong to it
//! alone (the plan compiler gives every output port its own), and it sums its inputs in the
//! order the plan lists them. So what a step computes does not depend on which thread runs it
//! or on what else is running at the time, and a block rendered by eight threads is, bit for
//! bit, the block rendered by one. That is tested, not assumed.
//!
//! The scheduling is the usual one for an audio graph: each step counts the steps it is
//! waiting for; a step that finishes counts its dependents down and queues those that reach
//! zero; threads take steps from the queue until all are done. Nothing here locks, allocates
//! or blocks. Threads with nothing to do for the moment spin; between blocks the workers
//! spin briefly and then park, and the audio thread unparks them when the next block opens.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{JoinHandle, Thread};

use riddim_plugin::EventBuffer;

use crate::nodes::{self, BlockCtx, NodeIo, NodeMessage};
use crate::plan::{Buffers, RtPlan, EVENT_CAPACITY};

/// How many node messages (parameter changes made by plugins, recorded notes) one thread can
/// collect in one block. Beyond this they are dropped and counted.
const MESSAGE_CAPACITY: usize = 4096;

/// How long a worker spins waiting for the next block before it parks. Blocks arrive every
/// millisecond or so at small buffer sizes, so a worker that has just finished one usually
/// catches the next without a system call.
const SPINS_BEFORE_PARK: u32 = 20_000;

/// What a thread needs of its own to run steps: scratch for merging event streams, and
/// somewhere to put the messages nodes hand it.
pub(crate) struct Scratch {
    merged_events: EventBuffer,
    /// (step, order within this thread's block, message).
    messages: Vec<(u32, u32, NodeMessage)>,
    dropped: u64,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch {
            merged_events: EventBuffer::with_capacity(EVENT_CAPACITY),
            messages: Vec::with_capacity(MESSAGE_CAPACITY),
            dropped: 0,
        }
    }
}

/// The plan's buffers as raw parts, so that threads can each take the pieces their step owns.
#[derive(Clone, Copy)]
struct RawBuffers {
    audio: *mut Vec<f32>,
    scratch: *mut Vec<f32>,
    events: *mut EventBuffer,
}

impl RawBuffers {
    fn of(buffers: &mut Buffers) -> RawBuffers {
        RawBuffers {
            audio: buffers.audio.as_mut_ptr(),
            scratch: buffers.scratch.as_mut_ptr(),
            events: buffers.events.as_mut_ptr(),
        }
    }
}

/// Runs step `index` of `plan` for one block.
///
/// # Safety
///
/// * `raw` points into buffers at least as large as the plan needs, each channel at least
///   `ctx.frames` long, and they stay put for the duration of the call.
/// * Every step this one depends on has finished for this block, and this step is run exactly
///   once per block, by one thread.
///
/// Together with what the compiler guarantees (a step's scratch and output buffers, its event
/// buffer, its node state and the delay lines on its inputs belong to it alone) that makes
/// every mutable access here exclusive and every shared access a read of finished data.
unsafe fn run_step(
    plan: &RtPlan,
    index: usize,
    raw: RawBuffers,
    ctx: &BlockCtx<'_>,
    scratch: &mut Scratch,
) {
    let step = &plan.steps[index];
    let frames = ctx.frames;

    // 1. Sum this step's inputs, always in the same order.
    for input in &step.inputs {
        for c in 0..input.channels {
            // SAFETY: this step's own scratch channel.
            let sum = unsafe { &mut *raw.scratch.add(input.first_scratch + c) };
            sum[..frames].fill(0.0);
        }
        for source in &input.sources {
            for c in 0..input.channels {
                // Mono sources feed every channel; otherwise channels map one to one.
                let from = if source.channels == 1 {
                    0
                } else if c < source.channels {
                    c
                } else {
                    continue;
                };
                // SAFETY: an output buffer of a step that has finished; nobody writes it now.
                let signal = unsafe { &*raw.audio.add(source.first_buffer + from) };
                let signal = &signal[..frames];
                // SAFETY: this step's own scratch channel.
                let sum = unsafe { &mut *raw.scratch.add(input.first_scratch + c) };
                let sum = &mut sum[..frames];
                match &source.delay {
                    None => {
                        for (s, x) in sum.iter_mut().zip(signal) {
                            *s += *x;
                        }
                    }
                    // SAFETY: this step is the only user of its sources' delay lines.
                    Some(delay) => unsafe { delay.get() }.process_add(c, signal, sum),
                }
            }
            if let Some(delay) = &source.delay {
                // SAFETY: as above.
                unsafe { delay.get() }.advance(frames);
            }
        }
    }

    // 2. Gather its events, in the order the plan lists their sources.
    scratch.merged_events.clear();
    for &source in &step.event_sources {
        // SAFETY: the event buffer of a step that has finished.
        let events = unsafe { &*raw.events.add(source) };
        for event in events.as_slice() {
            scratch.merged_events.push(*event);
        }
    }
    if step.event_sources.len() > 1 {
        scratch.merged_events.sort();
    }

    // 3. Run the node.
    // SAFETY: this step's own scratch and output ranges, and its own event buffer.
    let io = unsafe {
        NodeIo {
            inputs: std::slice::from_raw_parts(
                raw.scratch.add(step.first_scratch),
                step.in_channels,
            ),
            outputs: std::slice::from_raw_parts_mut(
                raw.audio.add(step.first_buffer),
                step.out_channels,
            ),
            events_in: scratch.merged_events.as_slice(),
            events_out: step.event_output.map(|index| &mut *raw.events.add(index)),
            latency: step.input_latency,
        }
    };
    // SAFETY: we are the one thread executing this step; the reference ends with it.
    let rt = unsafe { step.cell.get() };
    let (messages, dropped) = (&mut scratch.messages, &mut scratch.dropped);
    nodes::process(rt, &step.params, io, ctx, |message| {
        if messages.len() < messages.capacity() {
            messages.push((index as u32, messages.len() as u32, message));
        } else {
            *dropped += 1;
        }
    });
}

/// Per-plan scheduling state for running the plan on several threads. Built with the plan on
/// the control thread; reset for every block.
pub(crate) struct Schedule {
    /// For each step, how many of the steps it waits for have yet to finish this block.
    pending: Vec<AtomicU32>,
    /// Steps ready to run, in the order they became ready. Each slot holds a step index plus
    /// one, or zero while it has been reserved but not yet written.
    ready: Vec<AtomicUsize>,
    /// Next slot of `ready` to take from, and next to write to.
    head: AtomicUsize,
    tail: AtomicUsize,
    finished: AtomicUsize,
}

impl Schedule {
    pub fn new(steps: usize) -> Schedule {
        Schedule {
            pending: (0..steps).map(|_| AtomicU32::new(0)).collect(),
            ready: (0..steps).map(|_| AtomicUsize::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
        }
    }

    /// Sets up for a block. Only while no worker is inside the plan.
    fn reset(&self, plan: &RtPlan) {
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
        self.finished.store(0, Ordering::Relaxed);
        for slot in &self.ready {
            slot.store(0, Ordering::Relaxed);
        }
        for (pending, step) in self.pending.iter().zip(&plan.steps) {
            pending.store(step.dependencies, Ordering::Relaxed);
        }
        for (index, step) in plan.steps.iter().enumerate() {
            if step.dependencies == 0 {
                self.push(index);
            }
        }
    }

    fn push(&self, step: usize) {
        // Every step is pushed exactly once per block, so the slot exists.
        let slot = self.tail.fetch_add(1, Ordering::AcqRel);
        self.ready[slot].store(step + 1, Ordering::Release);
    }

    /// A step to run, if one is ready right now.
    fn pop(&self) -> Option<usize> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let slot = self.ready.get(head)?;
            let value = slot.load(Ordering::Acquire);
            if value == 0 {
                // Nothing ready (or the next one is a moment from being written).
                return None;
            }
            if self
                .head
                .compare_exchange_weak(head, head + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(value - 1);
            }
        }
    }

    fn all_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire) >= self.pending.len()
    }
}

/// The block being worked on. Written by the audio thread before it opens the block, read by
/// workers only while the block is open.
#[derive(Clone, Copy)]
struct Job {
    plan: *const RtPlan,
    raw: RawBuffers,
    ctx: *const BlockCtx<'static>,
}

/// Takes ready steps and runs them until the block is done.
///
/// # Safety
/// `job` describes a block that is open and stays open until this returns, and its pointers
/// are valid for that long.
unsafe fn work(job: &Job, scratch: &mut Scratch) {
    // SAFETY: per the contract above.
    let (plan, ctx) = unsafe { (&*job.plan, &*job.ctx) };
    let schedule = &plan.schedule;
    while !schedule.all_finished() {
        let Some(index) = schedule.pop() else {
            std::hint::spin_loop();
            continue;
        };
        // SAFETY: the step came off the ready queue, so everything it waits for has finished
        // and nobody else has it; the buffers are those of the open block.
        unsafe { run_step(plan, index, job.raw, ctx, scratch) };
        for &dependent in &plan.steps[index].dependents {
            if schedule.pending[dependent].fetch_sub(1, Ordering::AcqRel) == 1 {
                schedule.push(dependent);
            }
        }
        schedule.finished.fetch_add(1, Ordering::AcqRel);
    }
}

struct Worker {
    thread: OnceLock<Thread>,
    parked: AtomicBool,
    scratch: UnsafeCell<Scratch>,
    /// Heap operations this worker's thread has been caught at inside real-time sections.
    violations: AtomicU64,
}

struct Shared {
    /// Odd while a block is open for work, even between blocks.
    epoch: AtomicU64,
    /// Workers currently inside a block.
    active: AtomicUsize,
    job: UnsafeCell<Option<Job>>,
    shutdown: AtomicBool,
    workers: Vec<Worker>,
    /// Called once on each worker thread before it does anything else (joining the device's
    /// audio workgroup, on macOS).
    on_start: Box<dyn Fn() + Send + Sync>,
}

// SAFETY: `job` is written only by the audio thread while the epoch is even and no worker is
// active, and read by workers only after they have registered as active and seen the epoch
// odd; each worker's `scratch` is used by that worker while a block is open and by the audio
// thread only between blocks. The raw pointers inside are to data the audio thread keeps
// alive and still for as long as the block is open.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Worker threads that help the audio thread run plans.
pub(crate) struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    /// Starts `workers` threads. `on_start` runs once on each of them.
    pub fn new(
        workers: usize,
        on_start: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Pool> {
        let shared = Arc::new(Shared {
            epoch: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            job: UnsafeCell::new(None),
            shutdown: AtomicBool::new(false),
            workers: (0..workers)
                .map(|_| Worker {
                    thread: OnceLock::new(),
                    parked: AtomicBool::new(false),
                    scratch: UnsafeCell::new(Scratch::new()),
                    violations: AtomicU64::new(0),
                })
                .collect(),
            on_start: Box::new(on_start),
        });
        let mut threads = Vec::with_capacity(workers);
        for index in 0..workers {
            let shared = shared.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("riddim-render-{}", index + 1))
                    .spawn(move || worker_main(&shared, index))?,
            );
        }
        // Do not hand out the pool until every worker can be woken.
        for worker in &shared.workers {
            while worker.thread.get().is_none() {
                std::thread::yield_now();
            }
        }
        Ok(Pool { shared, threads })
    }

    pub fn workers(&self) -> usize {
        self.shared.workers.len()
    }

    /// Heap operations the workers have been caught at inside real-time sections.
    pub fn violations(&self) -> u64 {
        self.shared
            .workers
            .iter()
            .map(|w| w.violations.load(Ordering::Relaxed))
            .sum()
    }

    /// Runs every step of `plan` for one block, with the workers' help, and returns when all
    /// are done and every worker has left the plan.
    fn run(&self, plan: &RtPlan, raw: RawBuffers, ctx: &BlockCtx<'_>, scratch: &mut Scratch) {
        let shared = &*self.shared;
        plan.schedule.reset(plan);
        let job = Job {
            plan,
            raw,
            // The lifetime is erased here and honoured by not returning until the block is
            // closed and every worker is out.
            ctx: (ctx as *const BlockCtx<'_>).cast(),
        };
        // SAFETY: the epoch is even and no worker is active (the previous block was closed
        // the same way this one will be), so nobody is reading the job.
        unsafe { *shared.job.get() = Some(job) };
        shared.epoch.fetch_add(1, Ordering::SeqCst);
        for worker in &shared.workers {
            if worker.parked.load(Ordering::SeqCst) {
                if let Some(thread) = worker.thread.get() {
                    // Waking a parked thread is a system call (a futex wake on Linux, a
                    // semaphore signal on macOS), and the one thing here the sanitizer objects
                    // to. It does not block or wait on anything the woken thread holds, and
                    // there is no way to wake a sleeping thread without one; a worker that
                    // was still spinning is not woken at all.
                    riddim_rt::guard::permit(|| thread.unpark());
                }
            }
        }

        // SAFETY: the block is open until we close it below.
        unsafe { work(&job, scratch) };

        // Close the block, then wait for workers that were inside it to notice and leave.
        // (They have nothing left to do, so this is a matter of a few instructions each.)
        shared.epoch.fetch_add(1, Ordering::SeqCst);
        while shared.active.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        for worker in &self.shared.workers {
            if let Some(thread) = worker.thread.get() {
                thread.unpark();
            }
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn worker_main(shared: &Shared, index: usize) {
    let me = &shared.workers[index];
    let _ = me.thread.set(std::thread::current());
    (shared.on_start)();
    // The same floating-point mode as the audio thread, for the life of this one: a worker
    // that flushed denormals differently would compute different samples.
    let _denormals = riddim_rt::denormal::ScopedNoDenormals::new();
    let mut seen = 0u64;
    loop {
        // Wait for a block to open that we have not worked on yet: spin for a while, then park.
        let mut spins = 0u32;
        let epoch = loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let epoch = shared.epoch.load(Ordering::SeqCst);
            if epoch % 2 == 1 && epoch != seen {
                break epoch;
            }
            spins += 1;
            if spins < SPINS_BEFORE_PARK {
                std::hint::spin_loop();
                continue;
            }
            me.parked.store(true, Ordering::SeqCst);
            // Look once more before sleeping: the audio thread may have opened a block (and
            // looked at `parked`) between our last look and setting the flag.
            let again = shared.epoch.load(Ordering::SeqCst);
            if !(again % 2 == 1 && again != seen) && !shared.shutdown.load(Ordering::SeqCst) {
                std::thread::park();
            }
            me.parked.store(false, Ordering::SeqCst);
            spins = 0;
        };
        seen = epoch;

        // Register, then make sure the block is still the one we saw: the audio thread closes
        // a block by changing the epoch and then waiting for `active` to reach zero, so either
        // it sees us or we see that it has closed.
        shared.active.fetch_add(1, Ordering::SeqCst);
        if shared.epoch.load(Ordering::SeqCst) == epoch {
            // SAFETY: the block is open and we are registered, so the job is set and stays
            // valid until we deregister; the scratch is this worker's own.
            let job = unsafe { (*shared.job.get()).expect("an open block has a job") };
            let scratch = unsafe { &mut *me.scratch.get() };
            riddim_rt::guard::rt_scope(|| unsafe { work(&job, scratch) });
            me.violations
                .store(riddim_rt::guard::thread_violations(), Ordering::Relaxed);
        }
        shared.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs plans: alone, or with a pool of workers for plans big enough to be worth sharing out.
pub(crate) struct Executor {
    pool: Option<Pool>,
    /// Plans with fewer steps than this run on the audio thread alone.
    parallel_from: usize,
    scratch: Scratch,
}

impl Executor {
    pub fn new(pool: Option<Pool>, parallel_from: usize) -> Executor {
        Executor {
            pool: pool.filter(|pool| pool.workers() > 0),
            parallel_from,
            scratch: Scratch::new(),
        }
    }

    /// Heap operations the workers have been caught at inside real-time sections.
    pub fn worker_violations(&self) -> u64 {
        self.pool.as_ref().map_or(0, Pool::violations)
    }

    /// Runs every step of `plan` once for the block `ctx` describes, then hands each message
    /// the nodes produced to `deliver`, in step order. Returns how many messages were dropped
    /// for want of room.
    pub fn run(
        &mut self,
        plan: &RtPlan,
        buffers: &mut Buffers,
        ctx: &BlockCtx<'_>,
        mut deliver: impl FnMut(NodeMessage),
    ) -> u64 {
        let raw = RawBuffers::of(buffers);
        match &self.pool {
            Some(pool) if plan.steps.len() >= self.parallel_from => {
                pool.run(plan, raw, ctx, &mut self.scratch);
                // Every worker has left the plan, so their scratch is ours to read.
                let mut dropped = std::mem::take(&mut self.scratch.dropped);
                for worker in &pool.shared.workers {
                    // SAFETY: no block is open, so the worker is not using its scratch.
                    let theirs = unsafe { &mut *worker.scratch.get() };
                    dropped += std::mem::take(&mut theirs.dropped);
                    for message in theirs.messages.drain(..) {
                        if self.scratch.messages.len() < self.scratch.messages.capacity() {
                            self.scratch.messages.push(message);
                        } else {
                            dropped += 1;
                        }
                    }
                }
                // Which thread ran which step varies; the order messages go out in must not.
                // A step's messages all come from the one thread that ran it, in order, so
                // (step, order on that thread) is a total order, and an in-place sort will do
                // (the stable one allocates).
                self.scratch
                    .messages
                    .sort_unstable_by_key(|(step, order, _)| (*step, *order));
                for (_, _, message) in self.scratch.messages.drain(..) {
                    deliver(message);
                }
                dropped
            }
            _ => {
                for index in 0..plan.steps.len() {
                    // SAFETY: steps run in plan order, which respects every dependency, one at
                    // a time, on buffers borrowed for the whole call.
                    unsafe { run_step(plan, index, raw, ctx, &mut self.scratch) };
                }
                for (_, _, message) in self.scratch.messages.drain(..) {
                    deliver(message);
                }
                std::mem::take(&mut self.scratch.dropped)
            }
        }
    }
}
