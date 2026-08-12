#![cfg_attr(not(any(test, feature = "std")), no_std)]

//! The buffer cache's bookkeeping: which slot holds which block, and whether
//! that slot may be reused.
//!
//! Deliberately knows nothing about frames, DMA, futures or the block device.
//! A slot here is an index and a little state; the kernel owns the memory it
//! names. That separation is what lets this be tested on the host, and it is
//! worth having because of *how* this code fails.
//!
//! A cache that hands back the wrong slot does not crash. It returns another
//! block's bytes, and every caller above it — the VFS, then a filesystem, then
//! a program — treats them as the block it asked for. The failure is a wrong
//! answer, so the test has to be an assertion about the answer rather than the
//! absence of a fault.
//!
//! # The table is fixed, and that is a requirement rather than a simplification
//!
//! Memory pressure triggers writeback, writeback needs I/O, and I/O needs the
//! allocation that is already waiting. So the flush path may not call the
//! general allocator, and a table that can grow is a table that can allocate
//! while flushing. The capacity is a const parameter for that reason: there is
//! no `Vec` here to grow, and there is nowhere for one to appear later without
//! the type changing.

/// Which block, on which device.
///
/// A pair, not a block number. Two devices' block 7 are different blocks, and
/// keying on the number alone returns one device's bytes for the other's — a
/// wrong answer, reported as a cache hit, with nothing downstream in a position
/// to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockKey {
    pub dev: u32,
    pub block: u64,
}

/// What a slot holds, and what may be done with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Holds no block. Its memory is free for any key.
    Free,
    /// Holds a block whose contents match the device.
    Clean,
    /// Holds a block that differs from the device and must be written before
    /// the slot is reused.
    Dirty,
    /// An I/O is outstanding against this slot; the device owns its memory.
    InFlight,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    key: BlockKey,
    state: SlotState,
    /// Callers currently holding a reference to this slot's buffer.
    pins: u32,
}

impl Slot {
    const fn empty() -> Self {
        Self { key: BlockKey { dev: 0, block: 0 }, state: SlotState::Free, pins: 0 }
    }
}

/// A fixed table of `N` slots, keyed by `(dev, block)`.
pub struct Cache<const N: usize> {
    slots: [Slot; N],
}

impl<const N: usize> Default for Cache<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Cache<N> {
    /// Slots in this cache.
    pub const CAPACITY: usize = N;

    pub const fn new() -> Self {
        Self { slots: [Slot::empty(); N] }
    }

    /// The slot holding `key`, if one does.
    ///
    /// A `Free` slot is never a hit whatever its key says: `Free` means the
    /// memory holds nothing, and the key left behind in it is the previous
    /// occupant's. Matching on the key alone would return a slot whose contents
    /// are whatever the last block put there.
    pub fn lookup(&self, key: BlockKey) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.state != SlotState::Free && slot.key == key)
    }

    /// Claims a free slot for `key`, or `None` if none is free.
    ///
    /// Does not evict: choosing a victim is a separate decision with its own
    /// refusals, and folding the two together would let a caller that only
    /// wanted a free slot silently discard somebody else's dirty block.
    pub fn insert(&mut self, key: BlockKey) -> Option<usize> {
        let index = self.slots.iter().position(|slot| slot.state == SlotState::Free)?;
        self.slots[index] = Slot { key, state: SlotState::Clean, pins: 0 };
        Some(index)
    }

    /// The key a slot holds.
    pub fn key_of(&self, slot: usize) -> Option<BlockKey> {
        self.slots.get(slot).filter(|s| s.state != SlotState::Free).map(|s| s.key)
    }

    /// The state a slot is in.
    pub fn state_of(&self, slot: usize) -> Option<SlotState> {
        self.slots.get(slot).map(|s| s.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_devices_with_the_same_block_number_are_different_blocks() {
        // The key is a pair. Collapsing it to the block number returns one
        // device's data for another's -- a wrong answer, reported as a hit.
        let mut cache = Cache::<4>::new();
        let a = cache.insert(BlockKey { dev: 0, block: 7 }).expect("a fresh cache has slots");
        let b = cache.insert(BlockKey { dev: 1, block: 7 }).expect("a fresh cache has slots");
        assert_ne!(a, b, "two devices' block 7 landed in one slot");
        assert_eq!(cache.lookup(BlockKey { dev: 0, block: 7 }), Some(a));
        assert_eq!(cache.lookup(BlockKey { dev: 1, block: 7 }), Some(b));
    }

    #[test]
    fn a_block_that_was_never_inserted_is_a_miss() {
        // The direction that matters. A spurious *hit* hands out a slot holding
        // some other block's bytes, and nothing above re-checks.
        let cache = Cache::<4>::new();
        assert_eq!(cache.lookup(BlockKey { dev: 0, block: 0 }), None);
        assert_eq!(cache.lookup(BlockKey { dev: 9, block: 9 }), None);
    }

    #[test]
    fn a_free_slot_is_not_a_hit_even_when_its_leftover_key_matches() {
        // `Free` means the memory holds nothing; the key still in the slot is
        // the previous occupant's. Matching on the key alone would return a
        // buffer whose contents are whatever the last block left there --
        // which is precisely a hit that serves the wrong bytes.
        let mut cache = Cache::<1>::new();
        let key = BlockKey { dev: 0, block: 3 };
        let slot = cache.insert(key).expect("a fresh cache has slots");
        cache.release_for_test(slot);
        assert_eq!(cache.lookup(key), None, "a freed slot was reported as a hit");
    }

    #[test]
    fn a_full_table_refuses_rather_than_overwriting_an_occupant() {
        // `insert` does not evict. A version that quietly reused an occupied
        // slot would discard whatever it held -- including a dirty block --
        // and the caller asking only for a free slot would never know.
        let mut cache = Cache::<2>::new();
        assert!(cache.insert(BlockKey { dev: 0, block: 1 }).is_some());
        assert!(cache.insert(BlockKey { dev: 0, block: 2 }).is_some());
        assert_eq!(
            cache.insert(BlockKey { dev: 0, block: 3 }),
            None,
            "a full table handed out a slot that was already occupied"
        );
        // And the occupants are still there.
        assert!(cache.lookup(BlockKey { dev: 0, block: 1 }).is_some());
        assert!(cache.lookup(BlockKey { dev: 0, block: 2 }).is_some());
    }

    #[test]
    fn every_slot_in_the_table_can_be_used() {
        // Off-by-one in either direction is silent: one slot short wastes a
        // frame the kernel reserved, one slot long indexes past the frames.
        let mut cache = Cache::<8>::new();
        for block in 0..8u64 {
            assert!(
                cache.insert(BlockKey { dev: 0, block }).is_some(),
                "the table refused block {block} with capacity 8"
            );
        }
        assert_eq!(Cache::<8>::CAPACITY, 8);
    }
}

#[cfg(test)]
impl<const N: usize> Cache<N> {
    /// Returns a slot to the free list, for tests that need a freed slot.
    fn release_for_test(&mut self, slot: usize) {
        self.slots[slot].state = SlotState::Free;
    }
}
