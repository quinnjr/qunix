use core::sync::atomic::{AtomicU64, Ordering};

pub const TIMER_VECTOR: u8 = 32;

const IA32_APIC_BASE_MSR: u32 = 0x1B;

// Register offsets, in bytes, from the local APIC base.
const REG_SPURIOUS: usize = 0xF0;
const REG_EOI: usize = 0xB0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_DIVIDE: usize = 0x3E0;

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

/// Signals end-of-interrupt. Must be called from every APIC interrupt handler.
///
/// A no-op before [`init`] has run: no APIC interrupt can be in service, so
/// there is no EOI owed.
pub fn eoi() {
    write(REG_EOI, 0);
}
