//! Kernel-thread context switch.
//!
//! A switch here saves only what the System V ABI makes the *callee's*
//! responsibility — `rbx`, `rbp`, `r12`-`r15` — plus the return address. The
//! caller-saved registers need no saving because `switch` is an ordinary
//! `extern "C"` call: the compiler has already spilled anything live across it.
//! That is the whole reason this is seven words rather than a full trap frame.
//!
//! The context is not a separate allocation. It is written onto the outgoing
//! thread's own stack, and `*mut Context` *is* that thread's stack pointer at
//! the moment it stopped running. Resuming a thread is therefore `mov rsp, ...`
//! followed by pops — there is nothing to copy.

use core::arch::naked_asm;

/// A stopped thread's callee-saved state.
///
/// `#[repr(C)]` and the field order are load-bearing: the field at the lowest
/// address is popped first, so this must be listed in the order
/// [`switch`] pops. Reordering the fields silently restores registers into the
/// wrong places, which presents as corruption in whatever ran next rather than
/// as a fault here.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Context {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    /// Where the thread resumes. `switch`'s final `ret` consumes this.
    pub rip: u64,
}

/// Switches from the current thread to `to`.
///
/// Writes the outgoing thread's stack pointer through `from`, so the thread
/// being left behind records where to resume without the caller doing anything.
/// A thread that is never switched back to simply leaves that slot stale, which
/// is why `from` is a pointer-to-pointer rather than a return value: there is no
/// return to carry it on, the function does not come back until someone
/// switches *to* this thread.
///
/// # Safety
/// `to` must be a context prepared by [`init_kernel_stack`] or written by an
/// earlier `switch`, whose stack is mapped and owned by the thread being
/// resumed. `from` must be a writable slot that outlives the switch. Resuming a
/// context twice concurrently, or one whose stack has been freed, hands two
/// threads the same stack.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(from: *mut *mut Context, to: *mut Context) {
    naked_asm!(
        // Pushed in reverse of the field order, so the resulting block on the
        // stack reads as a `Context` from low address to high.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // *from = rsp -- the outgoing thread's context is now its stack.
        "mov [rdi], rsp",
        // Adopt the incoming thread's stack. Everything below is running on it.
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        // Consumes `rip`. For a thread resuming an earlier switch this returns
        // into `switch`'s caller; for a fresh one it enters `trampoline`.
        "ret",
    )
}

/// First instructions of a brand-new thread.
///
/// A fresh thread cannot simply `ret` into its entry point, because the entry
/// point needs its argument in `rdi` and there is no frame to supply it.
/// [`init_kernel_stack`] parks the entry point in `r15` and the argument in
/// `r14` — both callee-saved, so `switch`'s pops deliver them — and this moves
/// them into place.
///
/// `ud2` rather than a return: `entry` is `-> !`, so reaching the instruction
/// after the call means the type system was lied to, and faulting immediately
/// is far easier to diagnose than returning to a stack slot that was never a
/// return address.
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    naked_asm!(
        "mov rdi, r14",
        "call r15",
        "ud2",
    )
}

/// Prepares a fresh kernel stack so the first [`switch`] to it enters
/// `entry(arg)`.
///
/// Returns the context pointer to hand to `switch`.
///
/// # Safety
/// `stack_top` must be the exclusive upper bound of a mapped, writable region
/// owned by the new thread, with at least [`MIN_STACK`] bytes below it, and
/// must be 16-byte aligned. The region must outlive the thread — it is the
/// thread's stack, and freeing it while the thread is live or merely stopped
/// hands its saved context to whoever allocates next.
pub unsafe fn init_kernel_stack(
    stack_top: u64,
    entry: extern "C" fn(u64) -> !,
    arg: u64,
) -> *mut Context {
    assert!(stack_top.is_multiple_of(16), "kernel stack top {stack_top:#x} is not 16-aligned");

    // System V requires `rsp` to be 16-byte aligned at a `call`. `switch`'s
    // `ret` leaves `rsp` just past the context, so placing the context
    // immediately below a 16-aligned `stack_top` puts the trampoline's `call`
    // at exactly that alignment. Getting this wrong does not fault here; it
    // faults later, inside whatever callee first uses an aligned SSE move.
    let ctx_addr = stack_top - core::mem::size_of::<Context>() as u64;
    let ctx = ctx_addr as *mut Context;

    unsafe {
        ctx.write(Context {
            // Delivered to the trampoline by `switch`'s pops.
            r15: (entry as *const ()) as u64,
            r14: arg,
            r13: 0,
            r12: 0,
            rbx: 0,
            // Zero rather than `stack_top`: this terminates a frame-pointer
            // walk, so a backtrace from inside the new thread stops here
            // instead of wandering into whatever the stack memory used to hold.
            rbp: 0,
            rip: (trampoline as *const ()) as u64,
        });
    }
    ctx
}

/// Smallest kernel stack that leaves room for the context plus a fault.
///
/// Not a tuned number: it is the smallest size at which a stack overflow is
/// still likely to be caught by the guard-page-less `#DF` path rather than
/// silently scribbling. Stacks here have no guard page — see CLAUDE.md.
pub const MIN_STACK: usize = 4096 * 4;

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn context_field_order_matches_the_pop_sequence() {
        // `switch` pops r15 first, so it must sit at offset 0. A reordering
        // here restores registers into the wrong places, which shows up as
        // corruption in an unrelated thread rather than as a fault.
        assert_eq!(core::mem::offset_of!(Context, r15), 0);
        assert_eq!(core::mem::offset_of!(Context, r14), 8);
        assert_eq!(core::mem::offset_of!(Context, r13), 16);
        assert_eq!(core::mem::offset_of!(Context, r12), 24);
        assert_eq!(core::mem::offset_of!(Context, rbx), 32);
        assert_eq!(core::mem::offset_of!(Context, rbp), 40);
        assert_eq!(core::mem::offset_of!(Context, rip), 48);
        assert_eq!(core::mem::size_of::<Context>(), 56);
    }

    extern "C" fn never_called(_: u64) -> ! {
        unreachable!()
    }

    #[test]
    fn init_kernel_stack_leaves_the_trampoline_call_16_byte_aligned() {
        // The alignment System V requires at a `call`. Being wrong here does
        // not fault in `switch`; it faults much later, inside the first callee
        // that uses an aligned SSE move.
        //
        // Calls the real function rather than recomputing its arithmetic --
        // otherwise the test passes for any implementation, including none.
        let mut stack = alloc::vec![0u64; 512];
        let stack_top = (stack.as_mut_ptr() as u64 + 512 * 8) & !0xf;
        let ctx = unsafe { init_kernel_stack(stack_top, never_called, 7) };
        let resume_rsp = ctx as u64 + core::mem::size_of::<Context>() as u64;
        assert_eq!(resume_rsp % 16, 0, "trampoline would call with rsp = {resume_rsp:#x}");
        assert!(resume_rsp <= stack_top, "the context was placed above the stack top");
    }

    #[test]
    fn init_kernel_stack_delivers_the_entry_point_and_argument() {
        let mut stack = alloc::vec![0u64; 512];
        let stack_top = (stack.as_mut_ptr() as u64 + 512 * 8) & !0xf;
        let ctx = unsafe { init_kernel_stack(stack_top, never_called, 0xABCD) };
        let ctx = unsafe { &*ctx };
        // The trampoline reads these two registers; a swap would call the
        // argument as a function.
        assert_eq!(ctx.r15, never_called as *const () as u64, "entry not in r15");
        assert_eq!(ctx.r14, 0xABCD, "argument not in r14");
        assert_eq!(ctx.rbp, 0, "a non-zero rbp lets a backtrace walk off the stack");
    }

    #[test]
    #[should_panic(expected = "not 16-aligned")]
    fn init_kernel_stack_refuses_a_misaligned_stack_top() {
        // The negative direction. Accepting one produces a fault in an
        // unrelated callee much later, which is near-impossible to trace back.
        let mut stack = alloc::vec![0u64; 512];
        let stack_top = (stack.as_mut_ptr() as u64 + 512 * 8) & !0xf;
        unsafe { init_kernel_stack(stack_top + 8, never_called, 0) };
    }
}
