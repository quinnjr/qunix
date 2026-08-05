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

    // (ptr, layout) for every block currently handed out.
    let mut live: Vec<(*mut u8, Layout)> = Vec::new();

    for op in ops {
        match op {
            Op::Alloc { size, align_shift } => {
                // Bounded well below the arena so exhaustion is reachable but
                // not immediate; alignment capped at 4 KiB, the largest the
                // kernel ever asks for.
                let size = (size as usize % (64 * 1024)).max(1);
                let align = 1usize << (align_shift % 13);
                let Ok(layout) = Layout::from_size_align(size, align) else { continue };

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
                for &(other, other_layout) in &live {
                    let other_addr = other as usize;
                    assert!(
                        addr + size <= other_addr || other_addr + other_layout.size() <= addr,
                        "alloc({size}, {align}) returned {addr:#x}..{:#x}, overlapping live \
                         {other_addr:#x}..{:#x}",
                        addr + size,
                        other_addr + other_layout.size()
                    );
                }

                // Writing the whole block proves it is really owned and mapped.
                // A block that overlaps another allocation will corrupt it, and
                // the overlap check above will catch it on the next alloc.
                unsafe { std::ptr::write_bytes(ptr, 0xAB, size) };

                live.push((ptr, layout));
            }

            Op::Dealloc { which } => {
                if live.is_empty() {
                    continue;
                }
                let (ptr, layout) = live.swap_remove(which as usize % live.len());
                let before = heap.allocated_bytes();
                unsafe { heap.dealloc(ptr, layout) };
                assert!(
                    heap.allocated_bytes() < before,
                    "dealloc of {} bytes did not reduce allocated_bytes ({before})",
                    layout.size()
                );
            }
        }
    }

    // Everything still outstanding is accounted for.
    let outstanding: usize = live.iter().map(|(_, l)| l.size()).sum();
    assert!(
        heap.allocated_bytes() >= outstanding,
        "allocated_bytes {} is less than the {outstanding} bytes still live",
        heap.allocated_bytes()
    );
});
