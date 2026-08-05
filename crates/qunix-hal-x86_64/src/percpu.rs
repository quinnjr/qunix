//! Per-CPU state, reached through `GS`.
//!
//! Replaces M0's `static mut` GDT, TSS, IDT and double-fault stack. Those were
//! sound only because exactly one CPU existed; with application processors the
//! TSS in particular cannot be shared, since each CPU needs its own kernel
//! stack pointer and its own IST entries.
//!
//! # Why the BSP's block is static and the APs' are boxed
//!
//! The plan called for `install(cpu_id)` to `Box` the block. That cannot work
//! for the bootstrap processor: `gdt::init` runs before `frames::init` and
//! `heap::init` in `kmain`, and it has to — a CPU with no IDT triple-faults on
//! the first fault instead of printing a diagnostic, so the tables must exist
//! before the allocators run, not after. The BSP therefore uses a single
//! statically reserved block, and only APs (which start long after the heap is
//! up) allocate. Linux does the same thing for the same reason.
//!
//! # Fixed offsets
//!
//! Three fields are reached through `gs:[N]` rather than through a `&PerCpu`.
//! `kernel_rsp` and `user_rsp` by the `SYSCALL` entry stub, which runs before
//! any Rust and before a stack exists; `self_ptr` by `self_ptr()` below, which
//! is ordinary Rust but is how a `&PerCpu` is produced at all -- `gs:`
//! addressing can read through the base but cannot yield it. `cpu_id` and
//! `current_thread` are pinned alongside them so the scheduler can reach them
//! the same way without a later reshuffle.
//!
//! `OFFSET_*` and the `const` assertions are what keep the assembly and this
//! struct in agreement; reordering the fields would otherwise break the stub
//! silently, so it is a compile error instead.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use x86_64::structures::gdt::GlobalDescriptorTable;
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::structures::tss::TaskStateSegment;

use crate::gdt::{self, Selectors};

/// `IA32_GS_BASE` — the base `gs:` addressing adds while in kernel mode.
const IA32_GS_BASE: u32 = 0xC000_0101;
/// `IA32_KERNEL_GS_BASE` — the value `swapgs` exchanges into `GS_BASE`.
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// Double-fault stack size, now per CPU rather than shared.
///
/// The handler's peak depth is one `println!` — the 32 backtrace lines are
/// sequential, not nested — so a few KiB suffices in principle. 16 KiB keeps a
/// ~5x margin anyway, because this stack has no guard page and overflow grows
/// *down* into whatever precedes it, silently corrupting the very structures
/// fault handling depends on.
pub const IST_STACK_SIZE: usize = 4096 * 4;

/// Written at the low end of the stack and checked on entry, so an overflow is
/// reported rather than silently scribbling.
const IST_CANARY: u64 = 0x5153_5441_434B_5F30;

/// The CPU aligns RSP to 16 bytes on an IST stack switch, so an align-1 array
/// merely wastes the slack.
#[repr(C, align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

/// Per-CPU block. `#[repr(C)]` is load-bearing: see the offset assertions.
#[repr(C)]
pub struct PerCpu {
    /// offset 0x00 — stack the `SYSCALL` stub switches to.
    pub kernel_rsp: u64,
    /// offset 0x08 — scratch slot the stub parks the user stack pointer in.
    pub user_rsp: u64,
    /// offset 0x10 — this CPU's index, as assigned at bring-up.
    pub cpu_id: u32,
    _pad: u32,
    /// offset 0x18 — opaque pointer to the running thread, filled in by Task 4.
    pub current_thread: *mut (),
    /// offset 0x20 — address of this block.
    ///
    /// `gs:` addressing can read *through* the base but cannot produce it, so
    /// recovering `&PerCpu` needs a pointer stored inside the block itself.
    self_ptr: *const PerCpu,

    // Nothing below here has a fixed offset; assembly must not reach it.
    gdt: GlobalDescriptorTable,
    tss: TaskStateSegment,
    pub(crate) idt: InterruptDescriptorTable,
    selectors: Option<Selectors>,
    ist_stack: IstStack,
}

pub const OFFSET_KERNEL_RSP: usize = 0x00;
pub const OFFSET_USER_RSP: usize = 0x08;
pub const OFFSET_CPU_ID: usize = 0x10;
pub const OFFSET_CURRENT_THREAD: usize = 0x18;
pub const OFFSET_SELF_PTR: usize = 0x20;

// The `SYSCALL` stub hard-codes these. A field reorder that moved them would
// otherwise be found by a userspace process reading someone else's stack.
const _: () = {
    assert!(core::mem::offset_of!(PerCpu, kernel_rsp) == OFFSET_KERNEL_RSP);
    assert!(core::mem::offset_of!(PerCpu, user_rsp) == OFFSET_USER_RSP);
    assert!(core::mem::offset_of!(PerCpu, cpu_id) == OFFSET_CPU_ID);
    assert!(core::mem::offset_of!(PerCpu, current_thread) == OFFSET_CURRENT_THREAD);
    assert!(core::mem::offset_of!(PerCpu, self_ptr) == OFFSET_SELF_PTR);
};

impl PerCpu {
    const fn new(cpu_id: u32) -> Self {
        Self {
            kernel_rsp: 0,
            user_rsp: 0,
            cpu_id,
            _pad: 0,
            current_thread: core::ptr::null_mut(),
            self_ptr: core::ptr::null(),
            gdt: GlobalDescriptorTable::new(),
            tss: TaskStateSegment::new(),
            idt: InterruptDescriptorTable::new(),
            selectors: None,
            ist_stack: IstStack([0; IST_STACK_SIZE]),
        }
    }

    pub fn selectors(&self) -> &Selectors {
        self.selectors.as_ref().expect("per-CPU block not installed")
    }

    /// Whether this CPU's double-fault stack canary survives.
    ///
    /// `None` before installation, because the canary is written by `install`
    /// and the stack is zeroed until then — a bare `false` there would report a
    /// fault taken during early boot as a stack overflow it cannot have been.
    pub fn ist_canary_intact(&self) -> Option<bool> {
        self.selectors.as_ref()?;
        Some(unsafe { (&raw const self.ist_stack.0).cast::<u64>().read() } == IST_CANARY)
    }
}

/// The BSP's block, reserved statically because it is needed before any
/// allocator exists.
///
/// `UnsafeCell` rather than `static mut`: taking a reference to a `static mut`
/// is a hard error under `static_mut_refs` in edition 2024, and this milestone
/// bans it outright. Exactly one CPU ever touches this — the BSP, once, during
/// its own bring-up — which is what makes the `Sync` impl honest.
struct BspCell(UnsafeCell<PerCpu>);
// SAFETY: written once by the BSP in `install_bsp` before any other CPU is
// started, and thereafter reached only through `GS`, which every CPU points at
// its *own* block.
unsafe impl Sync for BspCell {}

static BSP: BspCell = BspCell(UnsafeCell::new(PerCpu::new(0)));

/// CPUs that have completed `install`. Bring-up ordering, not a lock.
static INSTALLED: AtomicU32 = AtomicU32::new(0);

/// Number of CPUs whose per-CPU block is live.
pub fn installed_count() -> u32 {
    INSTALLED.load(Ordering::Acquire)
}

/// Installs the bootstrap processor's block.
///
/// Idempotent, like M0's `gdt::init` before it: the in-QEMU test harness runs
/// several tests that each bring the CPU up from scratch, and the descriptor
/// tables must be rebuilt and reloaded each time (`ltr` refuses an already-busy
/// TSS descriptor) without the CPU being counted twice.
///
/// # Safety
/// Must be called on the BSP, before any other CPU is started.
pub unsafe fn install_bsp() {
    // SAFETY: single-threaded at this point in boot; no other reference to the
    // BSP block exists, and none can, because `current()` requires `GS` to be
    // set, which happens inside `finish_install` below.
    let block = unsafe { &mut *BSP.0.get() };
    unsafe { finish_install(block, 0) };
}

/// Installs an application processor's block, allocated on the kernel heap.
///
/// Deliberately leaked: the block outlives every reference to it, is reached
/// through `GS` for the life of the CPU, and freeing it would mean proving the
/// CPU is offline and no interrupt is in flight through its IST.
///
/// # Safety
/// Must be called once per AP, on that AP, after the kernel heap is up.
pub unsafe fn install_ap(cpu_id: u32) {
    let block = alloc::boxed::Box::leak(alloc::boxed::Box::new(PerCpu::new(cpu_id)));
    unsafe { finish_install(block, cpu_id) };
}

/// Shared tail: point `GS` at the block, then build and load this CPU's tables.
///
/// # Safety
/// `block` must be a live, uniquely-owned `PerCpu` that outlives this CPU.
unsafe fn finish_install(block: &mut PerCpu, cpu_id: u32) {
    // A repeat install on the same CPU rebuilds and reloads the tables but must
    // not be counted again -- `installed_count` is how SMP bring-up knows how
    // many CPUs are live, and double-counting would make it wait for a CPU that
    // does not exist.
    let first_time = block.selectors.is_none();
    block.cpu_id = cpu_id;
    block.self_ptr = block as *const PerCpu;

    // The canary sits at the lowest address, which is where a descending
    // overflow reaches first.
    unsafe { (&raw mut block.ist_stack.0).cast::<u64>().write(IST_CANARY) };
    let ist_top = block.ist_stack.0.as_ptr() as u64 + IST_STACK_SIZE as u64;

    // GS must be live before `gdt::build_and_load`, because the fault handlers
    // the IDT installs immediately afterwards read the per-CPU block, and a
    // fault taken between the two would otherwise dereference a null base.
    let base = block as *mut PerCpu as u64;
    unsafe {
        write_msr(IA32_GS_BASE, base);
        // Both MSRs hold the block address. `enter_user` does not `swapgs`
        // before `iretq`, so ring 3 runs with `GS_BASE` naming the kernel
        // block, and the syscall stub's `swapgs` pair only works because the
        // two are equal.
        //
        // `KERNEL_GS_BASE` is the authoritative copy, and that is what makes
        // the arrangement survivable. Ring 3 can zero the *hidden* `GS.base`
        // with three bytes -- `xor eax, eax; mov gs, ax` -- because loading a
        // segment register from ring 3 reloads the base from the descriptor.
        // Nothing prevents that and nothing should try to. What matters is
        // that no kernel entry path trusts `GS_BASE` on arrival: the syscall
        // stub reloads it via `swapgs`, and every interrupt and exception
        // handler reachable from ring 3 calls `restore_gs_base` before its
        // first `gs:` access. `KERNEL_GS_BASE` is writable only in ring 0, so
        // it is the one copy ring 3 cannot touch.
        write_msr(IA32_KERNEL_GS_BASE, base);
    }

    let selectors = unsafe { gdt::build_and_load(&mut block.gdt, &block.tss, ist_top) };
    block.selectors = Some(selectors);
    unsafe { crate::idt::build_and_load(&mut block.idt) };

    if first_time {
        INSTALLED.fetch_add(1, Ordering::AcqRel);
    }
}

/// # Safety
/// `msr` must be a writable MSR and `value` valid for it.
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

/// Repoints `GS_BASE` at this CPU's block, from the copy ring 3 cannot write.
///
/// Ring 3 can clear the hidden `GS.base` with `mov gs, ax`, so a handler
/// entered from ring 3 cannot assume `gs:` resolves to anything. Every
/// interrupt and exception handler that can be entered from ring 3 must call
/// this before its first `gs:` access, or it dereferences a base the faulting
/// process chose -- under that process's own page tables.
///
/// Deliberately *not* `swapgs`. `swapgs` is an exchange, so it is only correct
/// when paired with a second one on the way out and only when the caller knows
/// which side it is on; getting either wrong silently hands the kernel a user
/// value. This reads `KERNEL_GS_BASE`, which is ring-0-only, and writes
/// `GS_BASE` -- idempotent, unpaired, and correct whether or not ring 3
/// actually clobbered anything. It costs an `rdmsr`/`wrmsr` pair on entry,
/// which at the 100 Hz timer and on a fault path is not a measurable cost.
///
/// # Safety
/// This CPU's per-CPU block must have been installed, so `KERNEL_GS_BASE`
/// holds its address.
pub unsafe fn restore_gs_base() {
    // SAFETY: the caller guarantees the block is installed, so this MSR holds
    // its address; writing that same address to `GS_BASE` restores the
    // invariant every `gs:` access in the kernel depends on.
    unsafe {
        let base = read_msr(IA32_KERNEL_GS_BASE);
        write_msr(IA32_GS_BASE, base);
    }
}

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

/// This CPU's block.
///
/// # Panics
/// If `GS` has not been set — i.e. `install_*` has not run on this CPU. That is
/// a bring-up ordering bug, and a null deref here would present as a fault in
/// whatever happened to run next.
pub fn current() -> &'static PerCpu {
    let ptr = self_ptr();
    assert!(!ptr.is_null(), "per-CPU block read before install on this CPU");
    // SAFETY: the pointer was written by `finish_install` from a block that is
    // either `static` or leaked, so it is live for `'static`.
    unsafe { &*ptr }
}

/// This CPU's block, mutably.
///
/// # Safety
/// No `&PerCpu` from [`current`] may be live, and the returned reference must
/// not be held across any point where a *fault* can be taken. Masking
/// interrupts is not sufficient: the double-fault handler reads this block
/// through `gdt::ist_canary_intact`, and `#DF`/`#PF`/NMI are not maskable. The
/// obligation is about exception context, not about the interrupt flag.
#[allow(clippy::mut_from_ref)]
pub unsafe fn current_mut() -> &'static mut PerCpu {
    let ptr = self_ptr();
    assert!(!ptr.is_null(), "per-CPU block read before install on this CPU");
    unsafe { &mut *ptr.cast_mut() }
}

/// Whether this CPU has a per-CPU block installed.
///
/// Lets early-boot and panic paths degrade instead of asserting.
pub fn is_installed() -> bool {
    !self_ptr().is_null()
}

fn self_ptr() -> *const PerCpu {
    let ptr: u64;
    // SAFETY: a plain read of `gs:[0x20]`. Reads zero when `GS_BASE` is zero,
    // which the callers check for rather than dereferencing.
    unsafe {
        core::arch::asm!(
            "mov {}, gs:[{}]",
            out(reg) ptr,
            const OFFSET_SELF_PTR,
            options(nostack, preserves_flags, readonly),
        )
    };
    ptr as *const PerCpu
}

/// Records the stack the CPU switches to when it leaves ring 3.
///
/// Sets *both* places that matter, because they are used by different
/// mechanisms and setting only one produces a machine that works until it
/// doesn't:
///
/// - `PerCpu::kernel_rsp` is read by the `SYSCALL` entry stub, which does its
///   own stack switch because `syscall` does not.
/// - `TSS.privilege_stack_table[0]` is read by the *CPU* on any interrupt or
///   exception taken from ring 3. Leaving it zero means the first timer tick in
///   userspace pushes an interrupt frame to address 0.
///
/// # Safety
/// `stack_top` must be the top of a kernel stack that stays valid for as long
/// as this CPU can enter the kernel from ring 3.
pub unsafe fn set_kernel_stack(stack_top: u64) {
    let block = unsafe { current_mut() };
    block.kernel_rsp = stack_top;
    block.tss.privilege_stack_table[0] = x86_64::VirtAddr::new(stack_top);
}

/// This CPU's index.
pub fn cpu_id() -> u32 {
    current().cpu_id
}
