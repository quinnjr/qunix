//! `SYSCALL`/`SYSRET` plumbing.
//!
//! `SYSCALL` is fast because it does almost nothing: it loads CS and SS from an
//! MSR, saves RIP in `rcx` and RFLAGS in `r11`, and jumps. It does **not**
//! switch stacks. So the first thing the entry stub must do is get off the user
//! stack, and it must do that without touching memory the user controls — which
//! is why the per-CPU block is reached through `gs:` and why the kernel stack
//! pointer lives at a fixed offset in it.
//!
//! # Register convention
//!
//! Deliberately Linux's, so the M3 personality layer needs no re-plumbing:
//! `rax` holds the syscall number, arguments arrive in `rdi`, `rsi`, `rdx`,
//! `r10`, `r8`, and the result goes back in `rax`. `r10` rather than `rcx` for
//! the fourth argument because `SYSCALL` clobbers `rcx` with the return
//! address.

use core::arch::naked_asm;

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// What the kernel does with a syscall once registers are in memory.
pub type SyscallHandler = extern "C" fn(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64;

/// Installed handler. Read by the entry stub's Rust half.
///
/// A plain static rather than per-CPU: the dispatch table is the same on every
/// CPU, and making it per-CPU would mean an AP could service a syscall the BSP
/// cannot.
static HANDLER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Enables `SYSCALL`/`SYSRET` on this CPU and installs `handler`.
///
/// Must be called on every CPU: the MSRs written here are per-CPU, so an AP
/// that skips this raises #UD on the first `syscall` instruction rather than
/// entering the kernel.
///
/// # Safety
/// This CPU's per-CPU block must be installed (the stub reads `gs:`), and the
/// GDT must carry user segments laid out as `SYSRET` requires — see
/// [`crate::gdt::build_and_load`].
pub unsafe fn init(handler: SyscallHandler) {
    HANDLER.store(handler as *const () as usize, core::sync::atomic::Ordering::Release);

    let sel = crate::percpu::current().selectors();
    // STAR[47:32] is the kernel CS for SYSCALL; STAR[63:48] is the *base* from
    // which SYSRET computes user SS (base + 8) and user CS (base + 16). The
    // base is therefore the user data selector minus nothing -- the GDT layout
    // is what makes the arithmetic work, and `user_data` sits immediately
    // before `user_code` for exactly this reason.
    let star = ((sel.user_data.0 as u64 - 8) << 48) | ((sel.kernel_code.0 as u64) << 32);

    unsafe {
        // EFER.SCE — without it `syscall` is an invalid opcode.
        let efer = read_msr(IA32_EFER);
        write_msr(IA32_EFER, efer | 1);
        write_msr(IA32_STAR, star);
        write_msr(IA32_LSTAR, (syscall_entry as *const ()) as u64);
        // Cleared on entry. IF above all: the stub runs on a kernel stack it has
        // not yet finished switching to, and an interrupt landing between the
        // `swapgs` and the stack switch would push a frame onto the *user*
        // stack. DF is cleared because the SysV ABI requires it forward and
        // user code is free to have set it.
        write_msr(IA32_FMASK, 0x0000_0700 | (1 << 9) | (1 << 10));
    }
}

/// # Safety
/// `msr` must be readable.
unsafe fn read_msr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        )
    };
    ((hi as u64) << 32) | lo as u64
}

/// # Safety
/// `msr` must be writable and `value` valid for it.
unsafe fn write_msr(msr: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        )
    };
}

/// Where `SYSCALL` lands.
///
/// Runs on the *user* stack with interrupts masked. Every instruction before
/// the stack switch is chosen so it touches no memory the user controls.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        // GS currently holds whatever the user set. Swap in the kernel's.
        "swapgs",
        // Park the user stack pointer in the per-CPU block and adopt the
        // kernel's. Both through `gs:`, because there is nowhere else to put a
        // value at this point -- no stack, and every register is either an
        // argument or holds state SYSRET needs.
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kernel_rsp}]",

        // From here a kernel stack exists and ordinary pushes are safe.
        // rcx and r11 carry the return address and flags SYSRET consumes.
        "push rcx",
        "push r11",

        // Translate the syscall register convention into System V's. They are
        // not the same and do not overlap conveniently:
        //
        //   syscall:  nr=rax  a0=rdi  a1=rsi  a2=rdx  a3=r10  a4=r8
        //   sysv:     nr=rdi  a0=rsi  a1=rdx  a2=rcx  a3=r8   a4=r9
        //
        // So every argument shifts by one register. Written right-to-left so
        // each source is read before it is overwritten -- doing it in the other
        // order passes the syscall number as its own first argument, which is
        // what an earlier version of this stub did: it delivered `nr = rdi`,
        // and the process's `exit` was dispatched as whatever its first
        // argument happened to be.
        "mov r9, r8",
        "mov r8, r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call {dispatch}",

        "pop r11",
        "pop rcx",

        // Scrub the caller-saved registers `dispatch` was free to leave kernel
        // values in. Ring 3 takes this path on every syscall, and after
        // `sys_write` these hold kernel heap and HHDM addresses -- handing them
        // back is the same leak `enter_user` clears its 15 registers to avoid,
        // on the path that actually runs more than once.
        //
        // The exclusions are deliberate, and each is load-bearing:
        //   rax           -- the syscall's return value; clearing it returns 0
        //                    from every call.
        //   rcx, r11      -- consumed by `sysretq` as the user RIP and RFLAGS.
        //   rbx, rbp,     -- callee-saved, so they still hold the *user's* own
        //   r12-r15          values, restored by `dispatch`'s epilogue. The
        //                    process would see its own registers destroyed.
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",

        // Restore the user stack, put GS back, and return to ring 3. `sysretq`
        // (not `sysret`) for a 64-bit return; the 32-bit form drops the high
        // half of RIP.
        "mov rsp, gs:[{user_rsp}]",
        "swapgs",
        "sysretq",
        user_rsp = const crate::percpu::OFFSET_USER_RSP,
        kernel_rsp = const crate::percpu::OFFSET_KERNEL_RSP,
        dispatch = sym dispatch,
    )
}

/// Rust half of the entry path: arguments are already in SysV registers.
///
/// Returns the value the stub leaves in `rax`.
extern "C" fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let raw = HANDLER.load(core::sync::atomic::Ordering::Acquire);
    if raw == 0 {
        // `syscall` reached the kernel before anything installed a handler.
        // Returning an error rather than panicking: a userspace process making
        // a syscall too early is a userspace problem, and panicking here kills
        // the machine for it.
        return qunix_abi::Errno::BadSyscall as i64;
    }
    // SAFETY: `HANDLER` only ever holds a `SyscallHandler` stored by `init`.
    let handler: SyscallHandler = unsafe { core::mem::transmute::<usize, SyscallHandler>(raw) };
    handler(nr, a0, a1, a2, a3, a4)
}

/// Enters ring 3 at `entry` with `user_stack`, and does not come back.
///
/// Uses `iretq` rather than `sysretq` because this is not a return *from* a
/// syscall: there is no saved `rcx`/`r11` to restore, and `iretq` takes
/// everything it needs from the stack, including the ring change.
///
/// # Safety
/// `entry` and `user_stack` must be mapped user-accessible in the currently
/// active address space, and this CPU's `kernel_rsp` must already point at a
/// stack the syscall stub can use — otherwise the first syscall from the new
/// process lands on a null stack.
pub unsafe fn enter_user(entry: u64, user_stack: u64) -> ! {
    let sel = crate::percpu::current().selectors();
    // RPL 3 on both: the selector's low two bits are the requested privilege
    // level, and `iretq` uses CS's RPL to decide whether this is a ring change.
    let cs = sel.user_code.0 as u64 | 3;
    let ss = sel.user_data.0 as u64 | 3;
    // IF set, bit 1 always set. Entering ring 3 with interrupts masked leaves a
    // process that cannot be preempted and a machine that cannot be recovered.
    let rflags: u64 = 0x202;

    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            // Every general-purpose register is cleared before the ring change.
            // Whatever the kernel last left in them is otherwise visible to the
            // process on its first instruction, and this path runs immediately
            // after page-table construction, so those values are kernel heap
            // and physical-frame addresses.
            //
            // This block overwrites whichever registers the allocator picked
            // for the operands, and zeroes `rbp`, which is reserved and cannot
            // be an operand at all. Both are sound only because of
            // `options(noreturn)`: there is no exit from this block, so there
            // is no point at which LLVM could observe a clobbered input or need
            // a frame pointer restored.
            //
            // Declaring the operands `inout(reg) _ => _` to make the clobber
            // explicit does not compile -- `noreturn` forbids outputs, for the
            // same reason it makes the clobber harmless. So the reasoning has
            // to live here rather than in the operand list.
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor edi, edi",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            "iretq",
            ss = in(reg) ss,
            rsp = in(reg) user_stack,
            rflags = in(reg) rflags,
            cs = in(reg) cs,
            rip = in(reg) entry,
            options(noreturn),
        )
    }
}

/// Records the stack the kernel switches to when it is entered from ring 3.
///
/// Re-exported from [`crate::percpu::set_kernel_stack`], which sets both the
/// syscall stub's slot and the TSS entry the CPU uses for interrupts.
///
/// # Safety
/// As [`crate::percpu::set_kernel_stack`].
pub unsafe fn set_kernel_stack(stack_top: u64) {
    unsafe { crate::percpu::set_kernel_stack(stack_top) };
}
