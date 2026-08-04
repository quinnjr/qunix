use crate::boot;
use qunix_mm::{FrameBacking, buddy::BuddyAllocator};
use qunix_sync::SpinLock;

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

static ALLOCATOR: SpinLock<Option<BuddyAllocator<HhdmBacking>>> = SpinLock::new(None);

/// Populates the frame allocator from the bootloader memory map.
///
/// Idempotent: a second call is a no-op, which keeps tests independent of order.
pub fn init() {
    let mut guard = ALLOCATOR.lock();
    if guard.is_some() {
        return;
    }
    let offset = boot::hhdm_offset();
    let mut allocator = BuddyAllocator::new(HhdmBacking { offset });

    for region in boot::usable_regions() {
        // Skip the first megabyte: legacy BIOS structures live there, and some
        // firmware marks parts of it usable when it is not safe to scribble on.
        let start = region.start.max(0x10_0000);
        if start >= region.start + region.len {
            continue;
        }
        let len = region.start + region.len - start;
        unsafe { allocator.add_region(start, len) };
    }

    *guard = Some(allocator);
}

pub fn alloc(order: u8) -> Option<u64> {
    ALLOCATOR.lock().as_mut().expect("frames::init not called").alloc(order)
}

/// # Safety
/// `pa` and `order` must match a previous successful `alloc`.
pub unsafe fn free(pa: u64, order: u8) {
    unsafe { ALLOCATOR.lock().as_mut().expect("frames::init not called").free(pa, order) };
}

pub fn free_bytes() -> u64 {
    ALLOCATOR.lock().as_ref().map(|a| a.free_bytes()).unwrap_or(0)
}
