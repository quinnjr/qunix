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
//!
//! # Queries answer, mutators assert
//!
//! [`Cache::lookup`], [`Cache::key_of`] and [`Cache::state_of`] take an
//! arbitrary index and answer; the whole point of a query is that the caller
//! does not know yet. Every mutator takes a slot the caller was *handed* by
//! `lookup` or `insert`, so a bad index there is a bug in the caller rather
//! than a condition to report, and it panics. A mutator that quietly did
//! nothing instead would leave a block marked clean that was never written.

/// Which block, on which device.
///
/// A pair, not a block number. Two devices' block 7 are different blocks, and
/// keying on the number alone returns one device's bytes for the other's — a
/// wrong answer, reported as a cache hit, with nothing downstream in a position
/// to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    ///
    /// Also the state a slot is *claimed* in, before the read that fills it has
    /// completed — see [`Cache::insert`].
    InFlight,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    key: BlockKey,
    state: SlotState,
    /// Callers currently holding a reference to this slot's buffer.
    pins: u32,
    /// Set when the buffer was modified while an I/O was outstanding against
    /// it, so [`Cache::end_io`] cannot conclude the slot is clean.
    dirtied_during_io: bool,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            key: BlockKey { dev: 0, block: 0 },
            state: SlotState::Free,
            pins: 0,
            dirtied_during_io: false,
        }
    }
}

/// Whether a slot's memory may be taken for another block.
///
/// One predicate, used by both [`Cache::victim`] and [`Cache::evict`]. Written
/// twice they can disagree, and the disagreement that matters is silent:
/// `victim` offering a slot `evict` would refuse means eviction quietly stops
/// working under pressure, and the cache reports every operation as a success
/// while never reusing anything.
const fn reusable(slot: &Slot) -> bool {
    matches!(slot.state, SlotState::Free | SlotState::Clean) && slot.pins == 0
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
    ///
    /// A hit may be `InFlight`, meaning the buffer is not readable yet. The
    /// slot is still returned because the caller has to be able to *wait* for
    /// it; hiding it instead would send the caller to `insert`, which refuses a
    /// key already resident, leaving it with nothing to wait on. Check
    /// [`Self::state_of`] before reading through a hit.
    pub fn lookup(&self, key: BlockKey) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.state != SlotState::Free && slot.key == key)
    }

    /// Claims a free slot for `key`, or `None` if none is free or `key` is
    /// already resident.
    ///
    /// The claimed slot is `InFlight`, not `Clean`. Nothing has been read into
    /// it yet, so `Clean` — "these contents match the device" — would be a
    /// lie for as long as the fill takes, and the fill is an `await`. A second
    /// caller looking the key up in that window would take the hit and read a
    /// buffer holding whatever the frame held before, with every operation
    /// returning success. [`Self::end_io`] is what makes the slot readable.
    ///
    /// Refusing a resident key is the other half of the same guarantee. Two
    /// slots for one block diverge the moment either is written: `lookup`
    /// answers from one, writeback flushes the other, and a read afterwards
    /// serves bytes that were overwritten. The two refusals share a return
    /// value because a caller reaches `insert` only after `lookup` missed, so
    /// it already knows which one it got.
    pub fn insert(&mut self, key: BlockKey) -> Option<usize> {
        if self.lookup(key).is_some() {
            return None;
        }
        let index = self.slots.iter().position(|slot| slot.state == SlotState::Free)?;
        self.slots[index] =
            Slot { key, state: SlotState::InFlight, pins: 0, dirtied_during_io: false };
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

    /// How many references are held to a slot's buffer.
    ///
    /// Exposed because a pin excludes more than eviction. A caller that hands
    /// out references into the buffer must not overwrite it while one is live,
    /// and the pin count is the only thing that records whether one is.
    pub fn pins_of(&self, slot: usize) -> Option<u32> {
        self.slots.get(slot).map(|s| s.pins)
    }

    /// Records that a slot's contents no longer match the device.
    ///
    /// A slot with an I/O outstanding records the fact rather than changing
    /// state: the state belongs to the device until it completes, and taking it
    /// back would let `sync` issue a second concurrent write against a buffer
    /// the device is already reading. [`Self::end_io`] applies what is recorded
    /// here, which is what stops a write that arrives mid-writeback from being
    /// erased by the writeback's own completion.
    pub fn mark_dirty(&mut self, slot: usize) {
        let slot = &mut self.slots[slot];
        assert!(slot.state != SlotState::Free, "a slot holding no block was dirtied");
        if slot.state == SlotState::InFlight {
            slot.dirtied_during_io = true;
            return;
        }
        slot.state = SlotState::Dirty;
    }

    /// Records that a slot's contents match the device again.
    ///
    /// Refuses a slot with an I/O outstanding. Clearing `InFlight` here would
    /// leave the slot `Clean` and unpinned while the device still owns the
    /// buffer, so [`Self::victim`] would offer it for reuse — the exact
    /// corruption `victim` refuses. `end_io` is the only way out of `InFlight`.
    pub fn mark_clean(&mut self, slot: usize) {
        let slot = &mut self.slots[slot];
        assert!(
            matches!(slot.state, SlotState::Clean | SlotState::Dirty),
            "only a resident slot with no io outstanding can be marked clean"
        );
        slot.state = SlotState::Clean;
    }

    /// Marks an I/O outstanding against a slot, returning what it was before.
    ///
    /// The previous state is returned rather than discarded because the flush
    /// path needs it: a write that fails must leave the slot `Dirty`, not
    /// `Clean`, or the data is dropped and the caller has already been told the
    /// write succeeded.
    pub fn begin_io(&mut self, slot: usize) -> SlotState {
        let slot = &mut self.slots[slot];
        assert!(
            matches!(slot.state, SlotState::Clean | SlotState::Dirty),
            "a slot that is free or already has io outstanding cannot begin another"
        );
        let was = slot.state;
        slot.state = SlotState::InFlight;
        was
    }

    /// Ends an outstanding I/O, putting the slot into `state`.
    ///
    /// A slot dirtied while the I/O ran ends up `Dirty` whatever `state` says.
    /// Honouring `state` there loses the write: the slot reports `Clean`, drops
    /// out of [`Self::dirty_slots`], and no later read through the cache can
    /// reveal it, because the cache answers that read from the same slot.
    pub fn end_io(&mut self, slot: usize, state: SlotState) {
        let slot = &mut self.slots[slot];
        assert!(slot.state == SlotState::InFlight, "no io was outstanding against this slot");
        assert!(state != SlotState::Free, "ending io does not release a slot; `evict` does");
        let dirtied = core::mem::replace(&mut slot.dirtied_during_io, false);
        slot.state = if dirtied { SlotState::Dirty } else { state };
    }

    /// Takes a reference to a slot's buffer, so it cannot be reused.
    pub fn pin(&mut self, slot: usize) {
        let slot = &mut self.slots[slot];
        assert!(slot.state != SlotState::Free, "a slot holding no block was pinned");
        slot.pins += 1;
    }

    /// Releases a reference taken by [`Self::pin`].
    ///
    /// An unbalanced release panics rather than saturating. Saturating hides
    /// the double-unpin: one caller's extra release drops another caller's pin
    /// to zero, `victim` offers the slot while that caller is still reading
    /// through the buffer, and the read returns another block's bytes.
    pub fn unpin(&mut self, slot: usize) {
        let slot = &mut self.slots[slot];
        assert!(slot.pins > 0, "unpinned a slot that was not pinned");
        slot.pins -= 1;
    }

    /// A slot that may be reused, if any.
    ///
    /// Three separate refusals, and they are not interchangeable -- each is a
    /// different corruption:
    ///
    /// - **Dirty**: reusing it discards a write the caller was told succeeded,
    ///   and the block reads as its old contents forever after.
    /// - **InFlight**: the device owns the buffer until it completes. Reusing
    ///   it makes the device write one block's data into another block's
    ///   buffer, with every operation returning success.
    /// - **Pinned**: somebody holds a reference to the buffer and is reading
    ///   through it.
    ///
    /// `Free` slots first, so an untouched table is filled before anything is
    /// evicted.
    pub fn victim(&self) -> Option<usize> {
        if let Some(free) = self.slots.iter().position(|s| s.state == SlotState::Free) {
            return Some(free);
        }
        self.slots.iter().position(reusable)
    }

    /// Returns a slot to the free list, or refuses.
    ///
    /// Refuses exactly what [`Self::victim`] refuses, through the same
    /// predicate. Without this the table can never be reused at all: `victim`
    /// names a slot and nothing can act on the name.
    pub fn evict(&mut self, slot: usize) -> bool {
        let slot = &mut self.slots[slot];
        if !reusable(slot) {
            return false;
        }
        *slot = Slot::empty();
        true
    }

    /// Every slot holding a block that must be written back.
    ///
    /// What `sync` writes. A dirty slot missing from this is a write that was
    /// acknowledged and never reached the disk -- which no later read through
    /// the cache can reveal, because the cache answers it from the same slot.
    pub fn dirty_slots(&self) -> impl Iterator<Item = usize> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == SlotState::Dirty)
            .map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot that has been claimed *and filled*, which is what most of these
    /// tests want. `insert` alone leaves the slot `InFlight`, deliberately.
    fn filled<const N: usize>(cache: &mut Cache<N>, dev: u32, block: u64) -> usize {
        let slot = cache.insert(BlockKey { dev, block }).expect("the table refused a fill");
        cache.end_io(slot, SlotState::Clean);
        slot
    }

    #[test]
    fn two_devices_with_the_same_block_number_are_different_blocks() {
        // The key is a pair. Collapsing it to the block number returns one
        // device's data for another's -- a wrong answer, reported as a hit.
        let mut cache = Cache::<4>::new();
        let a = filled(&mut cache, 0, 7);
        let b = filled(&mut cache, 1, 7);
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
    fn a_slot_that_is_still_being_filled_does_not_claim_to_match_the_device() {
        // The window between claiming a slot and the read landing in it is an
        // `await`. A slot marked `Clean` there is a hit on a buffer holding
        // whatever the frame held before, and every operation returns success.
        let mut cache = Cache::<2>::new();
        let key = BlockKey { dev: 0, block: 1 };
        let slot = cache.insert(key).unwrap();
        assert_eq!(cache.state_of(slot), Some(SlotState::InFlight), "an unfilled slot read clean");
        // And it is not evictable while the read into it is outstanding.
        assert_eq!(cache.victim(), Some(1), "the slot being filled was offered for reuse");
        cache.end_io(slot, SlotState::Clean);
        assert_eq!(cache.state_of(slot), Some(SlotState::Clean));
    }

    #[test]
    fn a_key_that_is_already_cached_is_never_given_a_second_slot() {
        // Two slots for one block diverge the moment either is written:
        // `lookup` answers from one, writeback flushes the other, and the read
        // afterwards serves bytes that were overwritten.
        let mut cache = Cache::<4>::new();
        let key = BlockKey { dev: 0, block: 1 };
        let first = filled(&mut cache, 0, 1);
        assert_eq!(cache.insert(key), None, "a resident block was given a second slot");
        // Including while the first is still being filled -- the window a
        // second caller is most likely to arrive in.
        let other = cache.insert(BlockKey { dev: 0, block: 2 }).unwrap();
        assert_eq!(cache.insert(BlockKey { dev: 0, block: 2 }), None);
        assert_eq!(cache.lookup(BlockKey { dev: 0, block: 2 }), Some(other));
        assert_eq!(cache.lookup(key), Some(first));
    }

    #[test]
    fn a_dirty_slot_is_never_chosen_as_a_victim() {
        // Evicting a dirty slot discards a write the caller was told
        // succeeded, and the block reads as its old contents forever after.
        // Nothing downstream is in a position to notice.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        let b = filled(&mut cache, 0, 2);
        cache.mark_dirty(a);
        cache.mark_dirty(b);
        assert_eq!(cache.victim(), None, "a dirty slot was offered for reuse");
        assert!(!cache.evict(a), "a dirty slot was evicted on request");
    }

    #[test]
    fn a_slot_with_io_outstanding_is_never_chosen_as_a_victim() {
        // The device owns the buffer until it completes. Reusing it is the
        // driver's own bug one layer up: the device writes one block's data
        // into another block's buffer and every operation returns success.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        let b = filled(&mut cache, 0, 2);
        cache.begin_io(a);
        assert_eq!(cache.victim(), Some(b), "the one reusable slot was not offered");
        cache.begin_io(b);
        assert_eq!(cache.victim(), None, "a slot with io outstanding was offered for reuse");
        assert!(!cache.evict(a), "a slot the device owns was evicted on request");
    }

    #[test]
    fn a_pinned_slot_is_never_chosen_as_a_victim() {
        // A pin means somebody is reading through the buffer right now.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        let b = filled(&mut cache, 0, 2);
        cache.pin(a);
        cache.pin(b);
        assert_eq!(cache.victim(), None, "a pinned slot was offered for reuse");
        assert!(!cache.evict(a), "a pinned slot was evicted on request");
        cache.unpin(b);
        assert_eq!(cache.victim(), Some(b), "unpinning did not release the slot");
    }

    #[test]
    fn evict_refuses_exactly_what_victim_refuses() {
        // The two are written against one predicate because a disagreement is
        // silent: `victim` naming a slot `evict` then refuses means eviction
        // quietly stops working under pressure, with every call succeeding.
        //
        // Checked per slot rather than against `victim()`'s answer, which names
        // only the first reusable slot and so would leave every other slot's
        // refusal unasserted.
        let mut cache = Cache::<4>::new();
        let dirty = filled(&mut cache, 0, 1);
        cache.mark_dirty(dirty);
        let busy = filled(&mut cache, 0, 2);
        cache.begin_io(busy);
        let pinned = filled(&mut cache, 0, 3);
        cache.pin(pinned);
        let clean = filled(&mut cache, 0, 4);

        // The table is full and only one slot is reusable, so `victim` has
        // exactly one right answer and every other slot must be refused.
        assert_eq!(cache.victim(), Some(clean), "the one reusable slot was not the one offered");
        for slot in [dirty, busy, pinned] {
            assert!(!cache.evict(slot), "slot {slot} was refused by victim and evicted anyway");
            assert_ne!(cache.state_of(slot), Some(SlotState::Free));
        }
        assert!(cache.evict(clean), "the slot victim offered refused to be evicted");
        assert_eq!(cache.state_of(clean), Some(SlotState::Free));
        assert_eq!(cache.victim(), Some(clean), "the freed slot was not offered next");
    }

    #[test]
    fn a_free_slot_is_taken_before_a_clean_one_is_evicted() {
        // Filling before evicting. The other order throws away a cached block
        // while a frame the kernel reserved sits unused, which is invisible --
        // it costs hit rate, not correctness, and so nothing else would catch
        // it.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        let free = cache.victim().expect("a half-empty table offered no slot");
        assert_ne!(free, a, "an occupied slot was evicted while one was free");
    }

    #[test]
    fn begin_io_reports_the_state_it_replaced() {
        // The flush path needs it: a write that fails must leave the slot
        // `Dirty`, not `Clean`, or the data is dropped after the caller has
        // been told the write succeeded.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        cache.mark_dirty(a);
        assert_eq!(cache.begin_io(a), SlotState::Dirty, "the replaced state was lost");
        cache.end_io(a, SlotState::Dirty);
        assert_eq!(cache.state_of(a), Some(SlotState::Dirty));
    }

    #[test]
    fn a_write_that_arrives_during_writeback_is_not_lost() {
        // The writeback flushed the bytes as they were when it started. A
        // write landing after that is not on the disk, so concluding `Clean`
        // when it completes drops it -- and the slot leaves `dirty_slots`, so
        // no later flush picks it up and no read reveals it either.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        cache.mark_dirty(a);
        cache.begin_io(a);
        cache.mark_dirty(a);
        cache.end_io(a, SlotState::Clean);
        assert_eq!(cache.state_of(a), Some(SlotState::Dirty), "a write during writeback was lost");
        assert!(cache.dirty_slots().eq([a]), "the re-dirtied slot is not queued for writeback");
        // And the flag does not persist: a clean writeback after it settles.
        cache.begin_io(a);
        cache.end_io(a, SlotState::Clean);
        assert_eq!(cache.state_of(a), Some(SlotState::Clean), "the slot can never come clean");
    }

    #[test]
    #[should_panic(expected = "no io outstanding")]
    fn a_slot_the_device_owns_cannot_be_marked_clean() {
        // Clearing `InFlight` here leaves the slot clean and unpinned while the
        // device still owns the buffer, so `victim` offers it for reuse.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        cache.begin_io(a);
        cache.mark_clean(a);
    }

    #[test]
    #[should_panic(expected = "not pinned")]
    fn an_unbalanced_unpin_is_a_bug_rather_than_a_no_op() {
        // Saturating hides the double-unpin: one caller's extra release drops
        // another caller's pin to zero and `victim` offers the slot while that
        // caller is still reading through the buffer.
        let mut cache = Cache::<2>::new();
        let a = filled(&mut cache, 0, 1);
        cache.pin(a);
        cache.unpin(a);
        cache.unpin(a);
    }

    #[test]
    fn every_dirty_slot_is_reported_for_writeback() {
        // `sync` writes what this reports. A dirty slot missing from it is a
        // write that was acknowledged and never reached the disk -- and no
        // later read through the cache can reveal that, because the cache
        // answers it from the same slot.
        let mut cache = Cache::<4>::new();
        let a = filled(&mut cache, 0, 1);
        let b = filled(&mut cache, 0, 2);
        filled(&mut cache, 0, 3);
        cache.mark_dirty(a);
        cache.mark_dirty(b);
        // Compared as an iterator rather than collected: `Vec` is the one
        // thing in this file that would need an allocator, and the crate has
        // no business acquiring one for a test.
        assert!(cache.dirty_slots().eq([a, b]), "the dirty set does not match what was dirtied");
        cache.mark_clean(a);
        assert!(cache.dirty_slots().eq([b]), "a cleaned slot was still reported dirty");
    }

    #[test]
    fn a_free_slot_is_not_a_hit_even_when_its_leftover_key_matches() {
        // `Free` means the memory holds nothing; the key still in the slot is
        // the previous occupant's. Matching on the key alone would return a
        // buffer whose contents are whatever the last block left there --
        // which is precisely a hit that serves the wrong bytes.
        let mut cache = Cache::<1>::new();
        let key = BlockKey { dev: 0, block: 3 };
        let slot = filled(&mut cache, key.dev, key.block);
        assert!(cache.evict(slot), "a clean unpinned slot refused eviction");
        assert_eq!(cache.lookup(key), None, "a freed slot was reported as a hit");
    }

    #[test]
    fn the_pin_count_is_reported_and_a_freed_slot_reports_none() {
        // A pin excludes overwriting the buffer, not only evicting the slot, so
        // a caller that hands out references needs to read the count rather
        // than infer it from `victim` declining.
        let mut cache = Cache::<2>::new();
        let a = cache.insert(BlockKey { dev: 0, block: 1 }).unwrap();
        cache.end_io(a, SlotState::Clean);
        assert_eq!(cache.pins_of(a), Some(0));
        cache.pin(a);
        cache.pin(a);
        assert_eq!(cache.pins_of(a), Some(2), "the second pin was not counted");
        cache.unpin(a);
        assert_eq!(cache.pins_of(a), Some(1), "one unpin released both pins");
        assert_eq!(cache.pins_of(Cache::<2>::CAPACITY), None, "a slot past the end reported pins");
    }

    #[test]
    fn a_freed_slot_reports_no_key_and_a_slot_past_the_end_reports_nothing() {
        // The same hazard as `lookup`, reached from the other side. `key_of`
        // is how a caller learns which block a slot it already holds is for --
        // writeback asks it before issuing the write. A freed slot answering
        // with its previous occupant's key sends that slot's bytes to the
        // wrong block number, and the write succeeds.
        let mut cache = Cache::<2>::default();
        let key = BlockKey { dev: 3, block: 11 };
        let slot = filled(&mut cache, key.dev, key.block);
        assert_eq!(cache.key_of(slot), Some(key));
        assert!(cache.evict(slot));
        assert_eq!(cache.key_of(slot), None, "a freed slot named its old block");
        assert_eq!(cache.key_of(Cache::<2>::CAPACITY), None, "a slot past the end named a block");
        assert_eq!(cache.state_of(Cache::<2>::CAPACITY), None);
    }

    #[test]
    fn a_full_table_refuses_rather_than_overwriting_an_occupant() {
        // `insert` does not evict. A version that quietly reused an occupied
        // slot would discard whatever it held -- including a dirty block --
        // and the caller asking only for a free slot would never know.
        let mut cache = Cache::<2>::new();
        filled(&mut cache, 0, 1);
        filled(&mut cache, 0, 2);
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
