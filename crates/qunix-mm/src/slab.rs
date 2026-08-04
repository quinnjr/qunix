use core::alloc::Layout;

const CLASSES: [usize; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];

/// A size-class heap with intrusive free lists, falling back to a bump region
/// for anything larger than the biggest class.
///
/// Deliberately simple: enough for M0/M1, with per-class caches and reclaim of
/// oversized blocks left to a later milestone.
pub struct SlabHeap {
    free_lists: [*mut u8; CLASSES.len()],
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
            bump_next: 0,
            bump_end: 0,
            allocated: 0,
        }
    }

    /// Hands the heap a contiguous, mapped, writable region to manage.
    ///
    /// # Safety
    /// `va..va + len` must be mapped, writable, and owned exclusively by the heap.
    pub unsafe fn add_backing(&mut self, va: usize, len: usize) {
        self.bump_next = va;
        self.bump_end = va + len;
    }

    pub fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    fn class_for(layout: Layout) -> Option<usize> {
        if layout.align() > 16 {
            return None;
        }
        CLASSES.iter().position(|&size| size >= layout.size())
    }

    fn bump(&mut self, size: usize, align: usize) -> *mut u8 {
        let start = (self.bump_next + align - 1) & !(align - 1);
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
                    self.bump(size, size.min(16))
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
                let ptr = self.bump(layout.size(), layout.align().max(16));
                if !ptr.is_null() {
                    self.allocated += layout.size();
                }
                ptr
            }
        }
    }

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
                // Oversized blocks are not recycled in M0. Tracked as a known
                // limitation; the bump region is sized generously to compensate.
                self.allocated -= layout.size();
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
        unsafe { heap.add_backing(aligned, bytes) };
        (heap, backing)
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
