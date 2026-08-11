//! Kernel-side scheduler: per-CPU run queues, the thread table, and `schedule()`.
//!
//! The *policy* — which thread runs next — lives in `qunix-sched` and is
//! host-tested. This module is everything that policy deliberately knows
//! nothing about: real stacks, the context switch, and the locking.
//!
//! # What is per-CPU and what is shared
//!
//! `current` and the run queue are per-CPU, in `percpu::PerCpu`. They have to
//! be: a single `current` field is one "what am I running" slot shared between
//! CPUs, and the first switch would have one CPU save its stack pointer into
//! the other's context — two threads on one stack, which is the failure class
//! this project has already shipped twice in the allocator.
//!
//! The thread *table* and the reapable list are shared, because a thread can be
//! created on one CPU and reaped on another, and they are behind [`SCHED`].
//! That lock is only taken to look a thread up; the scheduling decision itself
//! touches per-CPU state.
//!
//! Lock order: `SCHED` may be taken while no run-queue lock is held, and a run
//! queue is a leaf. Nothing acquires `SCHED` while holding a run queue, which
//! is what lets two CPUs steal from each other without deadlocking.
//!
//! # The lock is never held across a switch
//!
//! `schedule` takes the scheduler lock, decides, updates the table, and then
//! **drops the lock before switching**. Holding it across `context::switch`
//! would deadlock the moment the incoming thread tried to schedule: the lock
//! would be held by a thread that is no longer running and cannot release it
//! until it is scheduled again. Every path through this file that reaches a
//! switch must therefore end its borrow first, which is why the switch happens
//! on raw pointers copied out of the guard rather than through it.
//!
//! # A thread is never runnable and running at once
//!
//! Dispatch *removes* a thread from a run queue — `pop` locally, `steal`
//! remotely — and that removal is the only thing preventing two CPUs from
//! resuming one context. The other half of the same invariant is that the
//! outgoing thread is **not** requeued before the switch: see [`HANDOFF_ID`].

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use qunix_hal_x86_64::context::{self, Context};
use qunix_hal_x86_64::percpu::{self, MAX_CPUS, NO_THREAD};
use qunix_sched::{ParkOutcome, Priority, ThreadId, WakeOutcome};
use qunix_sync::{IrqControl, IrqSpinLock};

use crate::thread::{Thread, ThreadState};

/// The thread table and everything that is genuinely shared, under one lock.
struct Scheduler {
    threads: BTreeMap<ThreadId, Thread>,
    next_id: u64,
    /// Threads that have exited and whose stacks and address spaces are waiting
    /// to be freed.
    ///
    /// A thread can free neither: it is standing on the stack, and the CPU may
    /// still hold the address space in CR3. So `exit` records the id here and
    /// the next thread to run does the freeing.
    reapable: alloc::vec::Vec<ThreadId>,
}

impl Scheduler {
    const fn new() -> Self {
        Self { threads: BTreeMap::new(), next_id: 0, reapable: alloc::vec::Vec::new() }
    }

    /// Allocates an id that has never been used before.
    ///
    /// **Never reusing an id is load-bearing outside this function.** A
    /// `Waker` carries a bare `ThreadId` as its data word -- no refcount, no
    /// generation -- which is only sound because an id that outlives its
    /// thread can never come to name a different one. `checked_add` rather
    /// than `+=` for the same reason: the kernel builds without overflow
    /// checks in release, so a wrapped counter would silently reintroduce
    /// reuse, and the first symptom would be a completion waking an unrelated
    /// thread.
    ///
    /// No test covers the overflow and none can: reaching it needs 2^64 spawns,
    /// so mutating it to `wrapping_add` leaves the suite green. Kept because it
    /// is correct and free, and recorded here so the next mutation pass reads
    /// this as unfalsifiable rather than as a hole.
    fn allocate_id(&mut self) -> ThreadId {
        let id = ThreadId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("thread ids exhausted");
        // `NO_THREAD` is the per-CPU "nothing is running" sentinel, so a thread
        // carrying it would be indistinguishable from an empty slot.
        assert_ne!(id.0, NO_THREAD, "thread id collided with the empty sentinel");
        id
    }
}

/// `IrqSpinLock`, not `SpinLock`: the timer interrupt calls into the scheduler,
/// so a plain spinlock would let a tick land on a CPU that already holds it and
/// spin forever against itself.
static SCHED: IrqSpinLock<Scheduler, qunix_hal_x86_64::Irq> = IrqSpinLock::new(Scheduler::new());

/// CPUs that have adopted their running context as a thread, one bit per
/// `cpu_id`.
///
/// Per-CPU rather than a single flag, because every CPU needs an entry of its
/// own before it can schedule — `schedule` saves the outgoing context into a
/// table entry, and a CPU without one would have to borrow another's.
static IDLE_INSTALLED: AtomicU64 = AtomicU64::new(0);

/// The thread that has just given up a CPU, per CPU, waiting to be made
/// runnable again.
///
/// It cannot be requeued before the switch. Between a push and
/// `context::switch` storing the thread's stack pointer there is a window in
/// which another CPU is free to pop that thread and resume it — through a
/// context that has not been saved yet, which puts two CPUs on one stack. So
/// the outgoing thread is handed to whatever runs on this CPU *next*, which by
/// construction runs after the save has completed, and that publishes it.
///
/// Written and read only by the owning CPU, with interrupts masked, so
/// `Relaxed` is enough: the ordering that matters is program order on one CPU.
static HANDOFF_ID: [AtomicU64; MAX_CPUS as usize] =
    [const { AtomicU64::new(NO_THREAD) }; MAX_CPUS as usize];
static HANDOFF_PRIORITY: [AtomicU8; MAX_CPUS as usize] =
    [const { AtomicU8::new(0) }; MAX_CPUS as usize];

/// Threads taken from another CPU's run queue.
///
/// Counted because a work-stealing path that is merely *uncovered* is
/// indistinguishable from one that does not work: on a machine where the owning
/// CPU always got there first, every assertion about the threads having run
/// would still hold. A test can require this to move.
static STEALS: AtomicU64 = AtomicU64::new(0);

/// Total threads taken from another CPU's run queue since boot.
///
/// Since boot, and never reset: a test that wants to prove *it* caused a steal
/// must sample this before and compare, not assert it is non-zero. An absolute
/// test passes on the strength of every earlier steal in the same boot, which
/// is how a regression test for the stealing path came to be unable to fail.
pub fn steal_count() -> u64 {
    STEALS.load(Ordering::Acquire)
}

/// Steal attempts abandoned because the remote queue was locked.
///
/// Counted because the `try_lock` in [`take_next`] cannot distinguish "no work
/// anywhere" from "could not look". Since the reordering that puts the steal
/// loop on the routine path -- it now runs whenever a CPU has no local runnable
/// work, rather than almost never -- contention on it became load-bearing, and
/// a CPU that skipped every victim runs its idle thread while runnable work
/// exists one queue away. That is bounded: the next tick tries again, and
/// `idle_loop` re-checks before halting. It is not a starvation bug, but it is
/// invisible without this, and "the scheduler is idle while work is queued" is
/// not a state anyone should have to infer.
static STEAL_CONTENDED: AtomicU64 = AtomicU64::new(0);

/// Steal attempts abandoned because the remote queue was locked, since boot.
pub fn steal_contention_count() -> u64 {
    STEAL_CONTENDED.load(Ordering::Acquire)
}

/// Times each CPU has gone round [`idle_loop`].
///
/// Per-CPU rather than a total, because the question a caller asks of this is
/// always about one processor: did *this* CPU, which is running a thread that
/// never yields, get back to its idle thread. A sum would answer yes whenever
/// any other CPU was idle, which is the opposite of the question.
static IDLE_ROUNDS: [AtomicU64; MAX_CPUS as usize] =
    [const { AtomicU64::new(0) }; MAX_CPUS as usize];

/// How many times `cpu` has gone round the idle loop.
pub fn idle_rounds(cpu: u32) -> u64 {
    IDLE_ROUNDS[cpu as usize].load(Ordering::Relaxed)
}

/// Whether a timer tick may switch threads.
///
/// Off until something explicitly turns it on. Early boot, and any window that
/// is not prepared to lose the CPU, would otherwise be preempted the moment the
/// APIC timer starts -- which on this kernel is before the scheduler has a
/// thread table.
static PREEMPT: AtomicBool = AtomicBool::new(false);

/// Enables or disables timer-driven preemption, returning the previous setting.
pub fn set_preemption(enabled: bool) -> bool {
    PREEMPT.swap(enabled, Ordering::AcqRel)
}

pub fn preemption_enabled() -> bool {
    PREEMPT.load(Ordering::Acquire)
}

/// Timer-tick entry point.
///
/// Separate from [`yield_now`] because the constraints differ: this runs in
/// interrupt context, must do nothing at all when this CPU is not ready, and
/// must never panic -- a panic here fires on every subsequent tick.
///
/// The readiness check is per-CPU rather than global: every CPU starts its
/// LAPIC timer as part of its own bring-up, and a tick that arrived before that
/// CPU adopted an idle thread would have `schedule` save its context into
/// whatever `NO_THREAD` names.
///
/// The caller must have signalled EOI already. Switching first would leave the
/// LAPIC waiting for an EOI that only arrives when this thread is scheduled
/// again, so the CPU would take no further timer interrupts in the meantime.
pub fn preempt() {
    if !PREEMPT.load(Ordering::Acquire) || percpu::current_thread() == NO_THREAD {
        return;
    }
    schedule(ThreadState::Ready);
}

/// Adopts the currently-executing context as this CPU's idle thread.
///
/// The boot path of every CPU is already a thread in everything but name: it
/// has a stack and a register state. Rather than construct a fake one, it is
/// adopted, so the first `schedule` on that CPU has somewhere to save its
/// context to.
///
/// Idempotent per CPU. The in-QEMU harness runs several tests in one boot;
/// re-adopting would orphan every thread the previous test spawned while
/// leaving their stacks allocated.
pub fn init() {
    let cpu = percpu::cpu_id();
    let bit = 1u64 << cpu;
    if IDLE_INSTALLED.load(Ordering::Acquire) & bit != 0 {
        return;
    }
    let id = {
        let mut sched = SCHED.lock();
        let id = sched.allocate_id();
        sched.threads.insert(id, Thread::adopt_current());
        id
    };
    // SAFETY: this is the CPU being initialised, and nothing else can be
    // scheduling on it — it has no idle thread until this line.
    unsafe { percpu::set_current_thread(id.0) };
    IDLE_INSTALLED.fetch_or(bit, Ordering::AcqRel);
}

/// What a new thread should run, handed to [`thread_entry`] through the single
/// `u64` the context switch can carry.
struct ThreadStart {
    entry: extern "C" fn(u64) -> !,
    arg: u64,
}

/// Every kernel thread's real first instruction.
///
/// `schedule` leaves interrupts disabled across a switch and restores them
/// after it returns; a thread running for the first time never reaches that
/// restore, so it would run with interrupts masked forever -- no preemption,
/// no timer, and a `yield_now` that can never be interrupted. Enabling them
/// here is what makes a fresh thread indistinguishable from a resumed one.
extern "C" fn thread_entry(raw: u64) -> ! {
    // Before anything else, and for the same reason `schedule` does it after
    // its own switch: the thread that gave this CPU up is still in nobody's run
    // queue, and this is the first code to run after its context was saved.
    publish_handoff();
    let start = unsafe { alloc::boxed::Box::from_raw(raw as *mut ThreadStart) };
    let ThreadStart { entry, arg } = *start;
    x86_64::instructions::interrupts::enable();
    entry(arg)
}

/// Creates a runnable kernel thread on the calling CPU.
///
/// Queued locally rather than spread round-robin: the thread has no cache
/// footprint anywhere yet, and an idle CPU will take it by stealing, which is
/// the mechanism that already has to be right. Spreading here would make
/// stealing the rarely-exercised path instead.
pub fn spawn_kernel(entry: extern "C" fn(u64) -> !, arg: u64, prio: Priority) -> ThreadId {
    let start = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(ThreadStart { entry, arg }));
    let id = {
        let mut sched = SCHED.lock();
        let id = sched.allocate_id();
        sched.threads.insert(id, Thread::new_kernel(thread_entry, start as u64, prio));
        id
    };
    // The table lock is released first: a run queue is a leaf lock and taking
    // it under `SCHED` would give this path a lock order nothing else has.
    percpu::run_queue().lock().push(id, prio);
    // Idle CPUs are halted, and nothing else will wake them. Sent after the
    // push, so a CPU woken by it is guaranteed to see the work.
    qunix_hal_x86_64::apic::send_ipi_all_excluding_self(crate::WAKE_VECTOR);
    id
}

/// The thread this CPU is running.
pub fn current_id() -> ThreadId {
    ThreadId(percpu::current_thread())
}

/// Live threads, including running ones and any awaiting reaping.
pub fn thread_count() -> usize {
    SCHED.lock().threads.len()
}

/// Whether `id` is still in the thread table.
///
/// A count is not enough for a caller waiting on one specific thread: the
/// harness shares one scheduler across every test, so an unrelated thread
/// starting or finishing moves the count without saying anything about the
/// thread the caller cares about.
pub fn thread_id_is_live(id: ThreadId) -> bool {
    SCHED.lock().threads.contains_key(&id)
}

/// Gives the running thread ownership of the address space it has activated.
///
/// Called by `process::user_thread_entry` immediately before it enters ring 3
/// and stops being able to own anything: `enter_user` never returns, so the
/// only owner that can outlive it is the thread table entry, which `reap`
/// already reclaims from a different thread.
///
/// The space is *not* dropped here under any circumstance — that is the whole
/// point. Dropping it would free the page tables the caller is about to
/// execute on.
pub fn adopt_address_space(space: crate::vmspace::VmSpace) {
    let current = current_id();
    let mut sched = SCHED.lock();
    let thread =
        sched.threads.get_mut(&current).expect("the running thread is not in the table");
    // Asserted rather than replaced. A `replace` would drop the previous space
    // right here — with the scheduler lock held, so the frame allocator would
    // be entered underneath it, and on a CPU that may still be running on the
    // tables being freed. A thread enters ring 3 exactly once, so a second call
    // is a bug rather than a case to handle.
    assert!(
        thread.address_space.is_none(),
        "{current:?} adopted a second address space; the first would be leaked"
    );
    thread.address_space = Some(space);
}

/// Runnable threads across every CPU, excluding idle threads.
///
/// The idle band is excluded because "is there work to do" is what callers
/// mean: a CPU's idle thread is queued while that CPU runs something else, and
/// counting it would report work where there is none.
/// Every CPU that has a run queue at all, whether or not it is scheduling yet.
///
/// Used for counting and for the reap guard, both of which must not miss a CPU:
/// a CPU is installed before it is marked online, and during that window it
/// already has a current thread and can already hold queued work.
pub fn runnable_count() -> usize {
    (0..MAX_CPUS)
        .filter_map(percpu::run_queue_of)
        .map(|queue| queue.lock().runnable_len())
        .sum()
}

/// Every CPU that is actually scheduling, by id.
///
/// Narrower than "has a block" on purpose: stealing from a CPU that has not
/// entered the scheduler would move work somewhere nothing is going to run it.
fn each_online_cpu() -> impl Iterator<Item = u32> {
    let online = percpu::online_mask();
    (0..MAX_CPUS).filter(move |cpu| online & (1u64 << cpu) != 0)
}

/// Yields the CPU to the next runnable thread, if there is one.
///
/// Returns without switching when nothing else is runnable — that is the
/// common case for an idle CPU and is not an error.
pub fn yield_now() {
    schedule(ThreadState::Ready);
}

/// Set by a test to the id whose park window should be held open.
#[cfg(test)]
static WIDEN_PARK: AtomicU64 = AtomicU64::new(NO_THREAD);

/// Holds a park window open so another processor can win the race with it.
#[cfg(test)]
fn widen_the_park_window(me: ThreadId) {
    if WIDEN_PARK.load(Ordering::Acquire) != me.0 {
        return;
    }
    // Consumed, so one arming widens one park. A sticky flag would slow every
    // later park in the suite and hide a regression behind the delay.
    WIDEN_PARK.store(NO_THREAD, Ordering::Release);
    WIDENED.fetch_add(1, Ordering::AcqRel);
    // Interrupts are masked here, so this cannot yield; the waking processor is
    // a different one and needs only wall-clock.
    for _ in 0..2_000_000 {
        core::hint::spin_loop();
    }
}

/// How many times the park window has actually been held open.
#[cfg(test)]
static WIDENED: AtomicU64 = AtomicU64::new(0);

/// How many times the park window has been held open. Test-only.
#[cfg(test)]
pub fn widened_count() -> u64 {
    WIDENED.load(Ordering::Acquire)
}

/// Arms the park-window hook for `id`. Test-only.
#[cfg(test)]
pub fn widen_next_park(id: ThreadId) {
    WIDEN_PARK.store(id.0, Ordering::Release);
}

/// Whether any processor names `id` as the thread it is running. Test-only
/// accessor for the predicate `unpark` gates its queueing on.
#[cfg(test)]
pub fn is_running_somewhere(id: ThreadId) -> bool {
    is_current_anywhere(id)
}

/// Blocks the calling thread until something calls [`unpark`] on it.
///
/// Returns when the thread is woken, which may be spuriously: the contract is
/// only that a woken thread eventually runs, so callers must re-check their
/// condition. That is what `task::block_on`'s loop does.
pub fn park() {
    let me = current_id();
    loop {
        // Masked across the whole decision. The state transition and the
        // switch must not be split by a tick: between them this CPU's
        // `current` and the thread's park state disagree, and a preemption
        // there would requeue a thread on its way to being blocked.
        let irq = qunix_hal_x86_64::Irq::disable_and_save();
        {
            let mut sched = SCHED.lock();
            let Some(thread) = sched.threads.get_mut(&me) else {
                // Not in the table. Only reachable if the caller is not a real
                // thread; returning is the only thing left that is not a hang.
                qunix_hal_x86_64::Irq::restore(irq);
                return;
            };
            match thread.park.park() {
                // A wakeup was already waiting, so this is the park that must
                // not happen.
                //
                // Returning here is an *optimisation*, not the guard. `park`
                // has already consumed the pending wake and left `parked`
                // false, so falling through to `schedule` would find the thread
                // unparked, hand it on as `Ready`, and resume it anyway -- the
                // saving is one pointless context switch. Said plainly because
                // mutating this arm to fall through fails no test, and the next
                // reader deserves to know that is correct rather than a hole:
                // the correctness lives in `schedule`'s re-check.
                //
                // Returning here is an *optimisation*, not the guard: `park`
                // has already consumed the pending wake and left `parked`
                // false, so falling through to `schedule` would find the thread
                // unparked, hand it on as `Ready` and resume it anyway. The
                // saving is one pointless context switch. Said plainly because
                // mutating this arm to fall through does not fail any test, and
                // the next reader deserves to know that is correct rather than
                // a hole -- the correctness is `schedule`'s re-check.
                ParkOutcome::Cancelled => {
                    qunix_hal_x86_64::Irq::restore(irq);
                    return;
                }
                ParkOutcome::Parked => thread.state = ThreadState::Blocked,
            }
        }

        // The window this test hook widens is the one `unpark`'s
        // `is_current_anywhere` check exists for: the scheduler lock is
        // released above and retaken inside `schedule`, and a wake arriving in
        // between finds this thread parked *and* still current. Without the
        // check it is pushed onto a run queue while it is executing, and
        // another processor resumes a context that is still in use.
        //
        // The window is a few instructions wide, so a test that merely hoped to
        // hit it would be a statement about the scheduler's timing rather than
        // about the guard -- CLAUDE.md is explicit that contention-dependent
        // paths must have their contention forced. Compiled out entirely when
        // not testing.
        #[cfg(test)]
        widen_the_park_window(me);

        // `schedule` re-checks the park under its *own* acquisition of SCHED,
        // because the lock is released between here and there and a completion
        // in that window clears the park without queueing the thread (it is
        // still current). Switching away on the stale decision is the lost
        // wakeup in its most expensive form.
        schedule(ThreadState::Blocked);

        // Two ways to arrive here: something else ran and switched back, or
        // there was nothing runnable and `schedule` returned without
        // switching. One question answers both.
        if !is_parked(me) {
            qunix_hal_x86_64::Irq::restore(irq);
            return;
        }

        // Still parked, and still running, because nothing else was runnable.
        // Halting is the only correct move: returning would run a blocked
        // thread at a suspension point it has not returned from, and spinning
        // would occupy the CPU the completion interrupt has to be delivered
        // on. `sti; hlt` is one instruction pair for the reason `idle_loop`
        // documents -- `sti` takes effect only after the following
        // instruction, so an interrupt that arrived while masked is delivered
        // once `hlt` has been entered, which is what wakes it.
        // SAFETY: no memory is touched and no stack slot is used.
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };

        // Re-checked here, *before* looping back to park again. Without this
        // the loop's next iteration calls `park` on a thread that has just been
        // woken: `wake` cleared `parked` without setting `pending`, so the park
        // succeeds, the thread blocks again, and the future is never re-polled.
        // The wake is consumed and its effect discarded -- a hang whose cause
        // is one missing check, and the exact bug the first `sleep_ticks` test
        // found. The post-`schedule` check above cannot cover it: this path
        // never reaches `schedule`, because there was nothing to switch to.
        if !is_parked(me) {
            qunix_hal_x86_64::Irq::restore(irq);
            return;
        }
    }
}

/// Makes a parked thread runnable. Safe to call from an interrupt handler.
///
/// Unknown ids are ignored rather than asserted on. A `Waker` outlives the
/// thread it names -- a device completes an I/O for a thread that has already
/// exited -- and ids are never reused, so an id absent from the table can only
/// be a dead thread. Panicking here would turn every late completion into a
/// kernel panic from interrupt context.
pub fn unpark(id: ThreadId) {
    // Whether the thread must be pushed onto a run queue, decided under the
    // lock and acted on after it is released.
    let mut woke = false;
    let queue_at: Option<Priority> = {
        let mut sched = SCHED.lock();
        let Some(thread) = sched.threads.get_mut(&id) else {
            return;
        };
        match thread.park.wake() {
            // Not parked. The wakeup is recorded in the park state and the
            // thread's own `park` will consume it; queueing it here would put
            // a running thread in a run queue.
            WakeOutcome::Noted => None,
            WakeOutcome::Runnable => {
                thread.state = ThreadState::Ready;
                woke = true;
                let prio = thread.priority;
                // A thread still *current* somewhere has not finished
                // switching away, so it needs no queueing -- it will observe
                // the cleared park when `schedule` returns to it. Queueing it
                // is the two-CPUs-one-stack failure: this CPU would push an id
                // another CPU is executing.
                if is_current_anywhere(id) { None } else { Some(prio) }
            }
        }
    };

    // The scheduler lock is released first. A run queue is a leaf lock, and
    // taking one under SCHED would give this path a lock order nothing else in
    // the kernel has -- see the module docs.
    if let Some(prio) = queue_at {
        percpu::run_queue().lock().push(id, prio);
    }
    // Sent on *any* wake that made a thread runnable, including the one that
    // queued nothing. A thread that was still current somewhere is sitting in
    // `park`'s `sti; hlt`, and leaving that needs an interrupt: without this it
    // would wait for the next timer tick, and on a processor whose timer is not
    // running it would wait forever. Sent after the push for the reason
    // `spawn_kernel` documents -- a processor woken by it must be able to see
    // the work.
    //
    // Latency rather than correctness, so no test fails without it: a halted
    // thread is also woken by the next timer tick. It matters on a processor
    // whose timer is not running, which is why it is not left to the tick.
    if woke {
        qunix_hal_x86_64::apic::send_ipi_all_excluding_self(crate::WAKE_VECTOR);
    }
}

/// Every thread the scheduler currently considers parked.
///
/// Allocates, so it is not for any hot path -- it exists for the harness check
/// that no parked thread is queued, which runs once per test.
pub fn parked_thread_ids() -> alloc::vec::Vec<ThreadId> {
    SCHED
        .lock()
        .threads
        .iter()
        .filter(|(_, t)| t.park.is_parked())
        .map(|(id, _)| *id)
        .collect()
}

/// Whether `id` is currently parked.
pub fn is_parked(id: ThreadId) -> bool {
    SCHED.lock().threads.get(&id).is_some_and(|t| t.park.is_parked())
}

/// Terminates the calling thread. Never returns.
///
/// The stack is *not* freed here: this code is running on it. The thread is
/// marked `Exited` and its id queued for another thread to reap.
pub fn exit_current() -> ! {
    schedule(ThreadState::Exited);
    // `schedule` only returns when it did not switch away, which for an exiting
    // thread means there was nothing to switch to. There is no correct
    // behaviour left: the caller cannot return, and continuing would run an
    // exited thread.
    panic!("the last thread exited with nothing else to run");
}

/// What this CPU should run next, taken *out* of whichever queue held it.
///
/// Removal before dispatch is what stops two CPUs running one thread, and it is
/// why both halves of this are `pop`/`steal` rather than a peek.
///
/// The order is local *runnable* work, then a steal, then this CPU's own idle
/// thread. That middle step is not an optimisation. An earlier version popped
/// whatever the local queue held and only stole when the queue was empty, and
/// the local queue is essentially never empty: a CPU's idle thread is pushed
/// back onto it the moment the CPU switches to anything else, so from the first
/// dispatch onward the CPU always had *something* local and never looked
/// elsewhere again.
///
/// The consequence was that work queued on a CPU which then stopped scheduling
/// starved indefinitely, while every other CPU cycled between its own resident
/// thread and its own idle thread with real work sitting one queue away.
/// Ordering the idle thread last states the rule plainly: **before running
/// nothing, look for something.**
/// Whether this CPU should dispatch from its own queue rather than look
/// elsewhere first.
///
/// The entire content of this function is *which question is asked*: whether
/// the local queue holds runnable work, not whether it holds anything. A CPU's
/// idle thread is pushed back onto its own queue whenever the CPU switches to
/// something else, so "is there anything local" answers yes from the first
/// dispatch onward and forever after — which is precisely the bug this
/// replaced. "Is there anything local worth running" is a different question
/// with a different answer.
///
/// Split out from [`take_next`] so that distinction is stated once and can be
/// asserted without four processors. It is thin on purpose; the integration
/// proof is `work_stranded_on_a_processor_that_stops_scheduling_is_migrated`.
const fn prefers_local(local_runnable: usize) -> bool {
    local_runnable > 0
}

fn take_next(cpu: u32) -> Option<ThreadId> {
    {
        // Locally first: a thread that last ran here has its stack, and
        // possibly its address space, warm in this CPU's caches.
        //
        // One acquisition for both the test and the take. Asking
        // `runnable_len` under one lock and popping under another lets a thief
        // empty the queue in between, and the `pop` would then return this
        // CPU's idle thread from the branch that has just established there is
        // real work to run.
        let mut queue = percpu::run_queue().lock();
        if prefers_local(queue.runnable_len()) {
            // `pop` serves the highest non-empty band and `runnable_len`
            // excludes the idle band, so a non-zero count here guarantees this
            // is a real thread rather than the idle one.
            if let Some(id) = queue.pop() {
                return Some(id);
            }
        }
    }
    for other in each_online_cpu() {
        if other == cpu {
            continue;
        }
        let Some(remote) = percpu::run_queue_of(other) else {
            continue;
        };
        // `try_lock`: the owner may be mid-dispatch, and a thief that blocks on
        // it turns a missed steal into a stalled CPU. Only one queue is ever
        // held at a time on this path, so two CPUs stealing from each other
        // cannot deadlock however the try_locks land.
        let Some(mut queue) = remote.try_lock() else {
            // Counted, not merely skipped. A miss here is indistinguishable
            // from an empty queue by every other observable, and after the
            // reordering this loop is the routine path rather than a rarity.
            STEAL_CONTENDED.fetch_add(1, Ordering::AcqRel);
            continue;
        };
        // Idle threads must never be stolen: an idle thread adopted the stack
        // its own CPU booted on, so running it here would put this CPU on
        // another CPU's stack. `runnable_len` excludes the idle band, and
        // `steal` serves the highest non-empty band, so a non-zero count here
        // guarantees the thread it returns is not an idle one.
        if queue.runnable_len() == 0 {
            continue;
        }
        if let Some(id) = queue.steal() {
            STEALS.fetch_add(1, Ordering::AcqRel);
            return Some(id);
        }
    }
    // Nothing runnable anywhere, so this CPU's own idle thread is the answer.
    // Only an idle-band thread can be here -- the runnable band was taken
    // above -- and it is never taken from another CPU, because an idle thread
    // adopted the stack its own CPU booted on.
    percpu::run_queue().lock().pop()
}

/// Makes the thread that gave up this CPU runnable again.
///
/// Runs on whatever this CPU turned to next, which is the earliest point at
/// which the outgoing thread's context is safely stored. See [`HANDOFF_ID`].
fn publish_handoff() {
    let cpu = percpu::cpu_id();
    let raw = HANDOFF_ID[cpu as usize].swap(NO_THREAD, Ordering::Relaxed);
    if raw == NO_THREAD {
        return;
    }
    let prio = priority_from_raw(HANDOFF_PRIORITY[cpu as usize].load(Ordering::Relaxed));
    // Checked at the point of violation rather than sampled afterwards.
    //
    // "A parked thread is in no run queue" is an instantaneous invariant, and a
    // test that scans the queues at some later moment observes whatever the
    // scheduler happens to be doing then: the offending thread may already have
    // been popped and re-parked. That is not hypothetical -- the mutation that
    // deletes `schedule`'s parked branch *was* caught by a scanning test, and
    // silently stopped being caught when an unrelated fix changed the timing.
    // A check here cannot drift, because it runs on the push itself.
    //
    // Test-only: it costs a scheduler-lock acquisition on every context switch.
    // Taken *before* the run queue, which is the order the module documents --
    // SCHED may be taken with no queue held, and nothing takes SCHED while
    // holding one.
    #[cfg(test)]
    assert!(
        !SCHED.lock().threads.get(&ThreadId(raw)).is_some_and(|t| t.park.is_parked()),
        "cpu {cpu} is queueing parked ThreadId({raw}); it would be dispatched and resumed at a \
         suspension point it has not returned from"
    );
    percpu::run_queue().lock().push(ThreadId(raw), prio);
}

/// Rebuilds a [`Priority`] from the byte the handoff slot carries.
///
/// Split out so the round trip can be asserted. Decoding wrong is silent and
/// specifically dangerous in one direction: an idle thread promoted out of the
/// idle band becomes stealable, and an idle thread adopted the stack its own
/// CPU booted on.
fn priority_from_raw(raw: u8) -> Priority {
    match raw {
        0 => Priority::Idle,
        2 => Priority::High,
        _ => Priority::Normal,
    }
}

/// The core switch. `outgoing_state` is what the *calling* thread becomes.
fn schedule(outgoing_state: ThreadState) {
    // Interrupts off across the *whole* decision, not just while a lock is
    // held. The locks must be dropped before the switch (see the module docs),
    // and that window is not safe to be preempted in: by then this CPU's
    // `current` already names the incoming thread, which is not yet running, so
    // a tick landing here would save the outgoing thread's stack pointer into
    // the incoming thread's context and hand two threads the same stack.
    //
    // Restored after the switch returns -- which is when *this* thread is
    // scheduled again, using the flag state this thread saved. A thread
    // starting for the first time never reaches that restore, which is why
    // `thread_entry` enables interrupts itself.
    let irq = qunix_hal_x86_64::Irq::disable_and_save();

    let cpu = percpu::cpu_id();
    let current = ThreadId(percpu::current_thread());

    let Some(next) = take_next(cpu) else {
        // Nothing else to run. An exiting thread has no way forward, so it is
        // left to `exit_current` to panic; a yielding one simply carries on,
        // which is the right answer for an idle CPU.
        if outgoing_state != ThreadState::Exited {
            reap();
        }
        qunix_hal_x86_64::Irq::restore(irq);
        return;
    };

    // Raw pointers are copied out under the lock and used after it is dropped;
    // see the module docs on why the lock cannot span the switch.
    let (from_slot, to_ctx): (*mut *mut Context, *mut Context);
    let incoming_stack_top: u64;
    let incoming_root: Option<u64>;

    {
        let mut sched = SCHED.lock();

        match outgoing_state {
            ThreadState::Exited => {
                if let Some(t) = sched.threads.get_mut(&current) {
                    t.state = ThreadState::Exited;
                }
                sched.reapable.push(current);
                // An exiting thread is deliberately not handed on: there is
                // nothing to make runnable again.
                HANDOFF_ID[cpu as usize].store(NO_THREAD, Ordering::Relaxed);
            }
            _ => {
                // Whether this thread is parked decides whether it is handed
                // on, and it is asked *here*, under this acquisition of SCHED,
                // rather than trusted from the caller. `park` released the
                // lock between marking the thread and reaching this line, and
                // an `unpark` in that window clears the park without queueing
                // the thread -- it was still current. Handing it on anyway is
                // the safe direction: a spurious requeue costs one poll, and
                // the opposite mistake is a thread that never runs again.
                //
                // Asked for *every* outgoing state, not only `Blocked`. A
                // timer tick arrives with `Ready` and would otherwise requeue
                // a thread that is parked and merely waiting for its own
                // `hlt`, putting a parked thread in a run queue -- the one
                // thing this state exists to prevent, reached through the one
                // path that does not mention it.
                let parked = sched.threads.get(&current).is_some_and(|t| t.park.is_parked());
                let prio = sched.threads.get(&current).map_or(Priority::Normal, |t| t.priority);
                if let Some(t) = sched.threads.get_mut(&current) {
                    t.state = if parked { ThreadState::Blocked } else { ThreadState::Ready };
                }
                if parked {
                    // Deliberately not handed on: a parked thread must be in no
                    // run queue until something unparks it.
                    HANDOFF_ID[cpu as usize].store(NO_THREAD, Ordering::Relaxed);
                } else {
                    // Not pushed to the run queue here. See `HANDOFF_ID`:
                    // between the push and the switch storing this thread's
                    // stack pointer, another CPU could pop it and resume a
                    // context that does not exist yet.
                    HANDOFF_PRIORITY[cpu as usize].store(prio as u8, Ordering::Relaxed);
                    HANDOFF_ID[cpu as usize].store(current.0, Ordering::Relaxed);
                }
            }
        }

        let Some(next_thread) = sched.threads.get_mut(&next) else {
            // The queue named a thread the table does not have. `remove` exists
            // precisely so this cannot happen; if it does, the two have drifted
            // and dispatching would jump through a dangling context.
            panic!("run queue holds {next:?}, which is not a live thread");
        };
        next_thread.state = ThreadState::Running;
        incoming_stack_top = next_thread.kernel_stack_top;
        incoming_root = next_thread.address_space.as_ref().map(|s| s.root_frame());
        to_ctx = next_thread.context;
        // A running thread's context slot is null, and a thread is removed from
        // its run queue before dispatch, so a null here means two CPUs reached
        // the same thread -- the invariant this whole module is arranged
        // around, checked rather than assumed.
        assert!(!to_ctx.is_null(), "{next:?} has no saved context to resume");

        // SAFETY: this is the CPU doing the scheduling, interrupts are masked,
        // and the incoming thread was removed from every run queue above.
        unsafe { percpu::set_current_thread(next.0) };

        // The address of the outgoing thread's context slot. Taken as a raw
        // pointer so the borrow of the map ends with the guard.
        let outgoing = sched
            .threads
            .get_mut(&current)
            .expect("the running thread is not in the table");
        from_slot = &raw mut outgoing.context;
    }

    // The incoming thread's kernel stack is programmed *before* the switch, so
    // it is in place the moment that thread runs. This writes `TSS.rsp0` and
    // the syscall stub's slot in *this* CPU's block, reached through `GS`, so
    // it is already per-CPU-correct with more than one CPU scheduling. A thread
    // that adopted a boot stack reports 0 and is skipped: it never enters
    // ring 3, so nothing traps back onto a stack it would have to name, and
    // writing 0 into `TSS.rsp0` would point the next ring-3 trap at the null
    // page.
    if incoming_stack_top != 0 {
        // SAFETY: the value came from the incoming thread's own stack
        // allocation, which the scheduler's table keeps alive for as long as
        // the thread exists.
        unsafe { qunix_hal_x86_64::percpu::set_kernel_stack(incoming_stack_top) };
    }

    // The page tables the incoming thread expects. A CPU that ran a user thread
    // and then switched to a kernel one kept that process's root in CR3 -- the
    // kernel half is identical, so nothing notices, right up to the point the
    // process is reaped and its tables are freed underneath this CPU. Now that
    // a thread can move between CPUs, "the CPU it ran on will switch away" is
    // no longer something the exit path alone can guarantee, so every dispatch
    // states which root it wants.
    // SAFETY: the kernel half maps this code and this stack in every root the
    // kernel builds, which is what makes a CR3 write here survivable.
    unsafe { crate::vmspace::activate_root(incoming_root) };

    // Locks released. From here the outgoing thread stops running and does not
    // resume until something switches back to it.
    unsafe { context::switch(from_slot, to_ctx) };

    // Reached only when this thread is scheduled again, on whichever CPU
    // resumed it. Publishing that CPU's handoff is the first thing, because
    // until it happens the thread that gave the CPU up is in no run queue and
    // cannot be found by anyone.
    publish_handoff();
    qunix_hal_x86_64::Irq::restore(irq);
    // Whatever ran in between may have exited, so this is the natural place to
    // collect it.
    reap();
}

/// Frees the stacks and address spaces of threads that have exited.
///
/// Runs on a thread other than the one being freed, which is the entire reason
/// it is deferred rather than done in `exit_current`. That rule is what makes
/// dropping the address space safe as well as the stack: by the time another
/// thread reaps it, no CPU holds the dying tables in CR3 — the exiting thread
/// ran `vmspace::activate_kernel_root`, and every other CPU that ever ran that
/// thread reprogrammed CR3 when it dispatched something else.
fn reap() {
    // The whole `Thread` is dropped *after* the lock is released: freeing the
    // stack runs the heap allocator and freeing the address space runs the
    // frame allocator, each of which takes its own lock. Holding the scheduler
    // lock across either orders two locks in a way nothing else does.
    let mut corpses: alloc::vec::Vec<Thread> = alloc::vec::Vec::new();
    let mut orphans: alloc::vec::Vec<ThreadId> = alloc::vec::Vec::new();
    {
        let mut sched = SCHED.lock();
        let ids = core::mem::take(&mut sched.reapable);
        for id in ids {
            // Cannot free the stack a CPU is standing on, and cannot free an
            // address space a CPU may still be running on. With more than one
            // CPU that is not only *this* CPU's current thread: an exiting
            // thread queues itself here and then switches away, so it is
            // briefly still current somewhere.
            if is_current_anywhere(id) {
                orphans.push(id);
                continue;
            }
            if let Some(thread) = sched.threads.remove(&id) {
                corpses.push(thread);
            }
        }
        sched.reapable.append(&mut orphans);
    }
    drop(corpses);
}

/// Whether any CPU still names `id` as its running thread.
///
/// The single-CPU version of this was `id == sched.current`. With per-CPU
/// `current` slots that test would let one CPU free the stack another is
/// executing on, which is the exact failure the deferral exists to prevent.
fn is_current_anywhere(id: ThreadId) -> bool {
    // Every installed CPU, not only the online ones. A CPU is running its idle
    // thread from the moment `init` adopts it, which is before it is marked
    // online, and freeing a stack out from under it in that window is the same
    // corruption as doing it later.
    (0..MAX_CPUS).filter_map(percpu::current_thread_of).any(|running| running == id.0)
}

/// Runs the scheduler on this CPU for the rest of its life. Never returns.
///
/// Every CPU ends up here, the bootstrap processor included. A CPU with nothing
/// to run halts rather than spinning, and is woken by the IPI `spawn_kernel`
/// sends — there is no other source of new work, and no timer on a halted CPU
/// that has nothing queued.
pub fn idle_loop() -> ! {
    loop {
        // Counted before the decision, so a CPU that halts still records the
        // round it took to get there.
        //
        // This is the only witness the kernel has that a thread which never
        // yields lost its processor. A CPU running such a thread reaches this
        // loop again only by being preempted -- there is no other path back to
        // the idle thread -- so a count that moves while a spinner is resident
        // is proof of preemption, taken on the processor it happened on rather
        // than inferred from a global tick counter.
        IDLE_ROUNDS[percpu::cpu_id() as usize].fetch_add(1, Ordering::Relaxed);
        // Masked across the decision. Work queued between the check and the
        // halt would otherwise be missed forever: the wakeup IPI would arrive
        // while this CPU was still deciding, be dropped, and leave the CPU
        // halted with a full run queue.
        let _ = qunix_hal_x86_64::Irq::disable_and_save();
        if runnable_count() > 0 {
            x86_64::instructions::interrupts::enable();
            yield_now();
        } else {
            // `sti; hlt` has to be one instruction pair with nothing between.
            // `sti` does not take effect until after the *next* instruction, so
            // an interrupt that arrived while masked is delivered only once
            // `hlt` has been entered -- which is what wakes it. Splitting them
            // reopens the window this whole block exists to close.
            // SAFETY: no memory is touched and no stack slot is used.
            unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };
        }
    }
}

/// Room for one run queue per CPU is asserted, not assumed: a `cpu_id` past the
/// end would index out of bounds in `publish_handoff` on the first switch.
const _: () = assert!(HANDOFF_ID.len() == MAX_CPUS as usize);
const _: () = assert!(HANDOFF_PRIORITY.len() == MAX_CPUS as usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn the_handoff_priority_encoding_round_trips_every_band() {
        // The handoff slot carries a priority as a byte, and `publish_handoff`
        // rebuilds it. Getting that wrong is silent, and wrong in one direction
        // is dangerous rather than merely unfair: an idle thread decoded into
        // the Normal band becomes stealable, and an idle thread adopted the
        // stack its own CPU booted on.
        for band in [Priority::Idle, Priority::Normal, Priority::High] {
            assert_eq!(
                priority_from_raw(band as u8),
                band,
                "{band:?} did not survive the handoff encoding"
            );
        }
        // The negative direction: the fallback arm must not quietly turn an
        // unknown byte into the idle band, which is the one band that must
        // never be produced by accident.
        assert_ne!(priority_from_raw(9), Priority::Idle, "an unknown band decoded as Idle");
    }

    #[test_case]
    fn a_queue_holding_only_an_idle_thread_is_not_local_work() {
        // The distinction the dispatch order turns on, asserted in the
        // direction that was broken. A queue holding only this CPU's idle
        // thread is *not empty*, and a `take_next` that asked `is_empty` --
        // or equivalently popped unconditionally -- took that idle thread and
        // never reached the steal loop, so work queued on a CPU that stopped
        // scheduling starved forever.
        //
        // The integration test proves the consequence on four processors;
        // this pins the premise, and it is the half that a future
        // simplification would delete.
        let mut queue = qunix_sched::RunQueue::new();
        queue.push(ThreadId(1), Priority::Idle);
        assert!(!queue.is_empty(), "premise broken: a queued idle thread left the queue empty");
        assert!(
            !prefers_local(queue.runnable_len()),
            "a queue holding only an idle thread was treated as local work; this CPU would \
             run nothing while another CPU held runnable work it could have taken"
        );

        // And the positive direction, so a `prefers_local` hard-wired to false
        // -- which would make every CPU scan every other queue on every
        // dispatch and throw away cache locality -- does not pass.
        queue.push(ThreadId(2), Priority::Normal);
        assert!(
            prefers_local(queue.runnable_len()),
            "real local work was not treated as local work"
        );
    }
}
