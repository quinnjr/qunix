use crate::{boot, frames};
use core::alloc::{GlobalAlloc, Layout};
use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};
use qunix_mm::slab::SlabHeap;
use qunix_sync::SpinLock;

/// Virtual base of the kernel heap; chosen to sit clear of both the kernel
/// image at -2 GiB and the HHDM.
const HEAP_BASE: u64 = 0xffff_a000_0000_0000;
const HEAP_PAGES: u64 = 4096; // 16 MiB

struct LockedHeap(SpinLock<SlabHeap>);

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.0.lock().alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.lock().dealloc(ptr, layout) };
    }
}

#[global_allocator]
static HEAP: LockedHeap = LockedHeap(SpinLock::new(SlabHeap::new()));

static INITIALISED: SpinLock<bool> = SpinLock::new(false);

/// Maps the kernel heap region and hands it to the slab allocator.
///
/// Idempotent, so tests may call it in any order.
pub fn init() {
    let mut done = INITIALISED.lock();
    if *done {
        return;
    }

    let mut space = unsafe { AddressSpace::active(boot::hhdm_offset()) };
    for page in 0..HEAP_PAGES {
        let va = HEAP_BASE + page * qunix_mm::PAGE_SIZE;
        let pa = frames::alloc(0).expect("out of frames while mapping the kernel heap");
        unsafe {
            space
                .map(
                    va,
                    pa,
                    PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
                    &mut || frames::alloc(0),
                )
                .expect("failed to map a kernel heap page");
        }
    }

    unsafe {
        HEAP.0
            .lock()
            .add_backing(HEAP_BASE as usize, (HEAP_PAGES * qunix_mm::PAGE_SIZE) as usize)
    };
    *done = true;
}
