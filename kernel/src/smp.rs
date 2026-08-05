//! Application-processor bring-up.
//!
//! Limine starts the APs for us: each one sits in the bootloader's holding pen
//! until a `goto_addr` is written into its [`MpInfo`], then jumps there. So
//! "bring-up" here is not the classic INIT-SIPI-SIPI dance — it is handing each
//! CPU an entry point and waiting for it to report in.
//!
//! # What an AP does, and what it does not
//!
//! Each AP installs its own per-CPU block — its own GDT, TSS, IDT and
//! double-fault stack — enables its local APIC, unmasks interrupts, and then
//! parks in `hlt`. It does **not** run scheduler threads.
//!
//! That is a real limit, not an oversight, and it has a specific cause:
//! `sched::Scheduler::current` is a single field naming one running thread. On
//! one CPU that is the truth; with APs scheduling it would be two CPUs sharing
//! one "what am I running" slot, and the first switch would have one CPU save
//! its stack pointer into the other's context. `percpu::PerCpu` already carries
//! a `current_thread` slot for exactly this, and moving `current` into it is
//! what makes APs schedulable. Until that happens, parking is the honest
//! behaviour: an AP that took work would corrupt the CPU that gave it.
//!
//! Parking with interrupts *enabled* is not a step towards that. It is what a
//! TLB shootdown requires: `hlt` with `IF` clear is not woken by a maskable
//! interrupt at all, so an AP parked the way M1 parked them could never
//! acknowledge an invalidation, and every initiator would spin forever. An AP
//! here still takes no scheduling work — its LAPIC timer was never started, so
//! the only interrupt that can reach it is an IPI.

use core::sync::atomic::{AtomicU32, Ordering};
use limine::mp::MpInfo;
use limine::request::MpRequest;

/// Asks Limine to start the APs and hold them for us.
///
/// In the same `.requests` section as the other requests — see `boot.rs`.
#[used]
#[unsafe(link_section = ".requests")]
static MP_REQUEST: MpRequest = MpRequest::new(0);

/// APs that have finished installing their per-CPU state.
///
/// Does not count the BSP: `online_count` is about CPUs this module started.
static ONLINE: AtomicU32 = AtomicU32::new(0);

/// Total CPUs the firmware reports, including the bootstrap processor.
///
/// Zero when the bootloader did not answer the MP request at all, which is
/// distinguishable from "one CPU" and is why this is not `1`-by-default.
pub fn cpu_count() -> u32 {
    MP_REQUEST.response().map_or(0, |r| r.cpus().len() as u32)
}

/// Application processors that have completed bring-up.
pub fn online_count() -> u32 {
    ONLINE.load(Ordering::Acquire)
}

/// Whether every AP the firmware reported has come online.
pub fn all_online() -> bool {
    let total = cpu_count();
    // `total - 1` because the BSP is in the list but is not started by us.
    total > 0 && online_count() >= total - 1
}

/// Starts every application processor and returns immediately.
///
/// Returns the number of CPUs it asked to start. Waiting for them is the
/// caller's business — [`wait_for_all`] does it with a bound.
pub fn start_all() -> u32 {
    let Some(response) = MP_REQUEST.response() else {
        // No MP response means a bootloader that did not honour the request.
        // Not a panic: a single-CPU boot is a legitimate configuration and the
        // kernel runs correctly on the BSP alone.
        return 0;
    };
    let bsp = response.bsp_lapic_id;
    let mut started = 0;
    for cpu in response.cpus() {
        if cpu.lapic_id == bsp {
            // The BSP is already running this code.
            continue;
        }
        // `processor_id` is the firmware's index and is what the AP will use as
        // its `cpu_id`, so per-CPU blocks are labelled the way the firmware and
        // any later ACPI table agree on, rather than by arrival order.
        cpu.bootstrap(ap_entry, cpu.processor_id as u64);
        started += 1;
    }
    started
}

/// Spins until every AP is online or the budget runs out.
///
/// Returns whether they all made it. A bounded wait rather than an unbounded
/// one because a CPU that never arrives is a real possibility — a firmware that
/// lists a processor it cannot start — and hanging the boot on it would be
/// worse than continuing with fewer CPUs.
pub fn wait_for_all(mut budget: u64) -> bool {
    while budget > 0 {
        if all_online() {
            return true;
        }
        core::hint::spin_loop();
        budget -= 1;
    }
    all_online()
}

/// First code an application processor runs.
///
/// # Safety
/// Called by the bootloader on a CPU with no per-CPU state, no usable stack
/// beyond the one Limine provided, and interrupts disabled. It must not return.
unsafe extern "C" fn ap_entry(info: &MpInfo) -> ! {
    let cpu_id = info.extra_argument() as u32;

    // First thing, before anything can fault: this CPU has no IDT until now, so
    // a fault before this triple-faults with no diagnostic at all.
    //
    // Allocating here is safe because `start_all` is called long after
    // `heap::init` — the BSP's block is static precisely because *it* could not
    // wait for the heap, but an AP always can.
    unsafe { qunix_hal_x86_64::percpu::install_ap(cpu_id) };

    // A local APIC that is not software-enabled does not accept fixed
    // interrupts, so without this the AP would sit in the shootdown mask and
    // never take the IPI. The MMIO page was mapped by the BSP into the kernel
    // half every address space shares, so it is already reachable here, and
    // `init` is idempotent — it stores the same base this CPU's `IA32_APIC_BASE`
    // reports.
    // SAFETY: `map_lapic` ran on the BSP before any AP was started, mapping
    // exactly this page uncacheable.
    unsafe { qunix_hal_x86_64::apic::init(crate::boot::hhdm_offset()) };

    // Interrupts on before the mask, not after. `mark_online` is a promise that
    // this CPU can acknowledge a TLB shootdown, and a CPU that is in the mask
    // but cannot take the IPI hangs every initiator. The LAPIC timer is
    // deliberately not started here: an AP has no run queue, so a tick would
    // only lead into `preempt`.
    x86_64::instructions::interrupts::enable();
    qunix_hal_x86_64::percpu::mark_online();

    ONLINE.fetch_add(1, Ordering::AcqRel);

    // Parked. See the module docs: taking scheduler work from here would
    // corrupt the CPU that queued it, because `sched.current` is not per-CPU
    // yet. `hlt` in a loop rather than a spin so the core is not burned, and
    // with interrupts enabled so a shootdown IPI actually wakes it.
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
