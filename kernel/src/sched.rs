//! Kernel-side scheduler: the run queue, the thread table, and `schedule()`.
//!
//! The *policy* — which thread runs next — lives in `qunix-sched` and is
//! host-tested. This module is everything that policy deliberately knows
//! nothing about: real stacks, the context switch, and the locking.
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
//! # One CPU for now
//!
//! There is a single global run queue and one lock, because only the bootstrap
//! processor schedules. Application processors come online and park: `current`
//! below is one field shared by every CPU, so two CPUs scheduling through it
//! would have one save its stack pointer into the other's context. Moving
//! `current` into `percpu::PerCpu` is what makes them schedulable; see
//! Execution Deviation D3 in the M1 plan.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, Ordering};
use qunix_sync::IrqControl;
use qunix_hal_x86_64::context::{self, Context};
use qunix_sched::{Priority, RunQueue, ThreadId};
use qunix_sync::IrqSpinLock;

use crate::thread::{Thread, ThreadState};

/// Everything the scheduler mutates, under one lock.
struct Scheduler {
    queue: RunQueue,
    threads: BTreeMap<ThreadId, Thread>,
    current: ThreadId,
    next_id: u64,
    /// Threads that have exited and whose stacks are waiting to be freed.
    ///
    /// A thread cannot free its own stack: it is standing on it. So `exit`
    /// records the id here and the next thread to run does the freeing.
    reapable: alloc::vec::Vec<ThreadId>,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            queue: RunQueue::new(),
            threads: BTreeMap::new(),
            current: ThreadId(0),
            next_id: 1,
            reapable: alloc::vec::Vec::new(),
        }
    }

    fn allocate_id(&mut self) -> ThreadId {
        let id = ThreadId(self.next_id);
        self.next_id += 1;
        id
    }
}

/// `IrqSpinLock`, not `SpinLock`: the timer interrupt calls into the scheduler,
/// so a plain spinlock would let a tick land on a CPU that already holds it and
/// spin forever against itself.
static SCHED: IrqSpinLock<Scheduler, qunix_hal_x86_64::Irq> = IrqSpinLock::new(Scheduler::new());

static INITIALISED: AtomicBool = AtomicBool::new(false);

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
/// interrupt context, must do nothing at all when the scheduler is not ready,
/// and must never panic -- a panic here fires on every subsequent tick.
///
/// The caller must have signalled EOI already. Switching first would leave the
/// LAPIC waiting for an EOI that only arrives when this thread is scheduled
/// again, so the CPU would take no further timer interrupts in the meantime.
pub fn preempt() {
    if !PREEMPT.load(Ordering::Acquire) || !INITIALISED.load(Ordering::Acquire) {
        return;
    }
    schedule(ThreadState::Ready);
}

/// Adopts the currently-executing context as thread 0.
///
/// The boot path is already a thread in everything but name: it has a stack and
/// a register state. Rather than construct a fake one, it is adopted, so the
/// first `schedule` has somewhere to save its context to.
pub fn init() {
    let mut sched = SCHED.lock();
    if INITIALISED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        // The in-QEMU harness runs several tests in one boot; re-initialising
        // would orphan every thread the previous test spawned while leaving
        // their stacks allocated.
        return;
    }
    let boot = ThreadId(0);
    sched.threads.insert(boot, Thread::adopt_current());
    sched.current = boot;
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
    let start = unsafe { alloc::boxed::Box::from_raw(raw as *mut ThreadStart) };
    let ThreadStart { entry, arg } = *start;
    x86_64::instructions::interrupts::enable();
    entry(arg)
}

/// Creates a runnable kernel thread.
pub fn spawn_kernel(entry: extern "C" fn(u64) -> !, arg: u64, prio: Priority) -> ThreadId {
    let start = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(ThreadStart { entry, arg }));
    let mut sched = SCHED.lock();
    let id = sched.allocate_id();
    let thread = Thread::new_kernel(thread_entry, start as u64, prio);
    sched.threads.insert(id, thread);
    sched.queue.push(id, prio);
    id
}

pub fn current_id() -> ThreadId {
    SCHED.lock().current
}

/// Live threads, including the running one and any awaiting reaping.
pub fn thread_count() -> usize {
    SCHED.lock().threads.len()
}

pub fn runnable_count() -> usize {
    SCHED.lock().queue.len()
}

/// Yields the CPU to the next runnable thread, if there is one.
///
/// Returns without switching when nothing else is runnable — that is the
/// common case for the boot thread and is not an error.
pub fn yield_now() {
    schedule(ThreadState::Ready);
}

/// Terminates the calling thread. Never returns.
///
/// The stack is *not* freed here: this code is running on it. The thread is
/// marked `Exited` and its id queued for the next thread to reap.
pub fn exit_current() -> ! {
    schedule(ThreadState::Exited);
    // `schedule` only returns when it did not switch away, which for an exiting
    // thread means there was nothing to switch to. There is no correct
    // behaviour left: the caller cannot return, and continuing would run an
    // exited thread.
    panic!("the last thread exited with nothing else to run");
}

/// The core switch. `outgoing_state` is what the *calling* thread becomes.
fn schedule(outgoing_state: ThreadState) {
    // Interrupts off across the *whole* decision, not just while the lock is
    // held. The lock must be dropped before the switch (see the module docs),
    // and that window is not safe to be preempted in: by then `sched.current`
    // already names the incoming thread, which is not yet running, so a tick
    // landing here would save the outgoing thread's stack pointer into the
    // incoming thread's context and hand two threads the same stack.
    //
    // Restored after the switch returns -- which is when *this* thread is
    // scheduled again, using the flag state this thread saved. A thread
    // starting for the first time never reaches that restore, which is why
    // `thread_entry` enables interrupts itself.
    let irq = qunix_hal_x86_64::Irq::disable_and_save();

    // Raw pointers are copied out under the lock and used after it is dropped;
    // see the module docs on why the lock cannot span the switch.
    let (from_slot, to_ctx): (*mut *mut Context, *mut Context);
    let incoming_stack_top: u64;

    {
        let mut sched = SCHED.lock();
        let current = sched.current;

        let Some(next) = sched.queue.pop() else {
            // Nothing else to run. An exiting thread has no way forward, so it
            // is left to `exit_current` to panic; a yielding one simply carries
            // on, which is the right answer for the boot thread.
            // The guard is dropped *before* the flag is restored, on both
            // paths. Restoring first re-enables interrupts while `SCHED` is
            // still held on this CPU, and the guard's own restore cannot undo
            // it -- `IrqSpinLock::lock` captured `was_enabled = false`, because
            // `schedule` had already masked. A tick landing in that window
            // re-enters `preempt` -> `schedule` -> `SCHED.lock()` and spins
            // against a lock this CPU owns, which is exactly the self-deadlock
            // the `IrqSpinLock` choice is documented to prevent.
            drop(sched);
            if outgoing_state != ThreadState::Exited {
                reap();
            }
            qunix_hal_x86_64::Irq::restore(irq);
            return;
        };

        // Requeue the outgoing thread *before* the switch, so it is visible to
        // whoever schedules next. An exiting thread is deliberately not
        // requeued -- that is the whole difference between the two states.
        match outgoing_state {
            ThreadState::Exited => {
                if let Some(t) = sched.threads.get_mut(&current) {
                    t.state = ThreadState::Exited;
                }
                sched.reapable.push(current);
            }
            _ => {
                let prio = sched.threads.get(&current).map_or(Priority::Normal, |t| t.priority);
                if let Some(t) = sched.threads.get_mut(&current) {
                    t.state = ThreadState::Ready;
                }
                sched.queue.push(current, prio);
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
        to_ctx = next_thread.context;
        assert!(!to_ctx.is_null(), "{next:?} has no saved context to resume");

        sched.current = next;
        // The address of the outgoing thread's context slot. Taken as a raw
        // pointer so the borrow of the map ends with the guard.
        let outgoing = sched
            .threads
            .get_mut(&current)
            .expect("the running thread is not in the table");
        from_slot = &raw mut outgoing.context;
    }

    // The incoming thread's kernel stack is programmed *before* the switch, so
    // it is in place the moment that thread runs. A thread that adopted the
    // boot stack reports 0 and is skipped: it never enters ring 3, so nothing
    // traps back onto a stack it would have to name, and writing 0 into
    // `TSS.rsp0` would point the next ring-3 trap at the null page.
    if incoming_stack_top != 0 {
        // SAFETY: the value came from the incoming thread's own stack
        // allocation, which the scheduler's table keeps alive for as long as
        // the thread exists.
        unsafe { qunix_hal_x86_64::percpu::set_kernel_stack(incoming_stack_top) };
    }

    // Lock released. From here the outgoing thread stops running and does not
    // resume until something switches back to it.
    unsafe { context::switch(from_slot, to_ctx) };

    // Reached only when this thread is scheduled again.
    qunix_hal_x86_64::Irq::restore(irq);
    // Whatever ran in between may have exited, so this is the natural place to
    // collect it.
    reap();
}

/// Frees the stacks of threads that have exited.
///
/// Runs on a thread other than the one being freed, which is the entire reason
/// it is deferred rather than done in `exit_current`.
fn reap() {
    // The stacks are dropped *after* the lock is released: freeing runs the
    // heap allocator, which takes its own lock, and holding the scheduler lock
    // across that orders two locks in a way nothing else does.
    let mut corpses = alloc::vec::Vec::new();
    {
        let mut sched = SCHED.lock();
        let current = sched.current;
        let ids = core::mem::take(&mut sched.reapable);
        for id in ids {
            if id == current {
                // Cannot free the stack we are standing on. Put it back for
                // whoever runs next.
                sched.reapable.push(id);
                continue;
            }
            if let Some(mut thread) = sched.threads.remove(&id) {
                sched.queue.remove(id);
                corpses.push(thread.stack.take());
            }
        }
    }
    drop(corpses);
}
