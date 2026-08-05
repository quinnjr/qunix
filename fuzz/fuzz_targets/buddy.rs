#![no_main]
//! Differential-free structural fuzzing of the buddy allocator.
//!
//! The property that matters here is not "does it crash" — the allocator is
//! `no_std` arithmetic over an array and will happily not crash while handing
//! the same frame to two callers. This codebase has already shipped exactly
//! that bug twice, so the harness asserts the thing those bugs violated:
//!
//! **No two live allocations may overlap**, and every allocation must lie
//! inside a region that was actually added.
//!
//! Both are checked after every single operation, against an independent
//! model, so a violation is reported at the operation that caused it rather
//! than whenever the allocator next happens to trip over itself.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use qunix_mm::{FrameBacking, MAX_ORDER, PAGE_SIZE, buddy::BuddyAllocator};
use std::cell::UnsafeCell;

/// Physical base of the fake arena. Deliberately not 0: a bug that produces a
/// null-ish address should look different from a valid one.
const ARENA_BASE: u64 = 0x10_0000;
const ARENA_LEN: u64 = 16 * 1024 * 1024;

/// Raw volatile access, matching `HhdmBacking` in the kernel rather than
/// anything more defensive. A bounds-checked backing would turn an
/// out-of-region write — a real bug — into a clean panic that reads as a
/// harness failure, and would hide which address was actually touched.
struct VecBacking {
    mem: UnsafeCell<Vec<u8>>,
}

impl VecBacking {
    fn new() -> Self {
        Self { mem: UnsafeCell::new(vec![0u8; ARENA_LEN as usize]) }
    }
}

impl FrameBacking for VecBacking {
    unsafe fn read_link(&self, pa: u64) -> u64 {
        assert!(
            (ARENA_BASE..ARENA_BASE + ARENA_LEN).contains(&pa),
            "read_link outside the arena at {pa:#x}"
        );
        let mem = unsafe { &*self.mem.get() };
        unsafe { mem.as_ptr().add((pa - ARENA_BASE) as usize).cast::<u64>().read_volatile() }
    }

    unsafe fn write_link(&self, pa: u64, value: u64) {
        assert!(
            (ARENA_BASE..ARENA_BASE + ARENA_LEN).contains(&pa),
            "write_link outside the arena at {pa:#x}"
        );
        let mem = unsafe { &mut *self.mem.get() };
        unsafe {
            mem.as_mut_ptr().add((pa - ARENA_BASE) as usize).cast::<u64>().write_volatile(value)
        };
    }
}

#[derive(Arbitrary, Debug)]
enum Op {
    /// Offsets rather than absolute addresses, so the fuzzer spends its budget
    /// on interesting shapes instead of rediscovering where the arena is.
    AddRegion { offset: u32, len: u32 },
    Alloc { order: u8 },
    /// Frees a live allocation, indexed modulo the live set.
    Free { which: u16 },
    /// Frees an address the allocator never handed out. This is a *supported*
    /// path — `free` is documented to reject foreign addresses rather than
    /// corrupt itself — so the assertion is that the counter moves and the
    /// heap stays consistent, not that nothing happens.
    FreeForeign { offset: u32, order: u8 },
}

fuzz_target!(|ops: Vec<Op>| {
    let mut buddy = BuddyAllocator::new(VecBacking::new());
    // (pa, order) for every block currently handed out.
    let mut live: Vec<(u64, u8)> = Vec::new();
    // Regions the model believes were accepted, as (start, end).
    let mut regions: Vec<(u64, u64)> = Vec::new();

    for op in ops {
        match op {
            Op::AddRegion { offset, len } => {
                // Clamped into the arena because the backing is only that big.
                // An unclamped address would fault the harness rather than the
                // allocator, which tests nothing.
                let start = ARENA_BASE + (offset as u64 % ARENA_LEN);
                let len = len as u64 % (ARENA_LEN / 2);
                let end = (start + len).min(ARENA_BASE + ARENA_LEN);
                if end <= start {
                    continue;
                }
                // Regions must not overlap each other: the allocator is
                // entitled to assume the firmware map does not double-count
                // RAM, and overlapping input would make the model wrong, not
                // the allocator.
                if regions.iter().any(|&(s, e)| start < e && s < end) {
                    continue;
                }
                let before = buddy.free_bytes();
                unsafe { buddy.add_region(start, end - start) };
                assert!(
                    buddy.free_bytes() >= before,
                    "add_region reduced free_bytes: {} -> {}",
                    before,
                    buddy.free_bytes()
                );
                // Only recorded once the allocator has actually taken usable
                // memory from it. A range too small to contain a whole page is
                // dropped entirely and leaves `region_count` at zero, so a
                // model that assumed acceptance would go on to call `free` and
                // trip the allocator's deliberate pre-init guard -- which is
                // how the first two versions of this harness "found a bug".
                if buddy.free_bytes() > before {
                    regions.push((start, end));
                }
            }

            Op::Alloc { order } => {
                let order = order % (MAX_ORDER + 1);
                let Some(pa) = buddy.alloc(order) else { continue };
                let size = PAGE_SIZE << order;

                assert_eq!(pa % size, 0, "alloc({order}) returned {pa:#x}, not {size}-aligned");
                assert!(
                    regions.iter().any(|&(s, e)| pa >= s && pa + size <= e),
                    "alloc({order}) returned {pa:#x}..{:#x}, outside every added region",
                    pa + size
                );
                // The invariant two shipped bugs violated.
                for &(other, other_order) in &live {
                    let other_size = PAGE_SIZE << other_order;
                    assert!(
                        pa + size <= other || other + other_size <= pa,
                        "alloc({order}) returned {pa:#x}..{:#x}, overlapping live \
                         {other:#x}..{:#x}",
                        pa + size,
                        other + other_size
                    );
                }
                live.push((pa, order));
            }

            Op::Free { which } => {
                // `free` deliberately asserts when no region has been added --
                // pre-init the backing is a placeholder and `push` would write
                // through a zero offset. Respecting that guard rather than
                // tripping it is the harness's job.
                if live.is_empty() || regions.is_empty() {
                    continue;
                }
                let (pa, order) = live.swap_remove(which as usize % live.len());
                let before = buddy.free_bytes();
                unsafe { buddy.free(pa, order) };
                // Freeing a block the allocator handed out must return exactly
                // its bytes. A short return means coalescing swallowed memory.
                assert_eq!(
                    buddy.free_bytes(),
                    before + (PAGE_SIZE << order),
                    "free({pa:#x}, {order}) did not return {} bytes",
                    PAGE_SIZE << order
                );
            }

            Op::FreeForeign { offset, order } => {
                if regions.is_empty() {
                    continue;
                }
                let order = order % (MAX_ORDER + 1);
                let size = PAGE_SIZE << order;
                let pa = ARENA_BASE + (offset as u64 % ARENA_LEN) / size * size;
                // Only interesting if it really is foreign: inside no region,
                // and not a live block.
                if regions.iter().any(|&(s, e)| pa >= s && pa + size <= e) {
                    continue;
                }
                if live.iter().any(|&(l, _)| l == pa) {
                    continue;
                }
                let before_free = buddy.free_bytes();
                let before_foreign = buddy.foreign_frees();
                unsafe { buddy.free(pa, order) };
                assert_eq!(
                    buddy.free_bytes(),
                    before_free,
                    "a foreign free at {pa:#x} added memory to the heap"
                );
                // Note the units: `foreign_frees` accumulates *bytes*
                // (`+= block_size(order)`), not a number of events, despite
                // the plural name.
                assert_eq!(
                    buddy.foreign_frees(),
                    before_foreign + size,
                    "a foreign free at {pa:#x} was not counted as rejected"
                );
            }
        }
    }

    // Whatever the sequence was, the allocator cannot claim more free memory
    // than was ever handed to it.
    let total: u64 = regions.iter().map(|&(s, e)| e - s).sum();
    assert!(
        buddy.free_bytes() <= total,
        "free_bytes {} exceeds the {total} bytes ever added",
        buddy.free_bytes()
    );
});
