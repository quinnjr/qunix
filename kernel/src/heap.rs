use crate::{boot, frames};
use core::alloc::{GlobalAlloc, Layout};
use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};
use qunix_mm::slab::SlabHeap;
use qunix_sync::IrqSpinLock;

/// Virtual base of the kernel heap; chosen to sit clear of both the kernel
/// image at -2 GiB and the HHDM.
const HEAP_BASE: u64 = 0xffff_a000_0000_0000;
const HEAP_PAGES: u64 = 4096; // 16 MiB
/// Backing frames are asked for in 2 MiB blocks rather than one page at a time.
/// On an unfragmented map that is 8 frame-allocator acquisitions instead of
/// 4096; the degradation ladder below can drive it far higher, which is the
/// price of booting at all on a shattered memory map.
const HUGE_ORDER: u8 = 9;

/// `IrqSpinLock`, not `SpinLock`: the global allocator is reachable from an
/// interrupt handler the moment one of them allocates, and a plain spinlock
/// there is a same-CPU self-deadlock rather than merely a contention problem.
struct LockedHeap(IrqSpinLock<SlabHeap, qunix_hal_x86_64::Irq>);

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0.lock().alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.lock().dealloc(ptr, layout) };
    }
}

#[global_allocator]
static HEAP: LockedHeap = LockedHeap(IrqSpinLock::new(SlabHeap::new()));

// A lock, not an `AtomicBool`, and deliberately unlike `serial::init`: heap and
// frame init are multi-step and fallible, and a concurrent second caller must
// block until the first has finished rather than see a flag flipped early and
// use a half-built heap. `serial::init` writes a handful of UART registers with
// no window to observe, so a flag is all it needs.
static INITIALISED: IrqSpinLock<bool, qunix_hal_x86_64::Irq> = IrqSpinLock::new(false);

/// Bytes left in the heap's bump region.
///
/// This is the number that predicts heap death: `allocated_bytes` cannot,
/// because alignment padding is consumed without being credited.
/// Bytes currently handed out, in the extents the heap actually charged.
///
/// Unlike [`bump_remaining`] this is unaffected by whether a block came from
/// the bump region or from a large-block free list, which is what makes it
/// usable as an assertion that does not depend on what ran earlier.
pub fn allocated_bytes() -> usize {
    HEAP.0.lock().allocated_bytes()
}

pub fn bump_remaining() -> usize {
    HEAP.0.lock().bump_remaining()
}

/// Maps the kernel heap region and hands it to the slab allocator.
///
/// Idempotent, so tests may call it in any order.
pub fn init() {
    let mut done = INITIALISED.lock();
    if *done {
        return;
    }

    let mut space = unsafe { AddressSpace::active(boot::hhdm_offset()) };
    let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE;

    let mut mapped = 0u64;
    let mut hint = HUGE_ORDER;
    while mapped < HEAP_PAGES {
        // Take the largest block still useful, degrading on failure. Demanding
        // order 9 outright would make the kernel unbootable on a fragmented
        // memory map: add_region shards every region edge into a descending
        // staircase of orders, so a firmware map with several small usable
        // regions can hold hundreds of MiB and still have no 2 MiB-aligned
        // block. Degrading costs iterations; not degrading costs the boot.
        let remaining = HEAP_PAGES - mapped;
        // Start one order above the last that worked rather than at that order
        // itself. Restarting at 9 every time would repeat the whole failed
        // ladder for each block, and each failed attempt is a full lock
        // acquisition -- but never climbing back means a single fragmented
        // block pins the rest of the 16 MiB to order 0, thousands of single
        // page maps deep. Climbing costs one wasted attempt per block while
        // the allocator stays fragmented, and recovers the fast path the
        // moment it is not.
        let mut order = hint.min(remaining.ilog2() as u8);
        let block_pa = loop {
            if let Some(pa) = frames::alloc(order) {
                break pa;
            }
            if order == 0 {
                let s = frames::stats();
                panic!(
                    "out of frames while mapping the kernel heap: {mapped}/{HEAP_PAGES} pages \
                     mapped, {} KiB free, dropped {} KiB at edges / {} KiB refused / {} KiB \
                     malformed / {} KiB excluded below 1 MiB",
                    s.free_bytes / 1024,
                    s.edge_dropped_bytes / 1024,
                    s.refused_region_bytes / 1024,
                    s.malformed_region_bytes / 1024,
                    s.excluded_low_bytes / 1024,
                );
            }
            order -= 1;
        };
        hint = (order + 1).min(HUGE_ORDER);

        // An order-9 block is 2 MiB and naturally aligned, so when the heap
        // offset is 2 MiB aligned too it can be mapped at PD granularity: one
        // walk and one PD entry per 2 MiB instead of 512 walks and a whole PT
        // frame. Both halves of that are checked here, rather than resting on
        // `mapped` happening to stay aligned because the order hint only ever
        // fell -- which stopped being true the moment the hint learned to climb.
        if order >= HUGE_ORDER && mapped % (1u64 << HUGE_ORDER) == 0 {
            unsafe {
                space
                    .map_2mib(
                        HEAP_BASE + mapped * qunix_mm::PAGE_SIZE,
                        block_pa,
                        flags,
                        &mut || frames::alloc(0),
                    )
                    .expect("failed to map a kernel heap huge page");
            }
        } else {
            for page in 0..(1u64 << order) {
                let va = HEAP_BASE + (mapped + page) * qunix_mm::PAGE_SIZE;
                let pa = block_pa + page * qunix_mm::PAGE_SIZE;
                unsafe {
                    space
                        .map(va, pa, flags, &mut || frames::alloc(0))
                        .expect("failed to map a kernel heap page");
                }
            }
        }
        mapped += 1u64 << order;
    }

    unsafe {
        HEAP.0
            .lock()
            .set_backing(HEAP_BASE as usize, (HEAP_PAGES * qunix_mm::PAGE_SIZE) as usize)
    };
    *done = true;
}
