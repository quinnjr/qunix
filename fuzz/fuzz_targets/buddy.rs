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
//! Both run in `admit`, which every block the allocator hands out goes through
//! before it joins the live set -- including the re-allocation the `Free` arm
//! performs, which is the one the doc below relies on. A `free` that corrupts
//! a list therefore surfaces at the next `alloc` of that order rather than at
//! the free itself; the exact `free_bytes` assertion after *every* op is what
//! narrows it down. If you add an `Op` that allocates, call `admit`.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use qunix_mm::{FrameBacking, MAX_ORDER, PAGE_SIZE, buddy::BuddyAllocator};
use std::cell::UnsafeCell;

/// Physical base of the fake arena. Deliberately not 0: a bug that produces a
/// null-ish address should look different from a valid one.
const ARENA_BASE: u64 = 0x10_0000;
const ARENA_LEN: u64 = 16 * 1024 * 1024;

/// Volatile link access like the kernel's `HhdmBacking`, rather than the
/// bounds-checked slicing the unit tests use: slicing would turn an
/// out-of-*region* write — a real allocator bug — into a clean panic that reads
/// as a harness failure, and the model would never get to catch it as an
/// overlap at the next `alloc`.
///
/// The arena-bounds assert below is a different check, and deliberate. Outside
/// the `Vec` there is nothing to write through at all, so that genuinely is a
/// harness fault and must say so with the offending address rather than let
/// ASan report it from inside the allocator.
///
/// Two other `VecBacking`s exist and differ on purpose — `crates/qunix-mm`'s
/// unit tests (bounds-checked slicing) and its benches (raw volatile, no
/// assert, because the assert dominated the measurement). Do not unify them.
/// Backed by `u64`, not `u8`: the accesses below are `u64` volatile reads and
/// writes, and a `Vec<u8>`'s buffer is only byte-aligned, so the cast would be
/// UB on a misaligned `pa` — exactly the input this target now generates.
struct VecBacking {
    mem: UnsafeCell<Vec<u64>>,
}

impl VecBacking {
    fn new() -> Self {
        Self { mem: UnsafeCell::new(vec![0u64; ARENA_LEN as usize / 8]) }
    }

    /// Byte offset of `pa`, or `None` if the whole 8-byte access does not fit.
    ///
    /// The end of the access is checked, not just its start: `pa` four bytes
    /// below the top passes a start-only check and then writes past the buffer,
    /// which ASan reports from an address that looks like an allocator bug.
    fn offset(pa: u64) -> Option<usize> {
        let end = pa.checked_add(8)?;
        (pa >= ARENA_BASE && end <= ARENA_BASE + ARENA_LEN && pa.is_multiple_of(8))
            .then(|| (pa - ARENA_BASE) as usize)
    }
}

impl FrameBacking for VecBacking {
    unsafe fn read_link(&self, pa: u64) -> u64 {
        let off = VecBacking::offset(pa).unwrap_or_else(|| {
            panic!("read_link outside the arena, or misaligned, at {pa:#x}")
        });
        let mem = unsafe { &*self.mem.get() };
        unsafe { mem.as_ptr().cast::<u8>().add(off).cast::<u64>().read_volatile() }
    }

    unsafe fn write_link(&self, pa: u64, value: u64) {
        let off = VecBacking::offset(pa).unwrap_or_else(|| {
            panic!("write_link outside the arena, or misaligned, at {pa:#x}")
        });
        let mem = unsafe { &mut *self.mem.get() };
        unsafe {
            mem.as_mut_ptr().cast::<u8>().add(off).cast::<u64>().write_volatile(value)
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
    ///
    /// `reclaim` decides whether the block is immediately allocated back. It
    /// exists because the arm unconditionally re-allocated, which meant `live`
    /// could never shrink: every free was followed by a matching alloc, so the
    /// allocator never held more than one adjacent free block and multi-level
    /// coalescing -- the part with the interesting arithmetic -- was
    /// unreachable by construction, in the same way the address-rounding did
    /// for misaligned frees.
    ///
    /// Adding the field changed how `Arbitrary` consumes bytes, so every corpus
    /// entry written before it decodes to a different sequence of ops. They
    /// still load and still run; what they lose is the minimisation and the
    /// coverage the fuzzer had accumulated for them, so the search effectively
    /// restarts from those inputs as fresh seeds rather than from where it was.
    Free { which: u16, reclaim: bool },
    /// Frees an address the allocator never handed out. This is a *supported*
    /// path — `free` is documented to reject foreign addresses rather than
    /// corrupt itself — so the assertion is that the counter moves and the
    /// heap stays consistent, not that nothing happens.
    FreeForeign { offset: u32, order: u8 },
}

/// Checks a block the allocator just handed out, then admits it to the model.
///
/// Extracted because the `Free` arm re-allocates and used to push the result
/// straight into `live` with none of these checks -- which is the one `alloc`
/// the module doc says will catch a list corrupted by `free`. A block that
/// overlapped would have been admitted silently and every later check would
/// then run against a poisoned model.
fn admit(pa: u64, order: u8, regions: &[(u64, u64)], live: &mut Vec<(u64, u8)>) {
    let size = PAGE_SIZE << order;
    assert_eq!(pa % size, 0, "alloc({order}) returned {pa:#x}, not {size}-aligned");
    assert!(
        regions.iter().any(|&(s, e)| pa >= s && pa + size <= e),
        "alloc({order}) returned {pa:#x}..{:#x}, outside every added region",
        pa + size
    );
    for &(other, other_order) in live.iter() {
        let other_size = PAGE_SIZE << other_order;
        assert!(
            pa + size <= other || other + other_size <= pa,
            "alloc({order}) returned {pa:#x}..{:#x}, overlapping live {other:#x}..{:#x}",
            pa + size,
            other + other_size
        );
    }
    live.push((pa, order));
}

fuzz_target!(|ops: Vec<Op>| {
    let mut buddy = BuddyAllocator::new(VecBacking::new());
    // (pa, order) for every block currently handed out.
    let mut live: Vec<(u64, u8)> = Vec::new();
    // Regions the model believes were accepted, as (start, end).
    let mut regions: Vec<(u64, u64)> = Vec::new();
    // Usable bytes the allocator actually took, summed from its own deltas.
    // `free_bytes <= sum of region lengths` is far too slack to catch anything:
    // it holds for an allocator that loses half its memory to a bad coalesce.
    let mut usable: u64 = 0;

    for op in ops {
        match op {
            Op::AddRegion { offset, len } => {
                // Placed into a free gap rather than discarded on overlap. With
                // multi-megabyte regions dropped on collision, a run reached
                // three or four regions against MAX_REGIONS = 128, so the
                // refusal path and `region_of`'s multi-region scan were both
                // unreachable and every colliding mutation was wasted.
                let len = (len as u64 % (ARENA_LEN / 32)).max(PAGE_SIZE);
                let mut sorted = regions.clone();
                sorted.sort_unstable();
                let mut gaps: Vec<(u64, u64)> = Vec::new();
                let mut cursor = ARENA_BASE;
                for &(rs, re) in &sorted {
                    if rs > cursor {
                        gaps.push((cursor, rs));
                    }
                    cursor = cursor.max(re);
                }
                if cursor < ARENA_BASE + ARENA_LEN {
                    gaps.push((cursor, ARENA_BASE + ARENA_LEN));
                }
                if gaps.is_empty() {
                    continue;
                }
                let (gap_start, gap_end) = gaps[offset as usize % gaps.len()];
                let start = gap_start + (offset as u64 % (gap_end - gap_start));
                let end = (start + len).min(gap_end);
                if end <= start {
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
                    usable += buddy.free_bytes() - before;
                }
            }

            Op::Alloc { order } => {
                let order = order % (MAX_ORDER + 1);
                let before = buddy.free_bytes();
                let Some(pa) = buddy.alloc(order) else { continue };
                let size = PAGE_SIZE << order;
                assert_eq!(
                    buddy.free_bytes(),
                    before - size,
                    "alloc({order}) removed the wrong number of bytes"
                );

                admit(pa, order, &regions, &mut live);
            }

            Op::Free { which, reclaim } => {
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
                // Negative direction: a just-freed block sits at some order >=
                // `order`, and alloc splits downward, so this must succeed.
                //
                // Conditional, so the free set can actually grow. Doing it every
                // time kept `live` at a fixed size and hid every coalescing path
                // past the first level. The cost of making it conditional is that
                // this is now only half the guard it was: an allocator that
                // regressed to always returning `None` is caught here on the
                // `reclaim == true` ops, and by the end-of-run `alloc(0)` check,
                // but not by the `reclaim == false` half, which asserts nothing
                // about allocation succeeding.
                if reclaim {
                    let again = buddy.alloc(order).unwrap_or_else(|| {
                        panic!("alloc({order}) failed right after free({pa:#x})")
                    });
                    admit(again, order, &regions, &mut live);
                }
            }

            Op::FreeForeign { offset, order } => {
                if regions.is_empty() {
                    continue;
                }
                let order = order % (MAX_ORDER + 1);
                let size = PAGE_SIZE << order;
                // Strided by the arena, not by `size`: for order >= 12 the
                // block is at least the whole 16 MiB arena, so dividing by
                // `size` collapsed every offset to a single address and
                // libFuzzer stopped mutating it. Page-aligned rather than
                // block-aligned so misaligned frees -- which the allocator must
                // also refuse -- are reachable at all.
                let stride = size.min(ARENA_LEN / 8).max(PAGE_SIZE);
                let pa = ARENA_BASE + (offset as u64 % ARENA_LEN) / stride * stride;
                // Only interesting if it really is foreign: inside no region,
                // and not a live block.
                // Refused by the allocator unless it is a properly aligned
                // block wholly inside a region. Misalignment is a rejection
                // reason in its own right, so those stay in scope here.
                let owned = regions.iter().any(|&(s, e)| pa >= s && pa + size <= e)
                    && pa.is_multiple_of(size);
                if owned {
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

    // Exact against the allocator's own reported deltas, which is stronger
    // than the `free_bytes <= total` bound it replaced -- that held for an
    // allocator losing half its memory. It is not fully independent: `usable`
    // is accumulated from `free_bytes` itself, so an `add_region` that
    // under-reported capacity from the outset would move both sides together.
    // Catching that needs `usable` computed from the region geometry instead.
    let live_bytes: u64 = live.iter().map(|&(_, o)| PAGE_SIZE << o).sum();
    assert_eq!(
        buddy.free_bytes(),
        usable - live_bytes,
        "free_bytes drifted: usable {usable} - live {live_bytes}"
    );
    // A run that refused every allocation asserts nothing above and would
    // otherwise pass silently, which is the failure mode this codebase keeps
    // hitting. Splitting always reaches order 0, so this holds for any correct
    // allocator with a page to spare.
    if buddy.free_bytes() >= PAGE_SIZE {
        assert!(
            buddy.alloc(0).is_some(),
            "free_bytes {} but alloc(0) refused",
            buddy.free_bytes()
        );
    }
});
