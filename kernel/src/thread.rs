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

use crate::vmspace::VmSpace;

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
    /// The thread's kernel stack.
    ///
    /// Never read after construction: it exists to be *owned*, so that the
    /// allocation lives exactly as long as the `Thread` and is released when
    /// `sched::reap` drops it — on a different thread, because a thread cannot
    /// free the stack it is standing on. `kernel_stack_top` below is the value
    /// anything actually uses.
    ///
    /// `Option` because a thread that adopted the stack its CPU booted on owns
    /// no allocation to release.
    #[allow(dead_code)]
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
    /// The address space this thread entered ring 3 on, for a user thread.
    ///
    /// Owned here because `syscall::enter_user` never returns: the frame that
    /// built the `VmSpace` cannot own it to the end, and dropping it there
    /// would free the page tables the process is about to execute on. Without
    /// an owner that outlives the switch to ring 3, the PML4, every
    /// intermediate table and every user page leak for the rest of the boot —
    /// on every process death, since exit is reachable both through `Sys::Exit`
    /// and through the ring-3 fault path.
    ///
    /// `Option` for the same reason as `stack`: `sched::reap` takes it out and
    /// drops it from a *different* thread, once this one has stopped and the
    /// CPU that ran it has switched back to the kernel root.
    pub address_space: Option<VmSpace>,
}

// SAFETY: `context` points into `stack`, which this `Thread` owns, so the
// pointer and the memory it names move together. `address_space` owns its
// frames outright and names them by physical address, so it too travels with
// the `Thread`. Sending a `Thread` to another CPU is what a scheduler *does* --
// a thread that stops on one CPU and resumes on another is the normal case, not
// an exceptional one.
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
            // Filled in by `sched::adopt_address_space` if this thread turns
            // out to be a user thread; a kernel thread never has one.
            address_space: None,
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
            address_space: None,
        }
    }
}
