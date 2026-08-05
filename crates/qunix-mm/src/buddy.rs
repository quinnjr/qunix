use crate::{FrameBacking, MAX_ORDER, PAGE_SIZE};

const NIL: u64 = u64::MAX;

// Bookkeeping words at the head of a free block. Two links instead of one so
// that `unlink` — the hot path of `free` — is O(1) instead of a list walk; the
// tag says which list, if any, the block is currently on, which is what makes
// the O(1) unlink possible without a walk to prove membership.
const OFF_NEXT: u64 = 0;
const OFF_PREV: u64 = 8;
const OFF_TAG: u64 = 16;

// Arbitrary constant mixed with address and order so that a tag is only valid
// for the exact (block, order) pair it was written for.
const TAG_SEED: u64 = 0x5155_4E49_585F_4652;

/// Real UEFI firmware breaks conventional memory into runs separated by
/// boot-services, ACPI and reserved descriptors; 20-40 usable runs is realistic
/// on large machines. Overflowing this discards the remainder — counted in
/// `refused_regions` and `refused_region_bytes`, not silently — so the headroom
/// is deliberately generous: 128 entries costs 2 KiB.
const MAX_REGIONS: usize = 128;

// `nonempty` is a u32 indexed by order.
const _: () = assert!((MAX_ORDER as u32) < u32::BITS);

/// A binary-buddy physical frame allocator.
///
/// Free blocks of each order form an intrusive doubly linked list threaded
/// through the first words of the block. `NIL` terminates a list.
pub struct BuddyAllocator<B: FrameBacking> {
    backing: B,
    free_lists: [u64; MAX_ORDER as usize + 1],
    /// Bit `n` set iff `free_lists[n]` is non-empty. Lets `alloc` find the
    /// smallest usable order with a mask and a `trailing_zeros` instead of
    /// walking the head array.
    nonempty: u32,
    free_bytes: u64,
    /// Bytes offered to `free` and refused as not being a block this allocator
    /// owns, for any of three reasons: the address lies in no recorded region,
    /// its extent runs past the end of the region it starts in, or it is not
    /// aligned to its own order. Absorbing any of them would mean writing 24
    /// bytes of link through memory the allocator was never given, and then
    /// handing that memory out.
    ///
    /// Bytes, not a count of events, despite the name.
    foreign_frees: u64,
    /// Coalescing attempts refused because the buddy's `prev`/`next` links did
    /// not name blocks that are themselves free at this order. Non-zero means
    /// either corruption or a caller forging a free tag.
    ///
    /// Coalescing only: `pop`'s own link rejection is counted separately, in
    /// `truncated_lists`. They are deliberately not shared — one says a `free`
    /// declined to merge and lost nothing, the other says a free list was cut
    /// short and `free_bytes` is now an over-estimate.
    rejected_unlinks: u64,
    /// Free lists truncated by `pop` refusing an implausible forward link.
    ///
    /// Every block past the rejected link is unreachable: the chain cannot be
    /// walked, so the bytes cannot be subtracted from `free_bytes`, and their
    /// count cannot be known. Guessing a correction would be worse than
    /// reporting the fact, so the counter exists instead and `free_bytes` is
    /// documented as an upper bound once it is non-zero.
    truncated_lists: u64,
    /// Regions actually handed to the allocator, in insertion order.
    ///
    /// A scalar min/max span would treat every hole between disjoint regions as
    /// managed, and `free` would then read a buddy tag out of unmapped MMIO on
    /// any machine with the usual low-RAM / PCI-hole / high-RAM split.
    regions: [(u64, u64); MAX_REGIONS],
    region_count: usize,
    /// Bytes lost to page rounding at region edges. Never allocatable, and
    /// otherwise invisible — `free_bytes` cannot show what was never added.
    edge_dropped_bytes: u64,
    /// Whole pages in regions refused because `MAX_REGIONS` was already full.
    ///
    /// Kept apart from edge rounding because the two say different things: edge
    /// slop is a handful of kilobytes of unavoidable granularity loss, whereas
    /// this is usable RAM the allocator declined to manage and is a sign that
    /// `MAX_REGIONS` needs raising.
    refused_region_bytes: u64,
    refused_regions: u32,
    /// Bytes in firmware entries discarded whole for being nonsensical — an
    /// overflowing range, or one with no whole page in it. Distinct again: this
    /// points at the memory map, not at the allocator's own limits.
    malformed_region_bytes: u64,
}

impl<B: FrameBacking> BuddyAllocator<B> {
    pub const fn new(backing: B) -> Self {
        Self {
            backing,
            free_lists: [NIL; MAX_ORDER as usize + 1],
            nonempty: 0,
            free_bytes: 0,
            foreign_frees: 0,
            rejected_unlinks: 0,
            truncated_lists: 0,
            regions: [(0, 0); MAX_REGIONS],
            region_count: 0,
            edge_dropped_bytes: 0,
            refused_region_bytes: 0,
            refused_regions: 0,
            malformed_region_bytes: 0,
        }
    }

    /// Replaces the backing. Only valid before any region is added — the
    /// existing free lists are threaded through memory reached via the old one.
    pub fn set_backing(&mut self, backing: B) {
        // A real assert, not debug-only: swapping the backing after regions
        // exist would leave the free lists pointing through the old one.
        assert!(self.region_count == 0, "backing changed after regions were added");
        self.backing = backing;
    }

    /// Bytes currently on the free lists.
    ///
    /// An **upper bound** once `truncated_lists()` is non-zero: a truncation
    /// orphans every block past the rejected link, and those bytes stay counted
    /// here because the chain that held them can no longer be walked to
    /// subtract them. Every other rejection path keeps this number exact — see
    /// the `refuse!` macro in `free`.
    pub fn free_bytes(&self) -> u64 {
        self.free_bytes
    }

    /// Every byte the allocator was offered but never manages, whatever the
    /// cause. Retained as the single headline number; the three components
    /// below are what actually tell you which problem you have.
    pub fn dropped_bytes(&self) -> u64 {
        self.edge_dropped_bytes + self.refused_region_bytes + self.malformed_region_bytes
    }

    /// Bytes discarded at region edges by page-granularity rounding.
    pub fn edge_dropped_bytes(&self) -> u64 {
        self.edge_dropped_bytes
    }

    /// Whole pages in regions refused for exceeding `MAX_REGIONS`.
    pub fn refused_region_bytes(&self) -> u64 {
        self.refused_region_bytes
    }

    /// Regions refused for exceeding `MAX_REGIONS`.
    pub fn refused_regions(&self) -> u32 {
        self.refused_regions
    }

    /// Bytes in firmware entries discarded whole as nonsensical.
    pub fn malformed_region_bytes(&self) -> u64 {
        self.malformed_region_bytes
    }

    /// Bytes passed to `free` that this allocator does not own — lying in no
    /// recorded region, overrunning the end of their region, or misaligned for
    /// their order. Non-zero means either a caller is returning memory that was
    /// never handed out (which `paging::unmap_and_prune` does by design), or one
    /// is freeing at the wrong order.
    ///
    /// Bytes, not a count of events, despite the name.
    pub fn foreign_frees(&self) -> u64 {
        self.foreign_frees
    }

    /// Coalescing attempts refused because a neighbour link did not name a
    /// block that is itself free at that order. Coalescing only — `pop`'s
    /// rejections are in [`Self::truncated_lists`].
    pub fn rejected_unlinks(&self) -> u64 {
        self.rejected_unlinks
    }

    /// Free lists cut short by `pop` refusing an implausible forward link.
    ///
    /// Non-zero means memory was leaked deliberately in preference to splicing
    /// through a corrupt link, and that [`Self::free_bytes`] is now an upper
    /// bound rather than an exact figure.
    pub fn truncated_lists(&self) -> u64 {
        self.truncated_lists
    }

    /// The region containing `block`, if any.
    ///
    /// Called once per `free` for the block being freed: regions are disjoint
    /// intervals and a merged block is the union of two adjacent sub-intervals,
    /// so every ancestor lies in the same region its child does, and re-looking
    /// it up per level would buy nothing.
    ///
    /// It is *also* called from `plausible_link`, on two paths. Coalescing runs
    /// it up to twice per level, so a full 18-level merge against populated
    /// free lists scans the region table repeatedly; and `pop` runs it once per
    /// allocation, on the hot path, to validate the link it is about to make
    /// the new list head. That cost is the price of validating a link before
    /// splicing through it; `plausible_link` short circuits on `NIL`, so the
    /// common case of a short list pays almost none of it.
    fn region_of(&self, block: u64) -> Option<(u64, u64)> {
        self.regions[..self.region_count]
            .iter()
            .copied()
            .find(|&(start, end)| block >= start && block < end)
    }

    const fn block_size(order: u8) -> u64 {
        PAGE_SIZE << order
    }

    /// The value stamped into a block while it sits on the free list of `order`.
    ///
    /// Distinct orders map to distinct tags for the same block, so a block free
    /// at order 2 is not mistaken for a free order-0 block at the same address.
    /// `| 1` guarantees a live tag can never equal the cleared value 0, so a
    /// zeroed live block cannot read as free. The residual 2^-64 chance that
    /// caller data happens to match is inherent to an in-band tag and is the
    /// price of an O(1) `unlink`.
    const fn free_tag(pa: u64, order: u8) -> u64 {
        (TAG_SEED ^ pa ^ (order as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1
    }

    fn word(&self, pa: u64, offset: u64) -> u64 {
        unsafe { self.backing.read_link(pa + offset) }
    }

    fn set_word(&mut self, pa: u64, offset: u64, value: u64) {
        unsafe { self.backing.write_link(pa + offset, value) };
    }

    fn push(&mut self, pa: u64, order: u8) {
        let head = self.free_lists[order as usize];
        self.set_word(pa, OFF_NEXT, head);
        self.set_word(pa, OFF_PREV, NIL);
        self.set_word(pa, OFF_TAG, Self::free_tag(pa, order));
        if head != NIL {
            self.set_word(head, OFF_PREV, pa);
        }
        self.free_lists[order as usize] = pa;
        self.nonempty |= 1 << order;
    }

    fn pop(&mut self, order: u8) -> Option<u64> {
        let head = self.free_lists[order as usize];
        if head == NIL {
            return None;
        }
        // `next` is read out of a block that was, until it was freed, caller
        // memory. `unlink` validates exactly this word before splicing through
        // it; `pop` did not, which made the identical hazard reachable from the
        // hot allocation path -- `set_word(next, OFF_PREV, NIL)` is an 8-byte
        // store to an address the previous owner of the block chose, and the
        // list head is left naming it for every subsequent `pop`.
        //
        // An implausible link truncates the list rather than following it. The
        // blocks beyond it are lost, which is a leak; splicing through it is
        // memory corruption, and a leak is the better of the two. The bytes
        // cannot be reclaimed from `free_bytes` because the chain that held
        // them is exactly what has just been declared unwalkable, so the event
        // is counted instead and `free_bytes` documents itself as an upper
        // bound while `truncated_lists` is non-zero.
        //
        // The orphaned blocks keep valid free tags and a `prev` naming `head`,
        // which is about to become a live allocation. `unlink` is what would
        // then write through that `prev`, and it refuses to, because it
        // requires a neighbour to be free in its own right and `head`'s tag is
        // cleared below.
        let next = match self.word(head, OFF_NEXT) {
            link if self.plausible_link(link, order) => link,
            _ => {
                self.truncated_lists += 1;
                NIL
            }
        };
        self.free_lists[order as usize] = next;
        if next == NIL {
            self.nonempty &= !(1 << order);
        } else {
            self.set_word(next, OFF_PREV, NIL);
        }
        // Clear the tag before the block leaves the allocator: a stale tag left
        // in a block someone else now owns would let a later `free` of its
        // buddy swallow live memory.
        self.set_word(head, OFF_TAG, 0);
        Some(head)
    }

    /// A free-list link that could plausibly name a block of `order`.
    ///
    /// `NIL` terminates a list; anything else must be inside a region the
    /// allocator was actually handed and aligned the way every block of this
    /// order is.
    fn plausible_link(&self, link: u64, order: u8) -> bool {
        link == NIL
            || (link.is_multiple_of(Self::block_size(order)) && self.region_of(link).is_some())
    }

    /// A free-list neighbour that is genuinely free at `order`, not merely at an
    /// address where such a block could live.
    ///
    /// Plausibility alone is not enough to authorise a write through a link.
    /// `pop` can orphan a block whose `prev` names a block it then returned to
    /// a caller: the orphan's tag is still valid, its `prev` is in-region and
    /// correctly aligned, and a later coalescing `unlink` of the orphan would
    /// splice through it — an 8-byte store into live caller memory. Reading the
    /// neighbour's own tag closes that: `pop` clears a block's tag as it leaves
    /// the allocator, so a returned block can never pass this.
    ///
    /// It refuses nothing legitimate. A block genuinely on the free list of
    /// `order` has its tag written by `push` and cleared only by `pop` or
    /// `unlink`, both of which remove it from the list first.
    ///
    /// The plausibility test must come first: it is what keeps the tag read
    /// below off memory this allocator was never given.
    fn free_neighbour(&self, link: u64, order: u8) -> bool {
        link == NIL
            || (self.plausible_link(link, order)
                && self.word(link, OFF_TAG) == Self::free_tag(link, order))
    }

    /// Removes `pa` from the free list of `order`, if it is on it.
    ///
    /// The tag identifies membership without walking the list, so this is O(1).
    fn unlink(&mut self, pa: u64, order: u8) -> bool {
        if self.word(pa, OFF_TAG) != Self::free_tag(pa, order) {
            return false;
        }
        let prev = self.word(pa, OFF_PREV);
        let next = self.word(pa, OFF_NEXT);
        // The tag lives in caller-owned memory, so a block that is live and
        // happens to hold a matching word gets us here with `prev`/`next` fully
        // attacker-chosen. Requiring each neighbour to be *free at this order*
        // — not merely at a plausible address — downgrades the splice from an
        // arbitrary 8-byte write to a write into a block this allocator
        // currently owns and has tagged as free. In-region-and-aligned alone
        // was not enough: `pop`'s truncation can leave an orphan whose `prev`
        // names a block that has since been handed out, and that address is
        // both in-region and correctly aligned. It is a mitigation rather than
        // a removal of the hazard: membership is still authorised by words that
        // live in memory the caller once owned, and only out-of-band state (a
        // per-order bitmap) would change that.
        if !self.free_neighbour(prev, order) || !self.free_neighbour(next, order) {
            self.rejected_unlinks += 1;
            return false;
        }
        if prev == NIL {
            self.free_lists[order as usize] = next;
            if next == NIL {
                self.nonempty &= !(1 << order);
            }
        } else {
            self.set_word(prev, OFF_NEXT, next);
        }
        if next != NIL {
            self.set_word(next, OFF_PREV, prev);
        }
        self.set_word(pa, OFF_TAG, 0);
        true
    }

    /// Adds a usable physical region to the allocator.
    ///
    /// May accept nothing, and says so only through the counters. A range that
    /// overflows the address space, one with no whole page in it, or one
    /// arriving after `MAX_REGIONS` is full is dropped and billed to
    /// `malformed_region_bytes` or `refused_region_bytes`. Check
    /// `dropped_bytes()` once the memory map has been consumed; a caller that
    /// assumes acceptance will go on to `free` into a region the allocator
    /// never took, which trips the pre-init assert.
    ///
    /// # Safety
    /// The region must be genuinely free physical memory that nothing else
    /// owns, and must remain accessible through `B` for the allocator's life.
    pub unsafe fn add_region(&mut self, start: u64, len: u64) {
        // Firmware supplies these numbers, so a nonsensical entry must be
        // dropped rather than wrapped into a plausible-looking small region.
        let (Some(mut addr), Some(limit)) =
            (start.checked_next_multiple_of(PAGE_SIZE), start.checked_add(len))
        else {
            // A firmware entry that overflows the address space is discarded
            // whole; count it so the total is not silently short.
            self.malformed_region_bytes += len;
            return;
        };
        let end = limit & !(PAGE_SIZE - 1);
        if addr >= end {
            self.malformed_region_bytes += len;
            return;
        }
        // Partial pages at either edge cannot be handed out at 4 KiB
        // granularity. Counting them keeps the loss visible instead of silently
        // absent from every total.
        self.edge_dropped_bytes += (addr - start) + (limit - end);

        if self.region_count == MAX_REGIONS {
            // Refusing is the safe failure: a region that is not recorded here
            // would never satisfy `region_of`, so its blocks could never
            // coalesce, and `free` would silently stop merging.
            self.refused_region_bytes += end - addr;
            self.refused_regions += 1;
            return;
        }
        self.regions[self.region_count] = (addr, end);
        self.region_count += 1;

        while addr < end {
            // Largest naturally aligned block that still fits. Derived from bit
            // counts rather than `addr % size`, which would emit a real 64-bit
            // division per iteration of a loop run once per block at boot.
            // `addr == 0` reports 64 trailing zeros, which saturates correctly.
            let by_align = addr.trailing_zeros().saturating_sub(PAGE_SIZE.trailing_zeros());
            let by_fit = ((end - addr) / PAGE_SIZE).ilog2();
            let order = by_align.min(by_fit).min(MAX_ORDER as u32) as u8;
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
        // Smallest non-empty order at or above `order`, found by masking off
        // the orders below it and taking the lowest remaining set bit. Two ALU
        // ops instead of an unpredictable walk over the head array.
        let usable = self.nonempty & !((1u32 << order) - 1);
        if usable == 0 {
            return None;
        }
        let mut source = usable.trailing_zeros() as u8;
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
        // Above `MAX_ORDER` the free-list index is out of bounds and the shift
        // in `block_size` overflows, so this is checked before anything reads
        // either. `alloc` has always rejected such orders.
        assert!(order <= MAX_ORDER, "free of order {order} above MAX_ORDER");
        // Pre-init, `backing` is a placeholder and `push` would write 24 bytes
        // through a zero offset. A real assert rather than `debug_assert`: this
        // is not a per-instruction hot path, and the release-build consequence
        // is memory corruption rather than a wrong answer.
        assert!(self.region_count > 0, "free before any region was added");
        self.free_bytes += Self::block_size(order);

        let mut pa = pa;
        let mut order = order;

        // Three ways a block can fail to be one this allocator owns, all
        // refused identically: undo the speculative `free_bytes` above and bill
        // it to `foreign_frees`. Written once rather than three times because
        // an accounting change applied to only some of them would leave
        // `free_bytes` permanently inflated, and `free_bytes` is what the
        // kernel uses to decide it is out of memory.
        macro_rules! refuse {
            () => {{
                self.free_bytes -= Self::block_size(order);
                self.foreign_frees += Self::block_size(order);
                return;
            }};
        }

        // One region lookup for the whole merge chain.
        let Some((region_start, region_end)) = self.region_of(pa) else {
            // Not ours: reject rather than absorb. Pushing it would write links
            // into memory in no region and then hand that memory out — the
            // bootloader's own page tables reach here via
            // `paging::unmap_and_prune`.
            refuse!()
        };
        // `region_of` locates `pa` only, so a block whose start is inside the
        // region but whose extent runs past its end would otherwise be
        // accepted. Pushing it puts memory the allocator was never given on a
        // free list, and the next `alloc` of that order hands it out. The merge
        // loop below already applies exactly this test to the *buddy*
        // (`region_end - buddy < size`); this is the same test for the block
        // being freed, which it was missing.
        if region_end - pa < Self::block_size(order) {
            refuse!()
        }
        // Extent alone is not enough. `buddy = pa ^ size` is only the real buddy
        // when `pa` is a multiple of `size`; for a misaligned `pa` the merge
        // loop walks unrelated blocks, and the block pushed at the end straddles
        // two naturally-aligned blocks, so a later `alloc` hands out memory that
        // overlaps a live allocation. That is the same aliasing this whole guard
        // exists to prevent, reached one step further along, and a fuzz harness
        // that derives addresses by rounding down to a multiple of the block
        // size cannot generate it.
        if !pa.is_multiple_of(Self::block_size(order)) {
            refuse!()
        }
        // One read `push` is about to do anyway. Without it a second `free` of
        // the same block re-pushes it, `push` writes `next(pa) = pa` when `pa`
        // is already the head, and two later `alloc`s return the same frame.
        assert!(
            self.word(pa, OFF_TAG) != Self::free_tag(pa, order),
            "double free of {pa:#x} at order {order}"
        );
        while order < MAX_ORDER {
            let size = Self::block_size(order);
            let buddy = pa ^ size;
            // The region check keeps the tag read below off memory the
            // allocator was never handed — without it, a buddy landing in a
            // PCI hole would be dereferenced through the HHDM, which does not
            // map it. What then authorises the merge is the free tag, which is
            // only ever written into a block while it is on this exact free
            // list, so blocks currently allocated and blocks free at some
            // other order both fail it.
            // `buddy < region_end` precedes the subtraction so a buddy past the
            // end cannot underflow into a value that compares as "fits".
            if buddy < region_start
                || buddy >= region_end
                || region_end - buddy < size
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
    ///
    /// Deliberately bounds-checked slicing rather than the raw volatile access
    /// `FrameBacking` documents as required, so an out-of-region access panics
    /// with an index instead of quietly corrupting the buffer. That is right
    /// for a unit test and wrong everywhere else, which is why the two other
    /// copies differ: `benches/buddy.rs` uses raw volatile because this
    /// checking dominated the measurement, and `fuzz/fuzz_targets/buddy.rs`
    /// uses raw volatile plus an arena-bounds assert. Do not unify them.
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

    /// Drains every order-0 block and asserts each lies inside `region`.
    ///
    /// The point of a refusal is not that a counter moved — it is that the
    /// refused memory never becomes allocatable at *any* order. Asserting
    /// `alloc(n) == None` for a single `n` is far weaker: on a region too small
    /// to hold an order-`n` block it is true no matter what `free` did.
    fn drain_and_check<B: FrameBacking>(a: &mut BuddyAllocator<B>, region: (u64, u64)) -> usize {
        let mut handed = 0;
        while let Some(pa) = a.alloc(0) {
            assert!(
                pa >= region.0 && pa + PAGE_SIZE <= region.1,
                "alloc handed out {pa:#x}, outside the region {:#x}..{:#x}",
                region.0,
                region.1
            );
            handed += 1;
        }
        handed
    }

    /// A block whose *start* is inside a region but whose *extent* runs past
    /// the end of it must be refused.
    ///
    /// `region_of` only locates `pa`, and the coalescing loop below it checks
    /// the buddy's extent (`region_end - buddy < size`) but never the incoming
    /// block's own. Accepting one pushes a free block that reaches beyond the
    /// memory the allocator was given, and the next `alloc` of that order hands
    /// that memory out. Found by the fuzz target, not by review.
    ///
    /// No in-tree caller frees above order 0 today, so this is a latent hole in
    /// a check that exists to be defensive rather than a live corruption.
    #[test]
    fn free_refuses_a_block_that_overruns_its_region() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 3 * 4096));
        unsafe { a.add_region(base, 3 * 4096) };
        let free_before = a.free_bytes();
        let foreign_before = a.foreign_frees();

        // 4 pages starting at a 3-page region: the last page lies outside.
        unsafe { a.free(base, 2) };

        assert_eq!(a.free_bytes(), free_before, "an overrunning free added memory to the heap");
        assert_eq!(
            a.foreign_frees(),
            foreign_before + (PAGE_SIZE << 2),
            "an overrunning free was not counted as rejected"
        );
        // The consequence that matters: the refused page must not become
        // allocatable at any order, and the region must still yield exactly the
        // three pages it really has.
        assert_eq!(drain_and_check(&mut a, (base, base + 3 * PAGE_SIZE)), 3);
    }

    /// The overrun need not start at the region base.
    ///
    /// A guard written against `region_start` instead of `region_end` would
    /// pass the base-aligned case above and fail this one.
    #[test]
    fn free_refuses_an_overrunning_block_at_a_nonzero_offset() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 6 * 4096));
        unsafe { a.add_region(base, 6 * 4096) };
        let free_before = a.free_bytes();
        let foreign_before = a.foreign_frees();

        // 4 pages starting one order-2 block in: base+4 pages .. base+8 pages,
        // against a region that ends at base+6 pages.
        unsafe { a.free(base + 4 * PAGE_SIZE, 2) };

        assert_eq!(a.free_bytes(), free_before);
        assert_eq!(a.foreign_frees(), foreign_before + (PAGE_SIZE << 2));
        assert_eq!(drain_and_check(&mut a, (base, base + 6 * PAGE_SIZE)), 6);
    }

    /// A block whose start is *before* every region takes the other rejection
    /// arm (`region_of` returns `None`), which nothing else covers.
    #[test]
    fn free_refuses_a_block_that_starts_before_its_region() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 4 * 4096));
        unsafe { a.add_region(base, 4 * 4096) };
        let free_before = a.free_bytes();
        let foreign_before = a.foreign_frees();

        unsafe { a.free(base - PAGE_SIZE, 0) };

        assert_eq!(a.free_bytes(), free_before);
        assert_eq!(a.foreign_frees(), foreign_before + PAGE_SIZE);
        assert_eq!(drain_and_check(&mut a, (base, base + 4 * PAGE_SIZE)), 4);
    }

    /// A block that fits its region but is not aligned to its own order must be
    /// refused: `pa ^ size` is not the buddy, so the merge loop walks unrelated
    /// blocks and the block finally pushed straddles two aligned ones.
    ///
    /// Before this check, `free(base + PAGE_SIZE, 1)` on a 4-page region was
    /// accepted and the next `alloc(1)` returned `base + PAGE_SIZE`, overlapping
    /// the order-1 block at `base`.
    #[test]
    fn free_refuses_a_block_not_aligned_to_its_order() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 4 * 4096));
        unsafe { a.add_region(base, 4 * 4096) };
        // Drain first, so anything that becomes allocatable below came from the
        // misaligned free rather than from the region itself.
        while a.alloc(0).is_some() {}
        let free_before = a.free_bytes();
        let foreign_before = a.foreign_frees();

        unsafe { a.free(base + PAGE_SIZE, 1) };

        assert_eq!(a.free_bytes(), free_before, "a misaligned free added memory to the heap");
        assert_eq!(a.foreign_frees(), foreign_before + (PAGE_SIZE << 1));
        assert_eq!(a.alloc(1), None, "a misaligned block was handed out by alloc");
        assert_eq!(a.alloc(0), None, "a misaligned block was split and handed out");
    }

    /// A refusal must leave the allocator's own structures untouched, not just
    /// decline to add memory. A refusal that wrote a tag or mangled a list would
    /// pass every assertion above and break the next merge.
    #[test]
    fn a_refused_free_leaves_coalescing_intact() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 4 * 4096));
        unsafe { a.add_region(base, 4 * 4096) };

        let a0 = a.alloc(0).unwrap();
        let a1 = a.alloc(0).unwrap();
        // Refuse something in between the two live blocks' lifetimes.
        unsafe { a.free(base + PAGE_SIZE, 1) };
        unsafe { a.free(a0, 0) };
        unsafe { a.free(a1, 0) };

        // The two order-0 buddies must still merge into an order-1 block.
        assert!(a.alloc(1).is_some(), "coalescing broke after a refused free");
    }

    /// A forged free tag whose links point outside every region must be
    /// refused, and must not become allocatable.
    ///
    /// This is the case `plausible_link` exists for and the one nothing
    /// covered: the existing forged-tag test supplies tags that fail the
    /// *first* comparison, so `plausible_link` was never reached and
    /// `rejected_unlinks` was asserted nowhere. CLAUDE.md names "a forged tag
    /// is rejected" as one of the negative directions whose absence let two
    /// memory-corruption bugs ship.
    #[test]
    fn unlink_refuses_a_forged_tag_whose_links_are_implausible() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 8 * 4096));
        unsafe { a.add_region(base, 8 * 4096) };

        // Two order-0 buddies. `live` stays allocated; `other` is freed, which
        // makes the allocator try to coalesce with `live`.
        let live = a.alloc(0).unwrap();
        let other = live ^ PAGE_SIZE;
        while a.alloc(0).map(|p| p != other).unwrap_or(false) {}

        // Forge the tag the coalescer checks, and point the links at an
        // address in no region at all.
        let forged = BuddyAllocator::<VecBacking>::free_tag(live, 0);
        unsafe {
            a.backing.write_link(live + OFF_TAG, forged);
            a.backing.write_link(live + OFF_PREV, 0xdead_0000);
            a.backing.write_link(live + OFF_NEXT, 0xdead_0000);
        }

        let before = a.rejected_unlinks();
        unsafe { a.free(other, 0) };

        assert_eq!(
            a.rejected_unlinks(),
            before + 1,
            "an implausible link was spliced instead of refused"
        );
        // The consequence: the live block must never be handed out again.
        while let Some(pa) = a.alloc(0) {
            assert_ne!(pa, live, "a live block was handed out via a forged tag");
        }
    }

    /// `pop` must validate the link it splices, exactly as `unlink` does.
    ///
    /// The same hazard, one step over from the fix that added `plausible_link`:
    /// the head's `next` word lives in memory a caller owned until it was
    /// freed, so following it unvalidated turns `set_word(next, OFF_PREV, NIL)`
    /// into an 8-byte write to an address that block's previous owner chose --
    /// and leaves the list head naming it for every later `pop`. `unlink`
    /// checked it; `pop`, on the hot allocation path, did not.
    /// Builds an order-0 free list with two entries whose buddies stay
    /// allocated, plus an untouched order-3 block, and returns
    /// `(allocator, base, head, orphan)`.
    ///
    /// The buddies matter: two freed frames that are buddies coalesce into an
    /// order-1 block and leave the order-0 list with a single entry, whose
    /// `next` is already `NIL`. A test that forges `next` on a one-entry list
    /// asserts nothing about truncation, because the list was already
    /// terminated. Freeing frames whose buddies are still out is what makes the
    /// list genuinely two long.
    fn allocator_with_a_two_entry_order_zero_list() -> (BuddyAllocator<VecBacking>, u64, u64, u64) {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 16 * 4096));
        unsafe { a.add_region(base, 16 * 4096) };

        // The low half as eight single frames; the high half stays as one
        // order-3 block, so there is still allocatable memory after the
        // truncation to count.
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.push(a.alloc(0).expect("the low half must yield eight frames"));
        }
        // Free two frames whose buddies are still held, so neither coalesces.
        assert_eq!(frames[0] ^ PAGE_SIZE, frames[1], "test needs frames 0 and 1 to be buddies");
        assert_eq!(frames[2] ^ PAGE_SIZE, frames[3], "test needs frames 2 and 3 to be buddies");
        unsafe { a.free(frames[0], 0) };
        unsafe { a.free(frames[2], 0) };

        let head = a.free_lists[0];
        assert_eq!(head, frames[2], "the most recent free must be the list head");
        (a, base, head, frames[0])
    }

    /// `pop` must validate the link it splices, exactly as `unlink` does.
    ///
    /// The same hazard, one step over from the fix that added `plausible_link`:
    /// the head's `next` word lives in memory a caller owned until it was
    /// freed, so following it unvalidated turns `set_word(next, OFF_PREV, NIL)`
    /// into an 8-byte write to an address that block's previous owner chose --
    /// and leaves the list head naming it for every later `pop`. `unlink`
    /// checked it; `pop`, on the hot allocation path, did not.
    #[test]
    fn pop_refuses_an_implausible_next_link() {
        let (mut a, base, head, orphan) = allocator_with_a_two_entry_order_zero_list();

        // The precondition the test rests on: without a real second entry,
        // truncation is indistinguishable from an already-terminated list.
        assert_ne!(
            a.word(head, OFF_NEXT),
            NIL,
            "the order-0 list has one entry, so truncating it proves nothing"
        );
        unsafe { a.backing.write_link(head + OFF_NEXT, 0xdead_0000) };

        let before = a.truncated_lists();
        let popped = a.alloc(0).expect("the head itself is still allocatable");
        assert_eq!(popped, head);
        assert_eq!(
            a.truncated_lists(),
            before + 1,
            "an implausible link was spliced instead of refused"
        );

        // The list is truncated, not left pointing at the forged address. Every
        // remaining frame must come from the untouched order-3 block: exactly
        // eight, and the orphaned entry is leaked rather than handed out.
        let mut handed = 0;
        while let Some(pa) = a.alloc(0) {
            assert_ne!(pa, 0xdead_0000, "a forged link was handed out as a block");
            assert_ne!(pa, orphan, "a block beyond the truncation was handed out");
            assert!(pa >= base && pa < base + 16 * 4096, "alloc returned {pa:#x}, outside the region");
            handed += 1;
        }
        assert_eq!(handed, 8, "truncation lost or invented blocks beyond the orphan");
    }

    /// A truncation must be reported, because it silently invalidates
    /// `free_bytes`.
    ///
    /// The orphaned blocks stay counted as free forever — the chain that held
    /// them is exactly what was declared unwalkable, so they cannot be
    /// subtracted. `free_bytes` is what the kernel consults to decide it is out
    /// of memory, so the over-report has to be visible somewhere.
    #[test]
    fn a_truncated_list_is_counted_and_free_bytes_becomes_an_upper_bound() {
        let (mut a, _base, head, orphan) = allocator_with_a_two_entry_order_zero_list();
        assert_eq!(a.truncated_lists(), 0, "nothing has been truncated yet");
        unsafe { a.backing.write_link(head + OFF_NEXT, 0xdead_0000) };

        let free_before = a.free_bytes();
        a.alloc(0).unwrap();
        assert_eq!(a.truncated_lists(), 1, "a truncation went unreported");

        // One frame left the allocator, so exactly one page comes off the
        // total; the orphan's page is still counted despite being unreachable,
        // which is the over-report the counter exists to disclose.
        assert_eq!(a.free_bytes(), free_before - PAGE_SIZE);
        let reachable: u64 = {
            let mut n = 0;
            while a.alloc(0).is_some() {
                n += 1;
            }
            n * PAGE_SIZE
        };
        assert_eq!(
            reachable,
            free_before - PAGE_SIZE - PAGE_SIZE,
            "the orphan at {orphan:#x} was either reclaimed or the leak grew"
        );
        // The disclosure itself: the allocator is empty, yet still reports a
        // page free. That gap is what `truncated_lists() != 0` warns about.
        assert_eq!(
            a.free_bytes(),
            PAGE_SIZE,
            "free_bytes should over-report by exactly the orphaned page"
        );
    }

    /// The corruption path `pop`'s truncation opens, and the reason `unlink`
    /// requires a neighbour to be *free* rather than merely plausible.
    ///
    /// After a truncation the second list entry keeps a valid free tag and a
    /// `prev` naming the block `pop` just returned to a caller. Coalescing that
    /// orphan away would execute `set_word(prev, OFF_NEXT, ..)` — an 8-byte
    /// store into live caller memory at an address that is in-region and
    /// correctly aligned, so the plausibility test alone waves it through.
    #[test]
    fn unlink_refuses_a_neighbour_that_is_not_itself_free() {
        let (mut a, _base, head, orphan) = allocator_with_a_two_entry_order_zero_list();
        unsafe { a.backing.write_link(head + OFF_NEXT, 0xdead_0000) };

        // `live` is now caller memory, and the orphan's `prev` still names it.
        let live = a.alloc(0).unwrap();
        assert_eq!(live, head);
        assert_eq!(a.word(orphan, OFF_PREV), live, "test needs the orphan to point at the live block");
        // What a caller writes into the frame it now owns.
        const SENTINEL: u64 = 0x0102_0304_0506_0708;
        for off in [OFF_NEXT, OFF_PREV, OFF_TAG] {
            unsafe { a.backing.write_link(live + off, SENTINEL) };
        }

        // Freeing the orphan's buddy is what drives `unlink(orphan, 0)`.
        let buddy = orphan ^ PAGE_SIZE;
        let before = a.rejected_unlinks();
        unsafe { a.free(buddy, 0) };

        assert_eq!(
            a.rejected_unlinks(),
            before + 1,
            "a neighbour that is not itself free was spliced through"
        );
        for off in [OFF_NEXT, OFF_PREV, OFF_TAG] {
            assert_eq!(
                a.word(live + off, 0),
                SENTINEL,
                "the coalescer wrote through a stale link into live memory at offset {off}"
            );
        }
    }

    /// `set_backing` after a region exists must panic rather than silently
    /// leave the free lists pointing through the old backing.
    #[test]
    #[should_panic(expected = "backing changed after regions were added")]
    fn set_backing_refuses_a_swap_after_regions_exist() {
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 4 * 4096));
        unsafe { a.add_region(base, 4 * 4096) };
        a.set_backing(VecBacking::new(base, 4 * 4096));
    }

    /// The 129th region must be refused, and its memory must never be handed
    /// out afterwards — the negative direction of `MAX_REGIONS`, which the fuzz
    /// target cannot reach because it can only fit a handful of regions.
    #[test]
    fn add_region_refuses_past_max_regions_and_never_hands_it_out() {
        let base = 0x100000;
        // One page per region, two pages apart so no two are adjacent.
        let span = (MAX_REGIONS as u64 + 2) * 2 * PAGE_SIZE;
        let mut a = BuddyAllocator::new(VecBacking::new(base, span as usize));
        for i in 0..MAX_REGIONS as u64 {
            unsafe { a.add_region(base + i * 2 * PAGE_SIZE, PAGE_SIZE) };
        }
        let free_before = a.free_bytes();

        let overflow = base + MAX_REGIONS as u64 * 2 * PAGE_SIZE;
        unsafe { a.add_region(overflow, PAGE_SIZE) };

        assert_eq!(a.refused_regions(), 1, "the 129th region was not refused");
        assert_eq!(a.refused_region_bytes(), PAGE_SIZE);
        assert_eq!(a.free_bytes(), free_before, "a refused region added memory");

        // Negative direction: a refused region is not recorded, so freeing into
        // it is foreign, and draining must never return one of its addresses.
        let foreign_before = a.foreign_frees();
        unsafe { a.free(overflow, 0) };
        assert_eq!(a.foreign_frees(), foreign_before + PAGE_SIZE);
        while let Some(pa) = a.alloc(0) {
            assert_ne!(pa, overflow, "a refused region was handed out");
        }
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
    fn unaligned_region_start_is_rounded_up_to_the_first_whole_page() {
        let mut a = BuddyAllocator::new(VecBacking::new(0x100000, 5 * 4096));
        // One page is lost to the misaligned head, one to the short tail.
        unsafe { a.add_region(0x100000 + 100, 4 * 4096) };
        assert_eq!(a.free_bytes(), 3 * 4096);
        // 3996 bytes rounded off the head plus 100 off the tail: exactly one
        // page of slop, and all of it edge rounding rather than a refusal.
        assert_eq!(a.edge_dropped_bytes(), 4096);
        assert_eq!(a.malformed_region_bytes(), 0);
        assert_eq!(a.refused_region_bytes(), 0);
        let pa = a.alloc(0).unwrap();
        assert_eq!(pa, 0x101000);
    }

    #[test]
    fn region_shorter_than_a_page_is_ignored() {
        let mut a = BuddyAllocator::new(VecBacking::new(0x100000, 4096));
        unsafe { a.add_region(0x100000, 100) };
        assert_eq!(a.free_bytes(), 0);
        // Discarded whole, so it is malformed rather than edge slop.
        assert_eq!(a.edge_dropped_bytes(), 0);
        assert_eq!(a.malformed_region_bytes(), 100);
        assert_eq!(a.dropped_bytes(), 100);
        assert!(a.alloc(0).is_none());
    }

    #[test]
    fn region_that_would_overflow_the_address_space_is_ignored() {
        let mut a = BuddyAllocator::new(VecBacking::new(0, 0));
        unsafe { a.add_region(u64::MAX - 4096, 2 * 4096) };
        assert_eq!(a.free_bytes(), 0);
        assert_eq!(a.edge_dropped_bytes(), 0);
        assert_eq!(a.malformed_region_bytes(), 2 * 4096);
        assert!(a.alloc(0).is_none());
    }

    #[test]
    fn regions_beyond_the_cap_are_refused_and_counted() {
        // One page per region, spaced two pages apart so they stay disjoint and
        // nothing merges across the gaps.
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 130 * 2 * 4096));
        for i in 0..130u64 {
            unsafe { a.add_region(base + i * 2 * 4096, 4096) };
        }
        assert_eq!(a.free_bytes(), 128 * 4096);
        assert_eq!(a.refused_regions(), 2);
        assert_eq!(a.refused_region_bytes(), 2 * 4096);
        // The refused pages are whole, so none of the loss is edge rounding.
        assert_eq!(a.edge_dropped_bytes(), 0);
        assert_eq!(a.dropped_bytes(), 2 * 4096);
    }

    /// Every order-1 block the allocator can still produce, checked against a
    /// frame that is supposed to remain allocated.
    fn assert_never_handed_out_inside_an_order_one_block(
        a: &mut BuddyAllocator<VecBacking>,
        live: u64,
    ) {
        while let Some(pa) = a.alloc(1) {
            assert!(
                !(pa..pa + 2 * PAGE_SIZE).contains(&live),
                "order-1 block {pa:#x} covers live frame {live:#x}"
            );
        }
    }

    #[test]
    fn a_block_whose_buddy_is_allocated_does_not_coalesce() {
        let base = 0x100000;
        let mut a = allocator_with(base, 8 * 4096);
        let first = a.alloc(0).unwrap();
        let live = a.alloc(0).unwrap();
        assert_eq!(first ^ PAGE_SIZE, live, "test needs the two frames to be buddies");
        unsafe { a.free(first, 0) };
        assert_never_handed_out_inside_an_order_one_block(&mut a, live);
    }

    #[test]
    fn caller_data_in_a_live_block_is_not_read_as_a_free_tag() {
        // The values an owner is most likely to leave in a fresh block, plus the
        // seed the tag is built from.
        for garbage in [0u64, u64::MAX, TAG_SEED, NIL - 1] {
            let base = 0x100000;
            let mut a = allocator_with(base, 8 * 4096);
            let live = a.alloc(0).unwrap();
            let buddy = a.alloc(0).unwrap();
            assert_eq!(live ^ PAGE_SIZE, buddy, "test needs the two frames to be buddies");
            // The owner of a live frame writes whatever it likes into it,
            // including over the allocator's former bookkeeping words.
            for off in [OFF_NEXT, OFF_PREV, OFF_TAG] {
                unsafe { a.backing.write_link(live + off, garbage) };
            }
            unsafe { a.free(buddy, 0) };
            assert_never_handed_out_inside_an_order_one_block(&mut a, live);
        }
    }

    #[test]
    #[should_panic(expected = "double free")]
    fn double_free_is_rejected() {
        let mut a = allocator_with(0x100000, 8 * 4096);
        let pa = a.alloc(0).unwrap();
        // Hold its buddy so the first free cannot coalesce `pa` away to a
        // higher order, which would hide the second free from the check.
        let _buddy = a.alloc(0).unwrap();
        unsafe { a.free(pa, 0) };
        unsafe { a.free(pa, 0) };
    }

    #[test]
    fn disjoint_regions_never_coalesce_across_the_hole_between_them() {
        // Two 16 KiB regions separated by a 16 KiB hole; the two regions are
        // order-2 buddies of each other's neighbours across that hole.
        let base = 0x100000;
        let mut a = BuddyAllocator::new(VecBacking::new(base, 12 * 4096));
        unsafe { a.add_region(base, 4 * 4096) };
        unsafe { a.add_region(base + 8 * 4096, 4 * 4096) };
        assert_eq!(a.free_bytes(), 8 * 4096);

        let mut pages = Vec::new();
        while let Some(pa) = a.alloc(0) {
            pages.push(pa);
        }
        assert_eq!(pages.len(), 8);
        for pa in pages {
            unsafe { a.free(pa, 0) };
        }
        assert_eq!(a.free_bytes(), 8 * 4096);
        // Each region can rebuild its own 16 KiB block, but a 32 KiB block
        // would have to span the hole.
        assert!(a.alloc(2).is_some());
        assert!(a.alloc(2).is_some());
        let mut b = BuddyAllocator::new(VecBacking::new(base, 12 * 4096));
        unsafe { b.add_region(base, 4 * 4096) };
        unsafe { b.add_region(base + 8 * 4096, 4 * 4096) };
        assert!(b.alloc(3).is_none(), "coalesced across an unmanaged hole");
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
