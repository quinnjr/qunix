#![no_main]
//! Structural fuzzing of the slab heap.
//!
//! As with the buddy target, the interesting failure is silent aliasing rather
//! than a crash: a size-class or recycling bug hands the same block to two
//! callers and nothing faults. So every live allocation's `[ptr, ptr + size)`
//! is tracked and checked for overlap after each `alloc`.
//!
//! The heap writes its free-list links *into* freed blocks, so the arena here
//! is real, owned, writable memory — see `ARENA` — not a fake address range.
//! Accounting is asserted exactly, against the extent the heap actually
//! charged, because every inequality this file used to assert was slack enough
//! to hold for an allocator that was losing memory.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use qunix_mm::slab::SlabHeap;
use std::alloc::Layout;
use std::cell::UnsafeCell;
use std::sync::OnceLock;

const ARENA_LEN: usize = 4 * 1024 * 1024;

/// One arena for the life of the process, reused by every run.
///
/// It cannot be per-run: the heap hands out interior pointers, so the memory
/// must outlive every allocation, and leaking a fresh 4 MiB per run both grows
/// without bound over millions of runs and is reported by LeakSanitizer. Held
/// in a `static` so it stays reachable, which is what keeps LSan quiet --
/// `Vec::leak` does not.
struct Arena(UnsafeCell<Vec<u8>>);
// libFuzzer drives one run at a time on one thread.
unsafe impl Sync for Arena {}

static ARENA: OnceLock<Arena> = OnceLock::new();

#[derive(Arbitrary, Debug)]
enum Op {
    Alloc { size: u32, align_shift: u8 },
    Dealloc { which: u16 },
}

fuzz_target!(|ops: Vec<Op>| {
    let arena = ARENA.get_or_init(|| Arena(UnsafeCell::new(vec![0u8; ARENA_LEN])));
    // Contents carry over between runs, which is harmless: the heap treats the
    // region as raw memory and writes its own links before reading any.
    let base = unsafe { (*arena.0.get()).as_mut_ptr() } as usize;

    let mut heap = SlabHeap::new();
    unsafe { heap.set_backing(base, ARENA_LEN) };

    // Poisoned each run so a stale free-list link left by a previous run reads
    // as garbage rather than as a plausible pointer into the same arena.
    unsafe { std::ptr::write_bytes(base as *mut u8, 0x5A, ARENA_LEN) };

    // Anchors the run. Without it, a `set_backing` that silently took nothing
    // would make every op below a no-op and the run would still pass.
    let probe_layout = Layout::from_size_align(16, 16).unwrap();
    let probe = heap.alloc(probe_layout);
    assert!(!probe.is_null(), "a fresh arena refused a 16-byte allocation");
    unsafe { heap.dealloc(probe, probe_layout) };

    // (ptr, layout, charged) for every block currently handed out. `charged` is
    // what the heap actually billed -- a class or power-of-two extent, always
    // >= layout.size() -- so accounting can be asserted exactly rather than as
    // an inequality that is permanently slack by the rounding on every block.
    let mut live: Vec<(*mut u8, Layout, usize)> = Vec::new();

    for op in ops {
        match op {
            Op::Alloc { size, align_shift } => {
                // Bounded well below the arena so exhaustion is reachable but
                // not immediate; alignment capped at 4 KiB, the largest the
                // kernel ever asks for.
                // Up to twice the arena, so exhaustion and the >4 MiB
                // never-recycled branch are both reachable; a 64 KiB cap made
                // the large-block leak path unreachable by construction.
                let size = (size as usize % (ARENA_LEN * 2)).max(1);
                let align = 1usize << (align_shift % 13);
                let Ok(layout) = Layout::from_size_align(size, align) else { continue };

                let before = heap.allocated_bytes();
                let ptr = heap.alloc(layout);
                if ptr.is_null() {
                    // Exhaustion is a legitimate answer, not a failure.
                    continue;
                }
                let addr = ptr as usize;

                assert_eq!(addr % align, 0, "alloc({size}, {align}) returned {addr:#x}, misaligned");
                assert!(
                    addr >= base && addr + size <= base + ARENA_LEN,
                    "alloc({size}, {align}) returned {addr:#x}..{:#x}, outside the arena",
                    addr + size
                );
                let charged = heap.allocated_bytes() - before;
                assert!(charged >= size, "charged {charged} for a {size}-byte request");
                // Overlap is checked over the *reserved* extent, not the
                // requested size: two blocks whose reserved tails overlap but
                // whose requested prefixes do not would otherwise pass, and a
                // size-class off-by-one presents exactly that way.
                for &(other, _, other_charged) in &live {
                    let other_addr = other as usize;
                    assert!(
                        addr + charged <= other_addr || other_addr + other_charged <= addr,
                        "alloc({size}, {align}) reserved {addr:#x}..{:#x}, overlapping live \
                         {other_addr:#x}..{:#x}",
                        addr + charged,
                        other_addr + other_charged
                    );
                }

                // Puts the block's full extent under ASan, so one that runs
                // past the arena or past its own size class faults here.
                // Aliasing itself is caught by the model check above, which does
                // not depend on this write.
                unsafe { std::ptr::write_bytes(ptr, 0xAB, size) };

                live.push((ptr, layout, charged));
            }

            Op::Dealloc { which } => {
                if live.is_empty() {
                    continue;
                }
                let (ptr, layout, charged) = live.swap_remove(which as usize % live.len());
                let before = heap.allocated_bytes();
                unsafe { heap.dealloc(ptr, layout) };
                // Exact: `< before` passed when a 64 KiB block was credited
                // back 8 bytes.
                assert_eq!(
                    heap.allocated_bytes(),
                    before - charged,
                    "dealloc credited back the wrong amount for a {}-byte request",
                    layout.size()
                );
            }
        }
    }

    // Drain everything and require the counter to land exactly on zero. This is
    // the invariant the previous `>=` was reaching for and could not express.
    for (ptr, layout, _) in live.drain(..) {
        unsafe { heap.dealloc(ptr, layout) };
    }
    assert_eq!(
        heap.allocated_bytes(),
        0,
        "bytes still charged after every block was freed"
    );
});
