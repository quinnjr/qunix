use core::sync::atomic::{AtomicU64, Ordering};

pub const TIMER_VECTOR: u8 = 32;

const IA32_APIC_BASE_MSR: u32 = 0x1B;

// Register offsets, in bytes, from the local APIC base.
const REG_SPURIOUS: usize = 0xF0;
const REG_EOI: usize = 0xB0;
/// Interrupt Command Register, low half. Writing it is what sends the IPI, so
/// any high-half destination must already be in place — which the shorthand
/// below makes unnecessary.
const REG_ICR_LOW: usize = 0x300;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_DIVIDE: usize = 0x3E0;

/// Set by the APIC while an IPI is still being delivered; the next write to the
/// ICR must wait for it to clear or the pending one is lost.
const ICR_DELIVERY_STATUS: u32 = 1 << 12;
/// Level=assert. Required for every delivery mode except INIT de-assert, which
/// this kernel never sends.
const ICR_LEVEL_ASSERT: u32 = 1 << 14;
/// Destination shorthand 0b11: every CPU on the bus except the sender.
const ICR_ALL_EXCLUDING_SELF: u32 = 0b11 << 18;

const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const SPURIOUS_ENABLE: u32 = 1 << 8;
const SPURIOUS_VECTOR: u32 = 0xFF;

static APIC_BASE: AtomicU64 = AtomicU64::new(0);

fn read_msr(msr: u32) -> u64 {
    let (high, low): (u32, u32);
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | low as u64
}

/// Address of a local APIC register, or `None` before [`init`] has run.
fn reg(offset: usize) -> Option<*mut u32> {
    // `Acquire`, pairing with `init`'s `Release` store: an application processor
    // has to see the LAPIC mapping `init` established before it sees the base,
    // and program order alone only orders them on the CPU that did both. Free
    // at the ISA level on x86-64, where every load is already acquire.
    //
    // The zero check below is a branch rather than an `assert!` on purpose:
    // this runs on every timer tick, and an assert drags `core::panicking` and
    // a formatted message into the ISR's reachable code. `debug_assert!` would
    // compile out and leave release writing to a raw offset as an absolute
    // address.
    let base = APIC_BASE.load(Ordering::Acquire);
    if base == 0 {
        return None;
    }
    Some((base as usize + offset) as *mut u32)
}

/// Writes a local APIC register, or does nothing if the APIC is not yet enabled.
fn write(offset: usize, value: u32) {
    if let Some(reg) = reg(offset) {
        unsafe { reg.write_volatile(value) };
    }
}

/// Physical base address of this CPU's local APIC, from `IA32_APIC_BASE`.
///
/// Limine's HHDM covers RAM only, so this MMIO page must be mapped explicitly
/// (and uncacheable) before `init` is called.
pub fn phys_base() -> u64 {
    // The address field is bits 12..MAXPHYADDR, not 12..32. A 32-bit mask
    // silently returns a wrong low address if firmware relocates the LAPIC
    // above 4 GiB, and the caller then maps the wrong frame.
    read_msr(IA32_APIC_BASE_MSR) & 0x000F_FFFF_FFFF_F000
}

/// Enables the local APIC on the current CPU.
///
/// # Safety
/// `hhdm_offset` must be the bootloader's higher-half direct map offset, and
/// `hhdm_offset + phys_base()` must already be mapped present, writable and
/// uncacheable. This function writes through that address immediately, so a
/// wrong offset is an arbitrary MMIO write.
///
/// Reaches the APIC MMIO window at `hhdm_offset + phys_base()`. That page is
/// device memory, which Limine's HHDM does not cover, so the caller must have
/// mapped it uncacheable first.
pub unsafe fn init(hhdm_offset: u64) {
    let base = hhdm_offset
        .checked_add(phys_base())
        .expect("hhdm offset + apic base overflows the address space");
    APIC_BASE.store(base, Ordering::Release);
    // Setting the enable bit with a spurious vector is what actually turns the
    // APIC on; without it no LVT entry will ever deliver.
    write(REG_SPURIOUS, SPURIOUS_ENABLE | SPURIOUS_VECTOR);
}

/// Starts the local APIC timer in periodic mode.
///
/// `divide` is the raw divide-configuration value (`0b1011` = divide by 1).
///
/// A no-op before [`init`] has run: with no APIC base there is nothing to
/// program, and the timer cannot have been delivering anyway.
pub fn start_timer(divide: u32, initial_count: u32) {
    write(REG_TIMER_DIVIDE, divide);
    write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
    write(REG_TIMER_INITIAL, initial_count);
}

/// Sends a fixed IPI on `vector` to every CPU on the bus except this one.
///
/// Returns whether it was sent: `false` means this CPU has no local APIC base
/// yet, which is the only reason the write can be skipped. Callers that wait
/// for a response must treat that as an error rather than as a delivered
/// message — a silently unsent IPI is an unbounded wait.
///
/// The *shorthand* rather than an explicit destination, because this kernel
/// tracks CPUs by the firmware's `processor_id` and not by LAPIC id, so it has
/// no list of destinations to iterate. The shorthand also reaches CPUs that are
/// not yet in the shootdown mask; those either have the vector installed (in
/// which case the handler finds no bit set for them and does nothing) or have
/// interrupts masked in the bootloader's holding pen, where the IPI stays
/// pending until they have loaded an IDT of their own.
pub fn send_ipi_all_excluding_self(vector: u8) -> bool {
    let Some(icr) = reg(REG_ICR_LOW) else {
        return false;
    };
    // SAFETY: `reg` returns an address inside the LAPIC MMIO page the caller of
    // `init` mapped uncacheable.
    unsafe {
        // A previous IPI still in delivery would be overwritten by this write.
        while icr.read_volatile() & ICR_DELIVERY_STATUS != 0 {
            core::hint::spin_loop();
        }
        icr.write_volatile(ICR_ALL_EXCLUDING_SELF | ICR_LEVEL_ASSERT | vector as u32);
    }
    true
}

/// Signals end-of-interrupt. Must be called from every APIC interrupt handler.
///
/// A no-op before [`init`] has run: no APIC interrupt can be in service, so
/// there is no EOI owed.
pub fn eoi() {
    write(REG_EOI, 0);
}
