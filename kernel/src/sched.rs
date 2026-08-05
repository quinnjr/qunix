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
//! There is a single global run queue and one lock. Task 6 makes the queue
//! per-CPU; until then a global one is honest about the fact that only the BSP
//! is running, and a per-CPU queue with one CPU in it would be untested
//! scaffolding.

use alloc::collections::BTreeMap;
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

static INITIALISED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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

/// Creates a runnable kernel thread.
pub fn spawn_kernel(entry: extern "C" fn(u64) -> !, arg: u64, prio: Priority) -> ThreadId {
    let mut sched = SCHED.lock();
    let id = sched.allocate_id();
    let thread = Thread::new_kernel(entry, arg, prio);
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
    // Raw pointers are copied out under the lock and used after it is dropped;
    // see the module docs on why the lock cannot span the switch.
    let (from_slot, to_ctx): (*mut *mut Context, *mut Context);

    {
        let mut sched = SCHED.lock();
        let current = sched.current;

        let Some(next) = sched.queue.pop() else {
            // Nothing else to run. An exiting thread has no way forward, so it
            // is left to `exit_current` to panic; a yielding one simply carries
            // on, which is the right answer for the boot thread.
            if outgoing_state == ThreadState::Exited {
                return;
            }
            drop(sched);
            reap();
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

    // Lock released. From here the outgoing thread stops running and does not
    // resume until something switches back to it.
    unsafe { context::switch(from_slot, to_ctx) };

    // Reached only when this thread is scheduled again. Whatever ran in between
    // may have exited, so this is the natural place to collect it.
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
