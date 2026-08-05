use core::alloc::Layout;

/// Size classes at 3/2 spacing rather than pure powers of two.
///
/// Doubling caps worst-case internal waste at 49% (a 1025-byte request
/// consuming 2048); interleaving the 1.5x steps caps it at 33%. The cost is
/// eight more free-list heads (64 bytes) and one extra comparison in
/// `class_for`, which stays branch-light and division-free.
const CLASSES: [usize; 17] = [
    8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048,
];

/// Smallest extent the large-block lists track. Nothing smaller can occur: an
/// allocation only reaches this path by exceeding the largest size class, or by
/// demanding `align > MAX_CLASS_ALIGN` (16), and an alignment is a power of two,
/// so the next one up is 32 — which `large_extent` then folds into the extent.
const LARGE_MIN: usize = 32;
const LARGE_MIN_SHIFT: u32 = LARGE_MIN.trailing_zeros();
/// 32 B .. 4 MiB, which covers every allocation the bump region can serve.
const LARGE_LISTS: usize = 18;

/// Reserved extent for a request the size classes cannot serve.
///
/// Both size and alignment are folded into one power of two, and blocks are
/// always bump-allocated at that alignment. A recycled block therefore
/// satisfies any later request with the same extent, whatever mix of size and
/// alignment produced it.
fn large_extent(layout: Layout) -> usize {
    layout
        .size()
        .next_power_of_two()
        .max(layout.align())
        .max(LARGE_MIN)
}

fn large_index(extent: usize) -> Option<usize> {
    let idx = (extent.trailing_zeros() - LARGE_MIN_SHIFT) as usize;
    (idx < LARGE_LISTS).then_some(idx)
}

/// The strongest alignment a size class will serve; anything stricter falls
/// through to the bump region.
const MAX_CLASS_ALIGN: usize = 16;

/// A size-class heap with intrusive free lists, falling back to a bump region
/// for anything larger than the biggest class.
///
/// Despite the name this is not a slab allocator: there are no object caches,
/// no constructors, and no per-CPU magazines. It is a segregated size-class
/// free-list allocator with a bump fallback — the M0 placeholder that a real
/// slab layer will replace behind the same type name.
///
/// Both paths recycle. Requests that map to a size class use the per-class free
/// lists; everything else (larger than 2048 bytes, or aligned more strictly
/// than 16) is rounded to a power-of-two extent and tracked in a parallel set
/// of large-block lists. Only extents above 4 MiB are still leaked.
pub struct SlabHeap {
    free_lists: [*mut u8; CLASSES.len()],
    /// Free lists for classless allocations, indexed by power-of-two extent.
    /// Without these the bump region never recycles, so a 16 MiB non-growing
    /// heap is exhausted by roughly 2x the peak size of every large object
    /// ever allocated -- eight 1 MiB buffers would do it.
    large_lists: [*mut u8; LARGE_LISTS],
    bump_next: usize,
    bump_end: usize,
    allocated: usize,
}

// The heap is only ever reached through a lock, and the raw pointers it holds
// refer to memory it exclusively owns.
unsafe impl Send for SlabHeap {}

impl SlabHeap {
    pub const fn new() -> Self {
        Self {
            free_lists: [core::ptr::null_mut(); CLASSES.len()],
            large_lists: [core::ptr::null_mut(); LARGE_LISTS],
            bump_next: 0,
            bump_end: 0,
            allocated: 0,
        }
    }

    /// Gives the heap the one contiguous, mapped, writable region it manages.
    ///
    /// This replaces any previous region rather than extending the heap, so a
    /// second call would orphan everything handed out from the first.
    ///
    /// # Safety
    /// `va..va + len` must be mapped, writable, owned exclusively by the heap,
    /// and must not wrap the address space. Must be called at most once,
    /// before any allocation.
    ///
    /// # Panics
    /// Both requirements are asserted rather than assumed, so violating them
    /// aborts rather than being undefined behaviour. A wrapping range was
    /// previously absorbed by taking nothing, which left the heap permanently
    /// empty and surfaced much later as an unrelated allocation failure.
    pub unsafe fn set_backing(&mut self, va: usize, len: usize) {
        // The "at most once" rule above is load-bearing, so it is enforced
        // rather than merely documented: a second call orphans every block
        // handed out from the first while leaving the free lists pointing into
        // it. `bump_end` catches a repeat call even when nothing was allocated.
        assert!(self.bump_end == 0 && self.allocated == 0, "backing replaced after allocations");
        // Asserted rather than absorbed, matching the sibling check above. A
        // wrapping range is a caller error under this function's `# Safety`
        // clause, and taking nothing instead left the heap permanently empty --
        // surfacing much later as an allocation failure with no connection to
        // its cause.
        let end = va.checked_add(len).expect("heap backing wraps the address space");
        self.bump_next = va;
        self.bump_end = end;
    }

    pub fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    /// Bytes left in the bump region.
    ///
    /// Exposed because bump exhaustion is how this heap actually dies, and
    /// `allocated_bytes` cannot predict it: alignment padding is consumed but
    /// never credited, so the counter systematically understates consumption.
    pub fn bump_remaining(&self) -> usize {
        self.bump_end.saturating_sub(self.bump_next)
    }

    fn class_for(layout: Layout) -> Option<usize> {
        if layout.align() > MAX_CLASS_ALIGN || layout.size() > CLASSES[CLASSES.len() - 1] {
            return None;
        }
        // Classes come in pairs (2^k, 3*2^(k-2)) with 8 as the lone smallest,
        // so the index is derived from the exponent and one comparison against
        // the 3/4 boundary -- no scan, no division.
        let size = layout.size().max(CLASSES[0]);
        let k = size.next_power_of_two().trailing_zeros() as usize;
        let three_quarter = 3usize << (k - 2);
        Some(if k >= 4 && size <= three_quarter { 2 * (k - 3) - 1 } else { 2 * (k - 3) })
    }

    fn bump(&mut self, size: usize, align: usize) -> *mut u8 {
        // `GlobalAlloc::alloc` must never panic, so the rounding-up step is as
        // guarded as the size step below it.
        let Some(start) = self.bump_next.checked_add(align - 1).map(|v| v & !(align - 1)) else {
            return core::ptr::null_mut();
        };
        let Some(end) = start.checked_add(size) else {
            return core::ptr::null_mut();
        };
        if end > self.bump_end {
            return core::ptr::null_mut();
        }
        self.bump_next = end;
        start as *mut u8
    }

    /// Allocates a block, or returns null when the heap cannot satisfy it.
    ///
    /// Safe, deliberately. There is nothing *this call* asks of the caller: the
    /// free-list reads below are sound as long as the heap's invariants hold,
    /// and those are established by [`set_backing`](Self::set_backing) and
    /// preserved by [`dealloc`](Self::dealloc). Both are `unsafe`, which is
    /// where the obligations are stated — the obligation is transferred, not
    /// absent, and breaking either poisons a list this safe `alloc` will read.
    /// Before `set_backing`, `bump_end` is zero and every path returns null
    /// rather than touching memory
    /// (`alloc_before_set_backing_returns_null_on_every_path`).
    ///
    /// A zero-size layout is accepted, not rejected: `class_for` floors the
    /// size at `CLASSES[0]`, so it is served an 8-byte block, and `dealloc`
    /// classifies it identically, so it round-trips.
    ///
    /// Returning a raw pointer is not itself unsafe; *dereferencing* it is, and
    /// that is the caller's act. Marking this `unsafe` implied an obligation at
    /// this call site that the caller could neither discover nor discharge,
    /// which devalues the marker on the calls that genuinely carry one.
    pub fn alloc(&mut self, layout: Layout) -> *mut u8 {
        match Self::class_for(layout) {
            Some(class) => {
                let size = CLASSES[class];
                let head = self.free_lists[class];
                let ptr = if head.is_null() {
                    // Every block of a class lands on one shared free list, so
                    // a block must satisfy the strongest alignment that class
                    // can ever be asked for, not just the alignment of the
                    // request that happened to create it. `class_for` caps that
                    // at `MAX_CLASS_ALIGN`.
                    self.bump(size.max(MAX_CLASS_ALIGN), MAX_CLASS_ALIGN)
                } else {
                    self.free_lists[class] = unsafe { *(head as *mut *mut u8) };
                    head
                };
                if !ptr.is_null() {
                    self.allocated += size;
                }
                ptr
            }
            None => {
                let extent = large_extent(layout);
                // Reuse before bumping: the bump region is finite and never
                // reclaimed once handed out.
                if let Some(idx) = large_index(extent) {
                    let head = self.large_lists[idx];
                    if !head.is_null() {
                        self.large_lists[idx] = unsafe { *(head as *mut *mut u8) };
                        self.allocated += extent;
                        return head;
                    }
                }
                let ptr = self.bump(extent, extent);
                if !ptr.is_null() {
                    self.allocated += extent;
                }
                ptr
            }
        }
    }

    /// Returns a block to a size-class list or a large-block list.
    ///
    /// One residual leak remains: a block whose power-of-two extent exceeds the
    /// largest `LARGE_LISTS` entry (4 MiB) has nowhere to go, so its accounting
    /// is reversed but the memory stays consumed. Everything else — every size
    /// class, and every classless block up to 4 MiB — is recycled.
    ///
    /// # Safety
    /// `ptr` and `layout` must match a previous successful `alloc`.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        match Self::class_for(layout) {
            Some(class) => {
                unsafe { *(ptr as *mut *mut u8) = self.free_lists[class] };
                self.free_lists[class] = ptr;
                self.allocated -= CLASSES[class];
            }
            None => {
                let extent = large_extent(layout);
                self.allocated -= extent;
                // Extents beyond the largest list stay leaked; everything the
                // bump region can realistically serve is covered.
                if let Some(idx) = large_index(extent) {
                    unsafe { *(ptr as *mut *mut u8) = self.large_lists[idx] };
                    self.large_lists[idx] = ptr;
                }
            }
        }
    }
}

impl Default for SlabHeap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gives the heap a real, correctly aligned host allocation to manage.
    fn heap_with(bytes: usize) -> (SlabHeap, Box<[u8]>) {
        let backing = vec![0u8; bytes + 4096].into_boxed_slice();
        let raw = backing.as_ptr() as usize;
        let aligned = (raw + 4095) & !4095;
        let mut heap = SlabHeap::new();
        unsafe { heap.set_backing(aligned, bytes) };
        (heap, backing)
    }

    /// A backing range that wraps the address space must abort rather than be
    /// absorbed.
    ///
    /// Absorbing it by taking nothing left the heap permanently empty and
    /// surfaced much later as an allocation failure with no connection to its
    /// cause, which is the regression this pins.
    #[test]
    #[should_panic(expected = "wraps the address space")]
    fn set_backing_refuses_a_wrapping_range() {
        let mut heap = SlabHeap::new();
        unsafe { heap.set_backing(usize::MAX - 16, 4096) };
    }

    /// A second `set_backing` must abort.
    ///
    /// The "at most once" rule is enforced rather than documented because a
    /// repeat call orphans every block handed out from the first region while
    /// leaving the free lists pointing into it — the next `alloc` of a recycled
    /// class then returns a pointer into memory the heap no longer manages.
    /// `bump_end` is what catches the case where nothing was allocated yet, so
    /// the no-allocation variant is the one asserted here.
    #[test]
    #[should_panic(expected = "backing replaced after allocations")]
    fn set_backing_refuses_a_second_call() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let mut second = std::vec![0u8; 64 * 1024];
        let va = second.as_mut_ptr() as usize;
        unsafe { heap.set_backing(va, 64 * 1024) };
    }

    /// Rounding `bump_next` up to a very strong alignment must not overflow.
    ///
    /// `GlobalAlloc::alloc` may not panic, and overflow checks are on for this
    /// crate in the dev profile, so an unchecked `bump_next + (align - 1)`
    /// aborts the kernel on a request a caller is entitled to make and be
    /// refused. The heap is placed at the top of the address space, which is
    /// what makes the rounding overflow rather than merely exceed `bump_end`.
    #[test]
    fn bump_refuses_an_alignment_that_overflows_the_address_space() {
        let mut heap = SlabHeap::new();
        // Never dereferenced: every path below returns null.
        unsafe { heap.set_backing(usize::MAX - 4096, 4096) };
        let layout = Layout::from_size_align(8, 1 << 62).unwrap();
        assert!(heap.alloc(layout).is_null(), "an unsatisfiable alignment was served");
        assert_eq!(heap.allocated_bytes(), 0, "a refused alloc was charged");
    }

    /// The same for `start + size`, which is a separate `checked_add`.
    ///
    /// An alignment the heap can round to, and a size that then runs off the end
    /// of the address space rather than merely off the end of the heap.
    #[test]
    fn bump_refuses_a_size_that_overflows_the_address_space() {
        let mut heap = SlabHeap::new();
        unsafe { heap.set_backing(usize::MAX - 4096, 4096) };
        let layout = Layout::from_size_align(1 << 62, 8).unwrap();
        assert!(heap.alloc(layout).is_null(), "an unsatisfiable size was served");
        assert_eq!(heap.allocated_bytes(), 0, "a refused alloc was charged");
    }

    /// A recycled size-class block must still satisfy the strongest alignment
    /// that class can be asked for, not the alignment of the request that
    /// created it.
    ///
    /// Every block of a class lands on one shared free list, so a block carved
    /// at align 1 for an `align(1)` request would later be handed to an
    /// `align(16)` request. `alloc` bumps class blocks at `MAX_CLASS_ALIGN`
    /// precisely to stop that, and nothing asserted it: the existing alignment
    /// test allocates from a fresh bump region whose base is page aligned, so it
    /// passes whatever alignment the bump used.
    #[test]
    fn a_recycled_class_block_still_meets_the_strongest_class_alignment() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let weak = Layout::from_size_align(8, 1).unwrap();
        let strong = Layout::from_size_align(8, MAX_CLASS_ALIGN).unwrap();
        assert_eq!(
            SlabHeap::class_for(weak),
            SlabHeap::class_for(strong),
            "both requests must share a class, or the free list is not shared"
        );

        // Several blocks, so at least one lands where a weaker bump stride would
        // have left an odd address.
        let mut carved = Vec::new();
        for _ in 0..4 {
            let p = heap.alloc(weak);
            assert!(!p.is_null());
            carved.push(p);
        }
        for p in carved {
            unsafe { heap.dealloc(p, weak) };
        }
        for _ in 0..4 {
            let p = heap.alloc(strong);
            assert!(!p.is_null());
            assert_eq!(
                p as usize % MAX_CLASS_ALIGN,
                0,
                "a recycled block did not meet the class's strongest alignment"
            );
        }
    }

    /// A large request the bump region cannot satisfy must be refused without
    /// charging it, and must leave the heap able to serve the next request.
    ///
    /// `allocated_bytes` is what the kernel reads to decide how much heap it is
    /// using; charging a failed allocation inflates it permanently, and nothing
    /// ever subtracts it because there is no pointer to `dealloc`.
    #[test]
    fn a_failed_large_allocation_is_not_charged_and_leaves_the_heap_usable() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let small = Layout::from_size_align(64, 8).unwrap();
        let live = heap.alloc(small);
        assert!(!live.is_null());
        let charged = heap.allocated_bytes();
        let remaining = heap.bump_remaining();

        // A megabyte out of a 64 KiB heap: classless by size, and hopeless.
        let huge = Layout::from_size_align(1 << 20, 8).unwrap();
        assert!(heap.alloc(huge).is_null(), "the heap served more than it has");

        assert_eq!(heap.allocated_bytes(), charged, "a failed allocation was charged");
        assert_eq!(heap.bump_remaining(), remaining, "a failed allocation consumed bump space");
        assert!(!heap.alloc(small).is_null(), "a failed allocation broke the heap");
    }

    /// A recycled large block must only be reused for its own extent.
    ///
    /// The large lists are indexed by power-of-two extent precisely so that a
    /// freed 64-byte extent cannot satisfy a 128-byte one. Nothing asserted the
    /// refusal — every large test frees and re-requests the same layout — and
    /// getting it wrong hands back a block half the size of the request, which
    /// the caller then writes past the end of.
    #[test]
    fn a_large_block_is_not_recycled_for_a_different_extent() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        // Classless via alignment, so both take the large path.
        let small = Layout::from_size_align(32, 64).unwrap();
        let large = Layout::from_size_align(32, 128).unwrap();
        assert!(SlabHeap::class_for(small).is_none() && SlabHeap::class_for(large).is_none());
        assert_ne!(
            large_extent(small),
            large_extent(large),
            "the two layouts must land in different large lists"
        );

        let freed = heap.alloc(small);
        assert!(!freed.is_null());
        unsafe { heap.dealloc(freed, small) };

        let bigger = heap.alloc(large);
        assert!(!bigger.is_null());
        assert_ne!(bigger, freed, "a 64-byte extent was handed out for a 128-byte request");
        assert_eq!(bigger as usize % 128, 0, "the large block is not aligned to its extent");
        // And the freed block is still there for a request of its own extent.
        assert_eq!(heap.alloc(small), freed, "the 64-byte extent stopped being recycled");
    }

    #[test]
    fn a_zero_size_layout_round_trips_through_the_smallest_class() {
        // The doc on `alloc` claims a zero-size request is served an 8-byte
        // block and that `dealloc` classifies it identically. Only the
        // no-backing path was covered, so neither half was pinned.
        let mut backing = std::vec![0u8; 64 * 1024];
        let base = backing.as_mut_ptr() as usize;
        let mut heap = SlabHeap::new();
        unsafe { heap.set_backing(base, 64 * 1024) };

        let layout = Layout::from_size_align(0, 1).unwrap();
        let first = heap.alloc(layout);
        assert!(!first.is_null(), "a zero-size request was refused");
        assert_eq!(heap.allocated_bytes(), 8, "a zero-size request was not charged one class");
        unsafe { heap.dealloc(first, layout) };
        assert_eq!(heap.allocated_bytes(), 0, "a zero-size block did not round-trip");
        // And it comes back from the free list rather than the bump region.
        assert_eq!(heap.alloc(layout), first, "the recycled block was not reused");
    }

    #[test]
    fn alloc_before_set_backing_returns_null_on_every_path() {
        let mut heap = SlabHeap::new();
        for (size, align) in [
            (8usize, 1usize),      // smallest size class
            (2048, 16),            // largest size class, strongest class align
            (32, 4096),            // classless via alignment
            (1 << 20, 8),          // classless via size
            (8 << 20, 8),          // beyond the largest large-list entry
            (0, 1),                // zero-size
        ] {
            let layout = Layout::from_size_align(size, align).unwrap();
            assert!(
                heap.alloc(layout).is_null(),
                "alloc({size}, {align}) returned non-null with no backing"
            );
        }
        assert_eq!(heap.allocated_bytes(), 0, "a refused alloc was still charged");
    }

    #[test]
    fn small_allocation_honours_an_alignment_stricter_than_its_size() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(8, 16).unwrap();
        let ptr = heap.alloc(layout);
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);
    }

    #[test]
    fn overaligned_small_allocation_is_recycled() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        // 32 bytes fits a size class, but align 64 exceeds MAX_CLASS_ALIGN, so
        // this takes the large-block path in both directions.
        let layout = Layout::from_size_align(32, 64).unwrap();
        let first = heap.alloc(layout);
        assert!(!first.is_null());
        assert_eq!(first as usize % 64, 0);
        unsafe { heap.dealloc(first, layout) };
        assert_eq!(heap.allocated_bytes(), 0);
        let second = heap.alloc(layout);
        assert_eq!(first, second, "over-aligned block was not recycled");
    }

    #[test]
    fn large_allocation_is_recycled() {
        let (mut heap, _backing) = heap_with(256 * 1024);
        let layout = Layout::from_size_align(9000, 16).unwrap();
        let first = heap.alloc(layout);
        assert!(!first.is_null());
        unsafe { heap.dealloc(first, layout) };
        let second = heap.alloc(layout);
        assert_eq!(first, second, "large block was not recycled");
    }

    #[test]
    fn repeated_large_alloc_free_cycles_do_not_exhaust_the_heap() {
        // The regression this fixes: without recycling, each cycle consumed a
        // fresh extent and a 16 MiB heap died after a few hundred iterations.
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        for _ in 0..10_000 {
            let p = heap.alloc(layout);
            assert!(!p.is_null(), "heap exhausted despite every block being freed");
            unsafe { heap.dealloc(p, layout) };
        }
    }

    #[test]
    fn every_size_maps_to_the_smallest_sufficient_class() {
        // Pins the index arithmetic in `class_for`. A silent off-by-one there
        // would hand back a block too small for the request.
        // Every alignment a size class is allowed to serve, since `class_for`
        // maps on size alone once the alignment is within `MAX_CLASS_ALIGN`.
        for align in [1usize, 2, 4, 8, 16] {
            for size in 1..=CLASSES[CLASSES.len() - 1] {
                let layout = Layout::from_size_align(size, align).unwrap();
                let class = SlabHeap::class_for(layout).expect("size within class range");
                assert!(CLASSES[class] >= size, "class {} too small for {size}", CLASSES[class]);
                if class > 0 {
                    assert!(
                        CLASSES[class - 1] < size,
                        "size {size} should have fitted class {}",
                        CLASSES[class - 1]
                    );
                }
            }
        }
        // One step past `MAX_CLASS_ALIGN` leaves the class path however small
        // the request is.
        assert!(SlabHeap::class_for(Layout::from_size_align(8, 32).unwrap()).is_none());
    }

    #[test]
    fn large_index_covers_its_documented_range() {
        assert_eq!(large_index(LARGE_MIN), Some(0));
        assert_eq!(large_index(4 * 1024 * 1024), Some(LARGE_LISTS - 1));
        assert_eq!(large_index(8 * 1024 * 1024), None);
    }

    #[test]
    fn an_extent_beyond_the_largest_list_is_leaked() {
        // The one residual leak `dealloc` documents. Pinned so that raising
        // `LARGE_LISTS` has to come with a deliberate change here.
        let (mut heap, _backing) = heap_with(32 * 1024 * 1024);
        let layout = Layout::from_size_align(8 * 1024 * 1024, 8).unwrap();
        let first = heap.alloc(layout);
        assert!(!first.is_null());
        assert_eq!(heap.allocated_bytes(), 8 * 1024 * 1024);
        let remaining = heap.bump_remaining();
        unsafe { heap.dealloc(first, layout) };
        assert_eq!(heap.bump_remaining(), remaining, "bump region cannot reclaim");
        // The memory is leaked but the *accounting* is not: `dealloc` reverses
        // the charge on every path, or `allocated_bytes` drifts upward for the
        // life of the kernel and eventually underflows on an unrelated free.
        assert_eq!(heap.allocated_bytes(), 0, "a leaked extent kept its charge");
        let second = heap.alloc(layout);
        assert!(!second.is_null());
        assert_ne!(first, second, "an extent beyond the largest list was recycled");
    }

    #[test]
    fn requests_beyond_the_largest_class_take_the_large_path() {
        let layout = Layout::from_size_align(CLASSES[CLASSES.len() - 1] + 1, 8).unwrap();
        assert!(SlabHeap::class_for(layout).is_none());
    }

    #[test]
    fn small_allocation_is_correctly_aligned_and_writable() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(24, 8).unwrap();
        let ptr = heap.alloc(layout);
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 8, 0);
        unsafe { core::ptr::write_bytes(ptr, 0xAB, 24) };
        assert_eq!(unsafe { *ptr }, 0xAB);
    }

    #[test]
    fn allocations_of_the_same_class_do_not_overlap() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(32, 8).unwrap();
        let a = heap.alloc(layout);
        let b = heap.alloc(layout);
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        assert!((a as isize - b as isize).unsigned_abs() >= 32);
    }

    #[test]
    fn freed_block_is_reused_by_the_next_same_sized_allocation() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let first = heap.alloc(layout);
        unsafe { heap.dealloc(first, layout) };
        let second = heap.alloc(layout);
        assert_eq!(first, second, "freed block was not reused");
    }

    #[test]
    fn large_allocation_falls_back_and_still_succeeds() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(9000, 16).unwrap();
        let ptr = heap.alloc(layout);
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);
    }

    #[test]
    fn exhaustion_returns_null_rather_than_panicking() {
        let (mut heap, _backing) = heap_with(8 * 1024);
        let layout = Layout::from_size_align(2048, 8).unwrap();
        let mut succeeded = 0;
        for _ in 0..64 {
            if !heap.alloc(layout).is_null() {
                succeeded += 1;
            }
        }
        assert!(succeeded > 0, "heap allocated nothing at all");
        assert!(succeeded < 64, "heap never reported exhaustion");
    }

    #[test]
    fn allocated_bytes_tracks_outstanding_allocations() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(64, 8).unwrap();
        assert_eq!(heap.allocated_bytes(), 0);
        let ptr = heap.alloc(layout);
        assert_eq!(heap.allocated_bytes(), 64);
        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(heap.allocated_bytes(), 0);
    }
}
