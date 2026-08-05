//! Cross-CPU TLB shootdown.
//!
//! Invalidating a translation on the CPU that changed the page table is not
//! enough once more than one CPU walks it. A remote CPU can hold the old
//! translation — in its TLB or in a paging-structure cache — and the caller of
//! `unmap` is about to hand the frame back to the allocator, which writes
//! free-list links into it. The remote CPU then reads allocator metadata
//! through a stale mapping, or walks it as a page table.
//!
//! # Waiting is the mechanism, not a refinement
//!
//! A shootdown that sends the IPI and returns is worth almost nothing: the
//! window it leaves is exactly the window the bug lives in. So the initiator
//! publishes the target, marks every other online CPU as owing an
//! acknowledgement, sends the IPI, and **does not return until every one of
//! those bits is clear**. Everything else here exists to make that wait
//! terminate.
//!
//! # Why the outstanding set is a bitmask and not a count
//!
//! A CPU that wants to start a shootdown while another is in progress must
//! spin, and it may be spinning with interrupts masked — `unmap` is reachable
//! from paths that mask them. A masked CPU cannot take the IPI, so the
//! in-progress initiator would wait for an acknowledgement that can never
//! arrive while the second CPU waits for a lock that is never released. The way
//! out is for the spinning CPU to do the invalidation *inline*, which it can
//! only do if it can tell whether the outstanding set includes it. A count
//! cannot answer that; one bit per `cpu_id` can.
//!
//! The consequence is that the IPI handler and the spin loop run the same
//! function, and that servicing must be idempotent: an IPI delivered after the
//! spin loop already acknowledged finds the bit clear and does nothing.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::structures::idt::InterruptStackFrame;

use crate::percpu::MAX_CPUS;

/// Vector the shootdown IPI is delivered on.
///
/// One above the timer. Installed by `idt::build_and_load` rather than by the
/// kernel, so that every CPU has it by construction: a CPU that took this
/// interrupt without a handler would triple-fault, and a CPU that never
/// installed it would leave every initiator waiting forever.
pub const SHOOTDOWN_VECTOR: u8 = 33;

/// Target meaning "invalidate everything", as opposed to a single page.
///
/// `u64::MAX` is not a page-aligned address and is not canonical, so it cannot
/// collide with a real request.
const TARGET_ALL: u64 = u64::MAX;

/// Which CPUs still owe an acknowledgement for the shootdown in progress, one
/// bit per `cpu_id`.
static PENDING: AtomicU64 = AtomicU64::new(0);

/// What the CPUs named in [`PENDING`] must invalidate.
static TARGET: AtomicU64 = AtomicU64::new(TARGET_ALL);

/// Whether a shootdown is in progress.
///
/// A bare flag rather than a `SpinLock`, because the wait loop has to do work
/// on each failed attempt (see the module docs) and a lock's own loop offers
/// nowhere to put it.
static IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Shootdowns each CPU has serviced, indexed by `cpu_id`.
///
/// The only externally visible evidence that a remote CPU actually performed
/// the invalidation. Without it the mechanism is unobservable from the
/// initiator: "the IPI was sent" and "the remote CPU invalidated and
/// acknowledged before I returned" look identical from here, and only the
/// second is the property that matters.
static SERVICED: [AtomicU64; MAX_CPUS as usize] =
    [const { AtomicU64::new(0) }; MAX_CPUS as usize];

/// How many shootdowns `cpu` has serviced. Saturates conceptually at `u64`.
pub fn serviced(cpu: u32) -> u64 {
    if cpu >= MAX_CPUS {
        return 0;
    }
    SERVICED[cpu as usize].load(Ordering::Acquire)
}

/// CPUs still owing an acknowledgement. Zero when no shootdown is outstanding.
pub fn pending() -> u64 {
    PENDING.load(Ordering::Acquire)
}

/// CPUs that must acknowledge a shootdown initiated by `self_id`.
///
/// Split out so the exclusion can be host-tested. Including the initiator would
/// make it wait for an acknowledgement only it can send, and it is inside the
/// wait loop rather than in the handler — that is a hang, not a slow path.
const fn remote_mask(online: u64, self_id: u32) -> u64 {
    if self_id >= MAX_CPUS {
        // A CPU outside the mask's width cannot be excluded by a bit, so it
        // would wait on itself. `percpu::mark_online` refuses such an id
        // outright; this is the second half of the same guard.
        return 0;
    }
    online & !(1u64 << self_id)
}

/// Invalidates one page on every other online CPU, and waits for all of them.
pub fn shootdown_page(va: u64) {
    shootdown(va);
}

/// Invalidates every non-global translation on every other online CPU, and
/// waits for all of them.
///
/// Used when the change is above the leaf — a page table unlinked by pruning
/// invalidates an unknown set of addresses, and the paging-structure caches
/// that hold it are not addressed by `invlpg` on the leaf.
pub fn shootdown_all() {
    shootdown(TARGET_ALL);
}

fn shootdown(target: u64) {
    // Not `percpu::cpu_id()`: this runs on every `unmap`, including ones taken
    // before any per-CPU block exists (the LAPIC remap in early boot), and
    // `cpu_id` asserts. With no block there is also no other CPU, so there is
    // nothing to shoot down.
    if !crate::percpu::is_installed() {
        return;
    }
    let remote = remote_mask(crate::percpu::online_mask(), crate::percpu::cpu_id());
    if remote == 0 {
        // The single-CPU case, and every case before the APs are up. The
        // caller's own invalidation has already happened.
        return;
    }

    while IN_PROGRESS
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // The other initiator may be waiting on *this* CPU, whose interrupts
        // may be masked. See the module docs: the work is done inline rather
        // than waited for, or neither side ever moves.
        service_pending();
        core::hint::spin_loop();
    }

    TARGET.store(target, Ordering::Release);
    PENDING.store(remote, Ordering::Release);
    // Asserted rather than ignored. `send_ipi_all_excluding_self` is a no-op
    // before the LAPIC is enabled, and a silently unsent IPI here is not a
    // degraded shootdown -- it is an infinite wait below, with no message.
    assert!(
        crate::apic::send_ipi_all_excluding_self(SHOOTDOWN_VECTOR),
        "tlb shootdown with cpus online but no local apic to send the ipi through"
    );

    // The whole point. Returning here with bits outstanding is indistinguishable
    // from never having sent the IPI: the caller frees the frame while a remote
    // CPU still resolves the address through it.
    while PENDING.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }

    IN_PROGRESS.store(false, Ordering::Release);
}

/// Performs and acknowledges this CPU's outstanding invalidation, if it has one.
///
/// Idempotent: called both from the IPI handler and from the spin loop above,
/// and the IPI routinely arrives after the spin loop has already cleared the
/// bit.
fn service_pending() {
    let cpu = crate::percpu::cpu_id();
    if cpu >= MAX_CPUS {
        return;
    }
    let bit = 1u64 << cpu;
    if PENDING.load(Ordering::Acquire) & bit == 0 {
        return;
    }
    invalidate(TARGET.load(Ordering::Acquire));
    SERVICED[cpu as usize].fetch_add(1, Ordering::AcqRel);
    // Released last, and with a read-modify-write, which on x86-64 is a full
    // fence: the initiator may free the frame the instant it sees this bit
    // clear, so the invalidation must not be reordered past it.
    PENDING.fetch_and(!bit, Ordering::AcqRel);
}

fn invalidate(target: u64) {
    if target == TARGET_ALL {
        // A CR3 reload, which drops every non-global entry. Nothing in this
        // kernel sets `PageFlags::GLOBAL` -- there is no such flag -- so
        // "non-global" is "all" for pages this kernel mapped. Limine's own
        // kernel mappings may be global, and they are never unmapped.
        x86_64::instructions::tlb::flush_all();
    } else {
        x86_64::instructions::tlb::flush(VirtAddr::new(target));
    }
}

/// The IPI handler. Installed on every CPU by `idt::build_and_load`.
pub(crate) extern "x86-interrupt" fn shootdown_handler(_frame: InterruptStackFrame) {
    // Before any `gs:` access. `service_pending` reads `percpu::cpu_id()`, and
    // this IPI can land while ring 3 is running -- which is free to have zeroed
    // the hidden `GS.base` with `mov gs, ax`. Same obligation as the timer and
    // the fault handlers.
    // SAFETY: this CPU's per-CPU block was installed before it could be sent an
    // IPI; `mark_online` is what puts it in the mask.
    unsafe { crate::percpu::restore_gs_base() };
    service_pending();
    // After the acknowledgement, so the initiator is released as early as
    // possible; before returning, or this CPU takes no further IPIs.
    crate::apic::eoi();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initiator_is_never_asked_to_acknowledge_itself() {
        // The negative direction, and the one that hangs rather than misbehaves:
        // the initiator waits for the mask to clear *before* it can service
        // anything, so a mask including itself is an unbreakable spin.
        let all_four = 0b1111u64;
        assert_eq!(remote_mask(all_four, 0), 0b1110);
        assert_eq!(remote_mask(all_four, 3), 0b0111);
        for cpu in 0..4u32 {
            assert_eq!(remote_mask(all_four, cpu) & (1 << cpu), 0, "cpu {cpu} waits on itself");
        }
    }

    #[test]
    fn a_cpu_that_is_not_online_is_not_waited_for() {
        // The other direction of the same hang: a CPU that never installed a
        // handler cannot acknowledge, so it must not be in the set. Only bit 0
        // is online here, so an initiator on cpu 0 has nobody to wait for.
        assert_eq!(remote_mask(0b0001, 0), 0);
        assert_eq!(remote_mask(0b0101, 0), 0b0100);
        assert_eq!(remote_mask(0, 0), 0, "an empty online set produced work");
    }

    #[test]
    fn a_cpu_id_wider_than_the_mask_waits_for_nobody_rather_than_shifting_out() {
        // `1u64 << 64` is undefined-shift territory and would panic in debug or
        // wrap to bit 0 in release -- which would silently exclude cpu 0 and
        // include the initiator. Refused instead.
        assert_eq!(remote_mask(u64::MAX, MAX_CPUS), 0);
        assert_eq!(remote_mask(u64::MAX, MAX_CPUS + 100), 0);
        // The last id that does fit still behaves.
        assert_eq!(remote_mask(u64::MAX, MAX_CPUS - 1), u64::MAX >> 1);
    }

    #[test]
    fn the_flush_everything_target_cannot_be_a_real_page() {
        // A page-aligned sentinel would collide with a legitimate request for
        // that address, turning a one-page invalidation into a full flush or
        // vice versa. Neither is loud.
        assert_ne!(TARGET_ALL & 0xfff, 0, "the sentinel is page-aligned");
    }

    #[test]
    fn serviced_reports_zero_for_a_cpu_outside_the_mask_rather_than_indexing() {
        assert_eq!(serviced(MAX_CPUS), 0);
        assert_eq!(serviced(u32::MAX), 0);
    }
}
