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
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use qunix_sched::RunQueue;
use qunix_sync::IrqSpinLock;
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
    /// offset 0x18 — id of the thread this CPU is running, or [`NO_THREAD`].
    ///
    /// One slot per CPU, which is the whole reason application processors can
    /// schedule. A single shared "what am I running" field would have the first
    /// switch save one CPU's stack pointer into the other CPU's context, and
    /// two threads would then be running on one stack.
    ///
    /// Not behind a lock: only the owning CPU ever writes it, and it is written
    /// with interrupts masked as part of a scheduling decision. Other CPUs read
    /// it for diagnostics only.
    current_thread: AtomicU64,
    /// offset 0x20 — address of this block.
    ///
    /// `gs:` addressing can read *through* the base but cannot produce it, so
    /// recovering `&PerCpu` needs a pointer stored inside the block itself.
    self_ptr: *const PerCpu,

    // Nothing below here has a fixed offset; assembly must not reach it.
    /// This CPU's queue of runnable threads.
    ///
    /// Per-CPU rather than one global queue, which is what removes the single
    /// lock every scheduling decision on every CPU used to serialise on.
    ///
    /// It still has a lock of its own, and that is not the lock being removed:
    /// work stealing means another CPU reaches into this queue, so the queue
    /// needs mutual exclusion with exactly one other party at a time. It is a
    /// *leaf* — nothing is ever acquired while it is held, and in particular
    /// not the scheduler's thread table — so two CPUs stealing from each other
    /// cannot deadlock.
    run_queue: IrqSpinLock<RunQueue, crate::Irq>,
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
            current_thread: AtomicU64::new(NO_THREAD),
            self_ptr: core::ptr::null(),
            run_queue: IrqSpinLock::new(RunQueue::new()),
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

/// Widest `cpu_id` the online mask can represent.
///
/// One atomic word, because a TLB shootdown has to test and clear a single
/// CPU's bit from an interrupt handler and a multi-word set could not be
/// updated atomically. A machine reporting a larger id is refused loudly at
/// [`mark_online`] rather than silently dropped from every shootdown, which
/// would be a correctness hole rather than a capacity limit.
pub const MAX_CPUS: u32 = 64;

/// CPUs that can service an inter-processor interrupt, one bit per `cpu_id`.
///
/// Deliberately *not* set by `install`. A per-CPU block is not enough to
/// respond to an IPI: the CPU also needs its local APIC software-enabled and
/// interrupts unmasked. A CPU in this mask that cannot take the IPI would make
/// every TLB shootdown initiator wait forever for an acknowledgement it is
/// unable to send, so the mask means "can acknowledge", not "exists".
static ONLINE_MASK: AtomicU64 = AtomicU64::new(0);

/// This CPU's index, keyed by its initial APIC id, or `u32::MAX` for an id no
/// processor has claimed.
///
/// Exists so a processor can identify itself **without touching `GS`**.
/// [`cpu_id`] reads `gs:[0x20]`, which is correct everywhere it is used from a
/// known context and fatal from an arbitrary one: ring 3 can zero the hidden
/// `GS.base` with `mov gs, ax`, and the read then lands on linear address 0x20
/// under whatever tables are active. A spinlock wait is exactly such an
/// arbitrary context -- it happens on every processor in every address space --
/// so the TLB shootdown it has to service cannot go through `GS`.
static APIC_TO_CPU: [AtomicU32; MAX_CPUS as usize] =
    [const { AtomicU32::new(u32::MAX) }; MAX_CPUS as usize];

/// This processor's initial APIC id, from `CPUID` leaf 1.
///
/// A pure register operation: no memory is read, so it is valid in any context,
/// including one whose `GS.base` is zero and whose page tables are a user
/// process's.
pub fn initial_apic_id() -> u32 {
    // Safe: `CPUID` is unconditionally available on x86-64, leaf 1 is
    // architectural, and the intrinsic is safe for exactly that reason. It
    // touches no memory and faults on nothing, which is what makes it usable
    // from a lock wait.
    let result = core::arch::x86_64::__cpuid(1);
    result.ebx >> 24
}

/// This CPU's index, derived without reading `GS`.
///
/// `None` before this processor has installed its per-CPU block, which is also
/// exactly when no shootdown can be waiting on it: `mark_online` runs later
/// still, so its bit is not in any `remote_mask`.
pub fn cpu_id_without_gs() -> Option<u32> {
    let apic = initial_apic_id();
    if apic >= MAX_CPUS {
        return None;
    }
    match APIC_TO_CPU[apic as usize].load(Ordering::Acquire) {
        u32::MAX => None,
        cpu => Some(cpu),
    }
}

/// Sentinel for "this CPU is running no thread the scheduler knows about".
///
/// A real id, not zero: thread 0 is the bootstrap processor's own idle thread,
/// so zero would make an uninitialised CPU claim to be running it.
pub const NO_THREAD: u64 = u64::MAX;

/// Every installed block, indexed by `cpu_id`.
///
/// Needed because per-CPU state stops being private the moment work stealing
/// exists: a CPU with an empty run queue has to reach another CPU's. `GS` can
/// only ever produce the block of the CPU doing the asking, so the blocks are
/// registered here as they are installed.
static BLOCKS: [AtomicPtr<PerCpu>; MAX_CPUS as usize] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS as usize];

fn block_ptr(cpu: u32) -> Option<*const PerCpu> {
    if cpu >= MAX_CPUS {
        return None;
    }
    let ptr = BLOCKS[cpu as usize].load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    Some(ptr.cast_const())
}

/// A CPU's run queue, or `None` if that CPU has never installed a block.
///
/// Deliberately hands back the queue rather than the block. The two fields any
/// CPU may touch on another's block are synchronised — this one by its own
/// lock, `current_thread` by being atomic — but the GDT, TSS and IDT beside
/// them are mutated by their owning CPU through `current_mut()`, and a
/// `&'static PerCpu` would alias those.
pub fn run_queue_of(cpu: u32) -> Option<&'static IrqSpinLock<RunQueue, crate::Irq>> {
    let ptr = block_ptr(cpu)?;
    // SAFETY: `finish_install` publishes only a block that is `static` or
    // leaked, so it lives for `'static`, and it publishes last — after the
    // block is fully constructed.
    Some(unsafe { &(*ptr).run_queue })
}

/// The thread a CPU is running, or `None` if it has never installed a block.
pub fn current_thread_of(cpu: u32) -> Option<u64> {
    let ptr = block_ptr(cpu)?;
    // SAFETY: as `run_queue_of`; the field is atomic, which is what makes a
    // cross-CPU read of it well-defined.
    Some(unsafe { (*ptr).current_thread.load(Ordering::Acquire) })
}

/// This CPU's run queue.
pub fn run_queue() -> &'static IrqSpinLock<RunQueue, crate::Irq> {
    &current().run_queue
}

/// The thread this CPU is running, or [`NO_THREAD`].
pub fn current_thread() -> u64 {
    current().current_thread.load(Ordering::Acquire)
}

/// Records the thread this CPU is running.
///
/// # Safety
/// Must be called as part of a scheduling decision made with interrupts masked
/// on this CPU. The slot is what says which context the next switch may save
/// into, so a stale or foreign value puts two threads on one stack.
pub unsafe fn set_current_thread(id: u64) {
    current().current_thread.store(id, Ordering::Release);
}

/// Number of CPUs whose per-CPU block is live.
pub fn installed_count() -> u32 {
    INSTALLED.load(Ordering::Acquire)
}

/// CPUs able to service an IPI, one bit per `cpu_id`.
pub fn online_mask() -> u64 {
    ONLINE_MASK.load(Ordering::Acquire)
}

/// Declares that this CPU can now service inter-processor interrupts.
///
/// Idempotent. Must be called *after* this CPU has loaded its IDT, enabled its
/// local APIC and unmasked interrupts — see [`ONLINE_MASK`] for what goes wrong
/// when it is called earlier.
pub fn mark_online() {
    let cpu = cpu_id();
    assert!(
        cpu < MAX_CPUS,
        "cpu id {cpu} exceeds the {MAX_CPUS}-cpu online mask; it could not be waited for \
         by a tlb shootdown"
    );
    ONLINE_MASK.fetch_or(1u64 << cpu, Ordering::AcqRel);
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
    // Published before anything else here. From this point the processor can
    // name itself without `GS`, which is what lets a spinlock wait service a
    // shootdown -- and the window this closes is the one where the block is
    // installed but unannounced, during which a wait would spin deaf.
    let apic = initial_apic_id();
    assert!(
        apic < MAX_CPUS,
        "initial apic id {apic} exceeds the {MAX_CPUS}-entry table; this cpu could not identify \
         itself to service a tlb shootdown, and a lock wait here would deadlock the machine"
    );
    APIC_TO_CPU[apic as usize].store(cpu_id, Ordering::Release);

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

    // Published last, so no other CPU can reach a half-built block through
    // `block()`. A repeat install on the BSP republishes the same address.
    assert!(
        cpu_id < MAX_CPUS,
        "cpu id {cpu_id} exceeds the {MAX_CPUS}-cpu block registry; its run queue would be \
         unreachable to work stealing"
    );
    BLOCKS[cpu_id as usize].store(block as *mut PerCpu, Ordering::Release);

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
