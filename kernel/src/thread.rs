//! Threads: a stack, a saved context, and a scheduling state.
//!
//! A thread is the scheduling unit. It owns its kernel stack, and the stack is
//! what makes ownership matter: the thread's saved [`Context`] lives *on* that
//! stack, so freeing the stack while the thread is merely stopped — not just
//! while it runs — hands its saved registers to whoever allocates next.
//!
//! Threads are kept alive by the scheduler's table rather than by whoever
//! spawned them, because the spawner usually has no reason to outlive the
//! thread it started.

use alloc::boxed::Box;
use qunix_hal_x86_64::context::{self, Context};
use qunix_sched::Priority;

/// Where a thread is in its life.
///
/// `Exited` is a state rather than a removal because the thread that exits is
/// running on the very stack that would have to be freed. It marks itself and
/// the *next* thread reclaims it — see `sched::reap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Running,
    Exited,
}

pub struct Thread {
    /// Where this thread resumes. Null while it is the running thread — the
    /// value is only meaningful once `switch` has stored it.
    pub context: *mut Context,
    /// The thread's kernel stack. `Option` so `reap` can drop it without
    /// dropping the bookkeeping that says the thread is gone.
    pub stack: Option<Box<[u8]>>,
    /// Top of this thread's kernel stack, or 0 for a thread that adopted the
    /// stack the CPU booted on.
    ///
    /// The scheduler programs this into `TSS.rsp0` and the syscall stub's slot
    /// on every switch. It has to be per-thread: a single value set once means
    /// the second user thread to enter ring 3 traps onto the *first* thread's
    /// stack, and both then scribble through each other's kernel frames.
    pub kernel_stack_top: u64,
    pub state: ThreadState,
    pub priority: Priority,
}

// SAFETY: `context` points into `stack`, which this `Thread` owns, so the
// pointer and the memory it names move together. Sending a `Thread` to another
// CPU is what a scheduler *does* -- a thread that stops on one CPU and resumes
// on another is the normal case, not an exceptional one.
//
// What this does NOT permit is two CPUs resuming the same context, which would
// hand them the same stack. That is prevented by the scheduler removing a
// thread from the run queue before dispatching it, not by this impl.
unsafe impl Send for Thread {}

impl Thread {
    /// Allocates a stack and prepares it so the first switch enters
    /// `entry(arg)`.
    pub fn new_kernel(
        entry: extern "C" fn(u64) -> !,
        arg: u64,
        priority: Priority,
    ) -> Self {
        // `vec![0; n]` rather than an uninitialised buffer: a fresh stack full
        // of whatever the heap last held makes a backtrace from this thread
        // walk into stale frames, and this kernel's panic path does exactly
        // that walk.
        let mut stack = alloc::vec![0u8; context::MIN_STACK].into_boxed_slice();
        let base = stack.as_mut_ptr() as u64;
        // Rounded *down*, so the context is never placed above the allocation.
        let stack_top = (base + context::MIN_STACK as u64) & !0xf;

        // SAFETY: the stack is owned by this `Thread`, is mapped and writable,
        // and `stack_top` lies inside it and is 16-aligned.
        let ctx = unsafe { context::init_kernel_stack(stack_top, entry, arg) };

        Self {
            context: ctx,
            stack: Some(stack),
            kernel_stack_top: stack_top,
            state: ThreadState::Ready,
            priority,
        }
    }

    /// The idle thread for a CPU: no stack of its own is prepared because it
    /// adopts the stack the CPU booted on.
    ///
    /// Its context is filled in by the first `switch` away from it, which is
    /// why it starts null.
    pub fn adopt_current() -> Self {
        Self {
            context: core::ptr::null_mut(),
            stack: None,
            kernel_stack_top: 0,
            state: ThreadState::Running,
            priority: Priority::Idle,
        }
    }
}
