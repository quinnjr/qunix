use crate::boot;
use core::sync::atomic::{AtomicU64, Ordering};
use qunix_mm::{FrameBacking, buddy::BuddyAllocator};
use qunix_sync::IrqSpinLock;

/// Lowest address safe to allocate below 1 MiB: above the real-mode interrupt
/// vector table and the BIOS data area.
pub const LOW_USABLE_START: u64 = 0x1000;
/// Highest address conventional memory can reach. The EBDA normally starts
/// here, but its base is dynamic and can sit as low as 0x8_0000, so this is
/// only the ceiling — [`ebda_floor`] narrows it to what the BIOS reported.
pub const LOW_USABLE_END: u64 = 0x9_F000;
/// Physical address of the BDA word holding the EBDA segment.
const BDA_EBDA_SEGMENT: u64 = 0x40E;
const ONE_MIB: u64 = 0x10_0000;

/// Bytes the firmware called usable below 1 MiB that we refuse to manage.
///
/// Counted here rather than in the allocator because the allocator never sees
/// them: the exclusion happens before `add_region`, so its own drop counters
/// cannot account for it and the boot banner would silently lose the bytes.
static EXCLUDED_LOW_BYTES: AtomicU64 = AtomicU64::new(0);

/// Free/dropped accounting for one boot, gathered under a single lock.
///
/// A struct rather than a widening tuple: the four drop counters are only
/// useful because they are distinguishable, and positional access would make
/// every caller a place to confuse them.
pub struct FrameStats {
    pub free_bytes: u64,
    /// Bytes lost rounding region edges to page granularity.
    pub edge_dropped_bytes: u64,
    /// Bytes in regions the allocator refused for exceeding its region table.
    pub refused_region_bytes: u64,
    /// Bytes in firmware entries discarded whole as nonsensical.
    pub malformed_region_bytes: u64,
    /// Bytes excluded by this module's sub-1 MiB policy.
    pub excluded_low_bytes: u64,
}

/// The lowest address the EBDA is reported to occupy, clamped to
/// [`LOW_USABLE_END`].
///
/// The 0x9_F000 constant alone is a guess: the EBDA base is dynamic, published
/// as a paragraph address in the BDA, and a machine that places it at 0x8_0000
/// would have us hand 124 KiB of firmware-owned memory to the allocator.
///
/// A zero word means the BIOS published no EBDA, so the ceiling stands. Any
/// other value is honoured even when implausibly low — under-reporting costs
/// at most the whole 636 KiB window, and over-reporting corrupts the BIOS.
fn ebda_floor(hhdm: u64) -> u64 {
    // SAFETY: the BDA is at a fixed physical address in conventional RAM, and
    // the HHDM maps physical zero onward, so this read is in-bounds and aligned.
    let segment = unsafe { ((hhdm + BDA_EBDA_SEGMENT) as *const u16).read_volatile() } as u64;
    if segment == 0 { LOW_USABLE_END } else { LOW_USABLE_END.min(segment << 4) }
}

/// Reaches physical memory through Limine's higher-half direct map.
struct HhdmBacking {
    offset: u64,
}

impl FrameBacking for HhdmBacking {
    unsafe fn read_link(&self, pa: u64) -> u64 {
        unsafe { ((self.offset + pa) as *const u64).read_volatile() }
    }
    unsafe fn write_link(&self, pa: u64, value: u64) {
        unsafe { ((self.offset + pa) as *mut u64).write_volatile(value) };
    }
}

/// Constructed empty rather than wrapped in an `Option`: the allocator has a
/// `const fn new`, so an `Option` would only add a discriminant test and a
/// panic path to every single frame allocation.
///
/// `IrqSpinLock`, not `SpinLock`, for the same reason as the heap — frame
/// allocation is reachable from interrupt context, where a plain spinlock is a
/// same-CPU self-deadlock.
static ALLOCATOR: IrqSpinLock<BuddyAllocator<HhdmBacking>, qunix_hal_x86_64::Irq> =
    IrqSpinLock::new(BuddyAllocator::new(HhdmBacking { offset: 0 }));
static INITIALISED: IrqSpinLock<bool, qunix_hal_x86_64::Irq> = IrqSpinLock::new(false);

/// Populates the frame allocator from the bootloader memory map.
///
/// Idempotent: a second call is a no-op, which keeps tests independent of order.
pub fn init() {
    let mut done = INITIALISED.lock();
    if *done {
        return;
    }
    let hhdm = boot::hhdm_offset();
    let mut allocator = ALLOCATOR.lock();
    allocator.set_backing(HhdmBacking { offset: hhdm });

    // Read once: the BDA is ordinary RAM and nothing stops a later allocation
    // from landing on it, so the value must be captured before the first
    // `add_region` makes the window allocatable.
    let low_end = ebda_floor(hhdm);
    let mut excluded = 0u64;

    for region in boot::usable_regions() {
        // Firmware supplies these, so the end must not be computed by an
        // unchecked add the way a trusted value could be.
        let Some(end) = region.start.checked_add(region.len) else {
            continue;
        };

        // Below 1 MiB, only the conventional-memory window is safe. The IVT and
        // BDA sit under 0x1000 and the EBDA/ROM area above `low_end`; the rest
        // is ordinary RAM that the old blanket `max(0x10_0000)` threw away.
        //
        // This goes on the general free lists, which means boot-time
        // allocations will consume it. M1's AP startup trampolines need real
        // mode-addressable memory and must therefore carve a reserved
        // sub-1 MiB pool of their own before the heap comes up, not expect to
        // find anything left here.
        let low_start = region.start.max(LOW_USABLE_START);
        let low_stop = end.min(low_end);
        if low_start < low_stop {
            unsafe { allocator.add_region(low_start, low_stop - low_start) };
        }
        // Whatever the firmware called usable below 1 MiB and we did not take.
        // Without this the banner's totals quietly disagree with the memory
        // map, because the allocator is never offered these bytes at all.
        let in_low_mib = end.min(ONE_MIB).saturating_sub(region.start.min(ONE_MIB));
        excluded += in_low_mib - low_stop.saturating_sub(low_start);

        let high_start = region.start.max(ONE_MIB);
        if high_start < end {
            unsafe { allocator.add_region(high_start, end - high_start) };
        }
    }

    EXCLUDED_LOW_BYTES.store(excluded, Ordering::Relaxed);
    *done = true;
}

pub fn alloc(order: u8) -> Option<u64> {
    ALLOCATOR.lock().alloc(order)
}

/// # Safety
/// `pa` and `order` must match a previous successful `alloc`.
pub unsafe fn free(pa: u64, order: u8) {
    unsafe { ALLOCATOR.lock().free(pa, order) };
}

pub fn free_bytes() -> u64 {
    ALLOCATOR.lock().free_bytes()
}

/// Free and dropped byte counts under a single lock acquisition.
///
/// Every acquisition masks interrupts for its duration, so reporting five
/// numbers should not cost five of them.
pub fn stats() -> FrameStats {
    let allocator = ALLOCATOR.lock();
    FrameStats {
        free_bytes: allocator.free_bytes(),
        edge_dropped_bytes: allocator.edge_dropped_bytes(),
        refused_region_bytes: allocator.refused_region_bytes(),
        malformed_region_bytes: allocator.malformed_region_bytes(),
        // Outside the lock's remit, but written once during `init` and only
        // ever read after, so a relaxed load is enough.
        excluded_low_bytes: EXCLUDED_LOW_BYTES.load(Ordering::Relaxed),
    }
}
