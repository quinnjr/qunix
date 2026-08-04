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
    /// `va..va + len` must be mapped, writable, and owned exclusively by the
    /// heap. Must be called at most once, before any allocation.
    pub unsafe fn set_backing(&mut self, va: usize, len: usize) {
        // The "at most once" rule above is load-bearing, so it is enforced
        // rather than merely documented: a second call orphans every block
        // handed out from the first while leaving the free lists pointing into
        // it. `bump_end` catches a repeat call even when nothing was allocated.
        assert!(self.bump_end == 0 && self.allocated == 0, "backing replaced after allocations");
        match va.checked_add(len) {
            Some(end) => {
                self.bump_next = va;
                self.bump_end = end;
            }
            // A region that wraps the address space cannot be real; take
            // nothing rather than hand out wrapped addresses.
            None => {
                self.bump_next = 0;
                self.bump_end = 0;
            }
        }
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

    /// # Safety
    /// Standard `GlobalAlloc::alloc` contract.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
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

    #[test]
    fn small_allocation_honours_an_alignment_stricter_than_its_size() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(8, 16).unwrap();
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);
    }

    #[test]
    fn overaligned_small_allocation_is_recycled() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        // 32 bytes fits a size class, but align 64 exceeds MAX_CLASS_ALIGN, so
        // this takes the large-block path in both directions.
        let layout = Layout::from_size_align(32, 64).unwrap();
        let first = unsafe { heap.alloc(layout) };
        assert!(!first.is_null());
        assert_eq!(first as usize % 64, 0);
        unsafe { heap.dealloc(first, layout) };
        assert_eq!(heap.allocated_bytes(), 0);
        let second = unsafe { heap.alloc(layout) };
        assert_eq!(first, second, "over-aligned block was not recycled");
    }

    #[test]
    fn large_allocation_is_recycled() {
        let (mut heap, _backing) = heap_with(256 * 1024);
        let layout = Layout::from_size_align(9000, 16).unwrap();
        let first = unsafe { heap.alloc(layout) };
        assert!(!first.is_null());
        unsafe { heap.dealloc(first, layout) };
        let second = unsafe { heap.alloc(layout) };
        assert_eq!(first, second, "large block was not recycled");
    }

    #[test]
    fn repeated_large_alloc_free_cycles_do_not_exhaust_the_heap() {
        // The regression this fixes: without recycling, each cycle consumed a
        // fresh extent and a 16 MiB heap died after a few hundred iterations.
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        for _ in 0..10_000 {
            let p = unsafe { heap.alloc(layout) };
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
        let first = unsafe { heap.alloc(layout) };
        assert!(!first.is_null());
        let remaining = heap.bump_remaining();
        unsafe { heap.dealloc(first, layout) };
        assert_eq!(heap.bump_remaining(), remaining, "bump region cannot reclaim");
        let second = unsafe { heap.alloc(layout) };
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
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 8, 0);
        unsafe { core::ptr::write_bytes(ptr, 0xAB, 24) };
        assert_eq!(unsafe { *ptr }, 0xAB);
    }

    #[test]
    fn allocations_of_the_same_class_do_not_overlap() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(32, 8).unwrap();
        let a = unsafe { heap.alloc(layout) };
        let b = unsafe { heap.alloc(layout) };
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        assert!((a as isize - b as isize).unsigned_abs() >= 32);
    }

    #[test]
    fn freed_block_is_reused_by_the_next_same_sized_allocation() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let first = unsafe { heap.alloc(layout) };
        unsafe { heap.dealloc(first, layout) };
        let second = unsafe { heap.alloc(layout) };
        assert_eq!(first, second, "freed block was not reused");
    }

    #[test]
    fn large_allocation_falls_back_and_still_succeeds() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(9000, 16).unwrap();
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);
    }

    #[test]
    fn exhaustion_returns_null_rather_than_panicking() {
        let (mut heap, _backing) = heap_with(8 * 1024);
        let layout = Layout::from_size_align(2048, 8).unwrap();
        let mut succeeded = 0;
        for _ in 0..64 {
            if !unsafe { heap.alloc(layout) }.is_null() {
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
        let ptr = unsafe { heap.alloc(layout) };
        assert_eq!(heap.allocated_bytes(), 64);
        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(heap.allocated_bytes(), 0);
    }
}
