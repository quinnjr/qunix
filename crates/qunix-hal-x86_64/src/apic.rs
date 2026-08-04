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

fn reg(offset: usize) -> *mut u32 {
    let base = APIC_BASE.load(Ordering::Acquire);
    assert!(base != 0, "apic::init has not been called");
    (base as usize + offset) as *mut u32
}

fn write(offset: usize, value: u32) {
    unsafe { reg(offset).write_volatile(value) };
}

/// Physical base address of this CPU's local APIC, from `IA32_APIC_BASE`.
///
/// Limine's HHDM covers RAM only, so this MMIO page must be mapped explicitly
/// (and uncacheable) before `init` is called.
pub fn phys_base() -> u64 {
    read_msr(IA32_APIC_BASE_MSR) & 0xFFFF_F000
}

/// Enables the local APIC on the current CPU.
///
/// Reaches the APIC MMIO window at `hhdm_offset + phys_base()`. That page is
/// device memory, which Limine's HHDM does not cover, so the caller must have
/// mapped it uncacheable first.
pub fn init(hhdm_offset: u64) {
    APIC_BASE.store(hhdm_offset + phys_base(), Ordering::Release);
    // Setting the enable bit with a spurious vector is what actually turns the
    // APIC on; without it no LVT entry will ever deliver.
    write(REG_SPURIOUS, SPURIOUS_ENABLE | SPURIOUS_VECTOR);
}

/// Starts the local APIC timer in periodic mode.
///
/// `divide` is the raw divide-configuration value (`0b1011` = divide by 1).
pub fn start_timer(divide: u32, initial_count: u32) {
    write(REG_TIMER_DIVIDE, divide);
    write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
    write(REG_TIMER_INITIAL, initial_count);
}

/// Signals end-of-interrupt. Must be called from every APIC interrupt handler.
pub fn eoi() {
    write(REG_EOI, 0);
}
