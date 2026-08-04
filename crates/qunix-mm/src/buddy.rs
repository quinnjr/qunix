use crate::{FrameBacking, MAX_ORDER, PAGE_SIZE};

const NIL: u64 = u64::MAX;

/// A binary-buddy physical frame allocator.
///
/// Free blocks of each order form an intrusive singly linked list whose `next`
/// pointer lives in the first word of the block. `NIL` terminates a list.
pub struct BuddyAllocator<B: FrameBacking> {
    backing: B,
    free_lists: [u64; MAX_ORDER as usize + 1],
    free_bytes: u64,
    region_start: u64,
    region_end: u64,
}

impl<B: FrameBacking> BuddyAllocator<B> {
    pub const fn new(backing: B) -> Self {
        Self {
            backing,
            free_lists: [NIL; MAX_ORDER as usize + 1],
            free_bytes: 0,
            region_start: u64::MAX,
            region_end: 0,
        }
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_bytes
    }

    const fn block_size(order: u8) -> u64 {
        PAGE_SIZE << order
    }

    fn push(&mut self, pa: u64, order: u8) {
        unsafe { self.backing.write_link(pa, self.free_lists[order as usize]) };
        self.free_lists[order as usize] = pa;
    }

    fn pop(&mut self, order: u8) -> Option<u64> {
        let head = self.free_lists[order as usize];
        if head == NIL {
            return None;
        }
        self.free_lists[order as usize] = unsafe { self.backing.read_link(head) };
        Some(head)
    }

    /// Removes `pa` from the free list of `order`, if present.
    fn unlink(&mut self, pa: u64, order: u8) -> bool {
        let mut cur = self.free_lists[order as usize];
        if cur == NIL {
            return false;
        }
        if cur == pa {
            self.free_lists[order as usize] = unsafe { self.backing.read_link(cur) };
            return true;
        }
        loop {
            let next = unsafe { self.backing.read_link(cur) };
            if next == NIL {
                return false;
            }
            if next == pa {
                let after = unsafe { self.backing.read_link(next) };
                unsafe { self.backing.write_link(cur, after) };
                return true;
            }
            cur = next;
        }
    }

    /// Adds a usable physical region to the allocator.
    ///
    /// # Safety
    /// The region must be genuinely free physical memory that nothing else
    /// owns, and must remain accessible through `B` for the allocator's life.
    pub unsafe fn add_region(&mut self, start: u64, len: u64) {
        let mut addr = start.next_multiple_of(PAGE_SIZE);
        let end = (start + len) & !(PAGE_SIZE - 1);
        if addr >= end {
            return;
        }
        self.region_start = self.region_start.min(addr);
        self.region_end = self.region_end.max(end);

        while addr < end {
            // Take the largest naturally aligned block that still fits.
            let mut order = MAX_ORDER;
            while order > 0 {
                let size = Self::block_size(order);
                if addr % size == 0 && addr + size <= end {
                    break;
                }
                order -= 1;
            }
            self.push(addr, order);
            self.free_bytes += Self::block_size(order);
            addr += Self::block_size(order);
        }
    }

    /// Allocates a naturally aligned block of `PAGE_SIZE << order` bytes.
    pub fn alloc(&mut self, order: u8) -> Option<u64> {
        if order > MAX_ORDER {
            return None;
        }
        // Find the smallest order at or above `order` with a free block.
        let mut source = order;
        while source <= MAX_ORDER && self.free_lists[source as usize] == NIL {
            source += 1;
        }
        if source > MAX_ORDER {
            return None;
        }
        let pa = self.pop(source)?;
        // Split downwards, returning the upper half of each split to its list.
        while source > order {
            source -= 1;
            let buddy = pa + Self::block_size(source);
            self.push(buddy, source);
        }
        self.free_bytes -= Self::block_size(order);
        Some(pa)
    }

    /// Returns a block to the allocator, coalescing with its buddy where possible.
    ///
    /// # Safety
    /// `pa` and `order` must exactly match a previous successful `alloc`, and
    /// the memory must no longer be in use.
    pub unsafe fn free(&mut self, pa: u64, order: u8) {
        self.free_bytes += Self::block_size(order);

        let mut pa = pa;
        let mut order = order;
        while order < MAX_ORDER {
            let buddy = pa ^ Self::block_size(order);
            // Only coalesce if the buddy is entirely inside the managed range
            // and currently free at this same order.
            if buddy < self.region_start
                || buddy + Self::block_size(order) > self.region_end
                || !self.unlink(buddy, order)
            {
                break;
            }
            pa = pa.min(buddy);
            order += 1;
        }
        self.push(pa, order);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host-side backing: a flat byte buffer standing in for physical memory.
    struct VecBacking {
        base: u64,
        mem: std::cell::UnsafeCell<Vec<u8>>,
    }

    impl VecBacking {
        fn new(base: u64, len: usize) -> Self {
            Self { base, mem: std::cell::UnsafeCell::new(vec![0u8; len]) }
        }
    }

    impl FrameBacking for VecBacking {
        unsafe fn read_link(&self, pa: u64) -> u64 {
            let mem = unsafe { &*self.mem.get() };
            let off = (pa - self.base) as usize;
            u64::from_le_bytes(mem[off..off + 8].try_into().unwrap())
        }
        unsafe fn write_link(&self, pa: u64, value: u64) {
            let mem = unsafe { &mut *self.mem.get() };
            let off = (pa - self.base) as usize;
            mem[off..off + 8].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn allocator_with(base: u64, bytes: usize) -> BuddyAllocator<VecBacking> {
        let mut a = BuddyAllocator::new(VecBacking::new(base, bytes));
        unsafe { a.add_region(base, bytes as u64) };
        a
    }

    #[test]
    fn empty_allocator_has_no_free_memory() {
        let a = BuddyAllocator::new(VecBacking::new(0, 0));
        assert_eq!(a.free_bytes(), 0);
    }

    #[test]
    fn add_region_accounts_all_whole_pages() {
        let a = allocator_with(0x100000, 16 * 4096);
        assert_eq!(a.free_bytes(), 16 * 4096);
    }

    #[test]
    fn alloc_order_zero_returns_page_aligned_address_in_region() {
        let mut a = allocator_with(0x100000, 16 * 4096);
        let pa = a.alloc(0).expect("allocation failed");
        assert_eq!(pa % PAGE_SIZE, 0);
        assert!(pa >= 0x100000 && pa < 0x100000 + 16 * 4096);
        assert_eq!(a.free_bytes(), 15 * 4096);
    }

    #[test]
    fn alloc_order_two_returns_sixteen_kib_aligned_block() {
        let mut a = allocator_with(0x100000, 16 * 4096);
        let pa = a.alloc(2).expect("allocation failed");
        assert_eq!(pa % (PAGE_SIZE << 2), 0);
        assert_eq!(a.free_bytes(), 12 * 4096);
    }

    #[test]
    fn free_restores_the_original_free_total() {
        let mut a = allocator_with(0x100000, 16 * 4096);
        let before = a.free_bytes();
        let pa = a.alloc(3).unwrap();
        assert_ne!(a.free_bytes(), before);
        unsafe { a.free(pa, 3) };
        assert_eq!(a.free_bytes(), before);
    }

    #[test]
    fn freed_buddies_coalesce_back_into_one_large_block() {
        let mut a = allocator_with(0x100000, 8 * 4096);
        // Drain the whole region as single pages, then give them all back.
        let mut pages = Vec::new();
        while let Some(pa) = a.alloc(0) {
            pages.push(pa);
        }
        assert_eq!(pages.len(), 8);
        assert_eq!(a.free_bytes(), 0);
        for pa in pages {
            unsafe { a.free(pa, 0) };
        }
        assert_eq!(a.free_bytes(), 8 * 4096);
        // If coalescing worked, an order-3 (32 KiB) allocation must now succeed.
        assert!(a.alloc(3).is_some(), "buddies failed to coalesce");
    }

    #[test]
    fn exhausted_allocator_returns_none_rather_than_panicking() {
        let mut a = allocator_with(0x100000, 4 * 4096);
        assert!(a.alloc(5).is_none(), "order-5 request should not fit in 16 KiB");
        for _ in 0..4 {
            assert!(a.alloc(0).is_some());
        }
        assert!(a.alloc(0).is_none());
    }

    #[test]
    fn allocations_never_overlap() {
        let mut a = allocator_with(0x100000, 64 * 4096);
        let mut seen = std::collections::HashSet::new();
        while let Some(pa) = a.alloc(0) {
            assert!(seen.insert(pa), "address {pa:#x} handed out twice");
        }
        assert_eq!(seen.len(), 64);
    }
}
