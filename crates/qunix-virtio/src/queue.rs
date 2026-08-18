//! The driver's half of a split virtqueue.
//!
//! Owns the descriptor table, the available ring and a mirror of the used ring
//! as plain memory. The kernel copies these into DMA memory and hands the
//! device their physical addresses; nothing here dereferences one.

use alloc::vec;
use alloc::vec::Vec;

use crate::{DESC_F_NEXT, Descriptor, RING_HEADER, ring_layout};

/// A descriptor index meaning "no descriptor".
///
/// `u16::MAX` rather than 0, because 0 is a perfectly good descriptor and a
/// sentinel that collides with a real value is how a free list comes to contain
/// a live entry.
const NIL: u16 = u16::MAX;

/// One entry of the used ring, as the device writes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct UsedElem {
    /// The *head* of the chain that completed.
    pub id: u32,
    /// Bytes the device wrote into the chain's device-writable buffers.
    pub len: u32,
}

/// The driver's view of one split virtqueue.
pub struct SplitQueue {
    size: u16,
    desc: Vec<Descriptor>,
    /// Head of the free-descriptor list, chained through `desc[i].next`.
    free_head: u16,
    /// Free descriptors. Tracked separately rather than derived by walking the
    /// list, so a corrupted chain cannot present as a queue that is merely
    /// full — the two disagreeing is a bug that says so.
    free: u16,
    /// Whether a chain headed here has been published and not yet completed.
    ///
    /// The used ring is written by the *device*, so the id in a completion is
    /// device-supplied input. This is what a completion is checked against: an
    /// id naming a descriptor that was never submitted means the device
    /// completed something the driver did not ask for, and freeing its "chain"
    /// would walk `next` links that mean nothing.
    in_flight: Vec<bool>,
    /// The available ring's free-running index. Counts every entry ever
    /// published and wraps at 65536; the *slot* is this modulo `size`.
    avail_idx: u16,
    avail_ring: Vec<u16>,
    used_ring: Vec<UsedElem>,
}

impl SplitQueue {
    /// A queue of `size` descriptors, all free.
    ///
    /// # Panics
    /// If `size` is zero or not a power of two. The spec requires a power of
    /// two so the device can reduce an index with a mask, and a zero-sized
    /// queue would make every allocation fail in a way that reads like
    /// exhaustion.
    pub fn new(size: u16) -> Self {
        assert!(size > 0, "a queue of zero descriptors accepts nothing");
        assert!(size.is_power_of_two(), "queue size {size} is not a power of two");
        let mut desc = vec![Descriptor::default(); size as usize];
        // Every descriptor on the free list, chained in order. The last points
        // at NIL rather than wrapping to 0, so exhaustion is detected by the
        // sentinel as well as by the count.
        for (i, d) in desc.iter_mut().enumerate() {
            d.next = if i + 1 < size as usize { i as u16 + 1 } else { NIL };
        }
        Self {
            size,
            desc,
            free_head: 0,
            free: size,
            in_flight: vec![false; size as usize],
            avail_idx: 0,
            avail_ring: vec![0; size as usize],
            used_ring: vec![UsedElem::default(); size as usize],
        }
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn free_count(&self) -> u16 {
        self.free
    }

    /// Takes `n` descriptors as one chain, or nothing at all.
    ///
    /// All or nothing, deliberately. A partial chain is a request whose data
    /// buffer is missing: the device reads the header, finds no `NEXT`, and
    /// completes a transfer of nothing — successfully. So the count is checked
    /// before any descriptor leaves the free list.
    pub fn alloc_chain(&mut self, n: u16) -> Option<u16> {
        if n == 0 || n > self.free {
            return None;
        }
        let head = self.free_head;
        let mut prev = NIL;
        for _ in 0..n {
            let idx = self.free_head;
            // The free list ending early while the count says otherwise means
            // the two have drifted, which is a corrupted chain rather than an
            // empty one. Loud, because the alternative is handing out `NIL` as
            // a descriptor index.
            assert_ne!(idx, NIL, "the free list ran out with {} descriptors counted", self.free);
            self.free_head = self.desc[idx as usize].next;
            self.free -= 1;
            if prev != NIL {
                self.desc[prev as usize].flags |= DESC_F_NEXT;
                self.desc[prev as usize].next = idx;
            }
            prev = idx;
        }
        // The tail terminates the chain: a stale NEXT flag here would send the
        // device walking into a descriptor belonging to someone else.
        self.desc[prev as usize].flags &= !DESC_F_NEXT;
        self.desc[prev as usize].next = 0;
        Some(head)
    }

    /// Returns every descriptor in a chain to the free list.
    ///
    /// Every one, not just the head: releasing only the head leaks the rest,
    /// and the queue runs dry after `size / chain_len` requests — long after
    /// the code that leaked them ran.
    pub fn free_chain(&mut self, head: u16) {
        let mut idx = head;
        for _ in 0..self.size {
            let d = self.desc[idx as usize];
            let more = d.flags & DESC_F_NEXT != 0;
            let next = d.next;
            self.desc[idx as usize] = Descriptor { next: self.free_head, ..Default::default() };
            self.free_head = idx;
            self.free += 1;
            if !more {
                break;
            }
            idx = next;
        }
        self.in_flight[head as usize] = false;
    }

    /// Fills in one descriptor of a chain.
    ///
    /// Takes the index rather than the position in the chain, because the
    /// caller walks the chain it allocated and knows which is which.
    pub fn describe(&mut self, idx: u16, addr: u64, len: u32, device_writable: bool) {
        let next_flag = self.desc[idx as usize].flags & DESC_F_NEXT;
        self.desc[idx as usize].addr = addr;
        self.desc[idx as usize].len = len;
        self.desc[idx as usize].flags =
            next_flag | if device_writable { crate::DESC_F_WRITE } else { 0 };
    }

    /// The descriptor after `idx` in its chain, if any.
    pub fn next_in_chain(&self, idx: u16) -> Option<u16> {
        let d = self.desc[idx as usize];
        (d.flags & DESC_F_NEXT != 0).then_some(d.next)
    }

    /// Publishes a chain to the device, returning the new available index.
    ///
    /// The two indices are different things and this is where conflating them
    /// would show: `avail_idx` counts every entry ever published and wraps at
    /// 65536, while the *slot* it occupies is that count modulo the queue size.
    /// Writing at `avail_idx` itself would land outside a 64-entry ring on the
    /// 64th publish and stay outside forever after.
    pub fn publish(&mut self, head: u16) -> u16 {
        // In flight from *here*, not from `alloc_chain`. A chain the driver has
        // allocated but not yet handed to the device cannot legitimately
        // complete, and the used ring is device-written: marking it in flight at
        // allocation makes `take_used` accept a forged completion for an id the
        // device was never told about, and the submitter then reads a bounce
        // buffer holding the previous request through that slot.
        self.in_flight[head as usize] = true;
        let slot = self.avail_idx % self.size;
        self.avail_ring[slot as usize] = head;
        // Wrapping, not saturating: the index is defined to wrap, and the
        // device reduces it the same way. A saturating counter stops advancing
        // after 65535 publishes and the device never sees another request.
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.avail_idx
    }

    /// The ring slot the last [`publish`](Self::publish) wrote.
    pub fn last_slot(&self) -> u16 {
        self.avail_idx.wrapping_sub(1) % self.size
    }

    pub fn avail_index(&self) -> u16 {
        self.avail_idx
    }

    /// Takes the completion in used-ring slot `slot`, if it names a live chain.
    ///
    /// The used ring is written by the device, so its contents are
    /// device-supplied input in exactly the sense the ELF loader's input is.
    /// Two things are checked, and neither is paranoia:
    ///
    /// - an id past the end of the descriptor table would index out of bounds;
    /// - an id naming a descriptor that is not in flight means the device
    ///   completed something never submitted, and freeing that "chain" would
    ///   walk `next` links belonging to a live request. "Submitted" means
    ///   *published*: a chain the driver has allocated and not yet handed over
    ///   cannot have completed, and accepting one lets a device report a
    ///   request the driver never made.
    ///
    /// Clearing `in_flight` here is what makes a completion consumable exactly
    /// once. Taking one twice frees its chain twice, which puts one descriptor
    /// on the free list twice — and the next two allocations hand the same
    /// descriptor to two different requests.
    pub fn take_used(&mut self, slot: u16) -> Option<(u16, u32)> {
        let elem = self.used_ring[(slot % self.size) as usize];
        if elem.id >= self.size as u32 {
            return None;
        }
        let head = elem.id as u16;
        if !self.in_flight[head as usize] {
            return None;
        }
        self.in_flight[head as usize] = false;
        Some((head, elem.len))
    }

    /// The descriptor table as the device reads it.
    pub fn desc_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.desc.len() * Descriptor::BYTES);
        for d in &self.desc {
            out.extend_from_slice(&d.to_bytes());
        }
        out
    }

    /// One descriptor, as the device reads it.
    ///
    /// The per-descriptor form exists because a request touches three of them
    /// and the whole-table form serialises all 64 -- under the driver's lock,
    /// with interrupts masked, on every submission.
    pub fn descriptor_bytes(&self, index: u16) -> Option<[u8; Descriptor::BYTES]> {
        self.desc.get(index as usize).map(|d| d.to_bytes())
    }

    /// One available-ring slot, as the device reads it.
    ///
    /// `slot` is a ring position, not the free-running index: the two are
    /// different numbers and conflating them writes outside the ring for any
    /// queue smaller than 65536, which is every queue.
    pub fn avail_slot_bytes(&self, slot: u16) -> Option<[u8; 2]> {
        self.avail_ring.get(slot as usize).map(|s| s.to_le_bytes())
    }

    /// The available ring's *slots*, without its index.
    ///
    /// The index is deliberately excluded, because publishing it is a separate
    /// step with a barrier in front of it. The spec's order is: write the
    /// descriptor and the ring slot, barrier, then advance `idx` — the index is
    /// what makes the slot valid, so a device that polls the ring can otherwise
    /// see an incremented index over a slot still holding its previous
    /// occupant, and fetch a descriptor belonging to a request that has already
    /// been freed. Returning them together in one blob made that ordering
    /// impossible to express.
    pub fn avail_slots_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 * self.avail_ring.len() + 2);
        for slot in &self.avail_ring {
            out.extend_from_slice(&slot.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // used_event
        out
    }

    /// Byte offset of the available ring's index within the ring structure.
    pub const AVAIL_IDX_OFFSET: usize = 2;
    /// Byte offset of the available ring's first slot.
    pub const AVAIL_SLOTS_OFFSET: usize = RING_HEADER;

    /// Copies the device's used ring in from an already-taken snapshot.
    ///
    /// Takes `idx` as a parameter rather than reading it out of `bytes`,
    /// because the *order* of those two reads is the whole protocol: the driver
    /// must load `used.idx`, acquire-fence, and only then read the entries it
    /// gates. Deriving both from one slice leaves the compiler free to schedule
    /// the entry loads first, and a stale entry whose head has since been
    /// recycled passes every liveness check the driver has.
    ///
    /// The caller is also responsible for reading that memory *volatilely*. A
    /// `&[u8]` over a region the device is writing tells the compiler the bytes
    /// do not change for the reference's lifetime, which is not true and is not
    /// a promise the driver can make.
    pub fn ingest_used(&mut self, bytes: &[u8], idx: u16) -> Option<u16> {
        if bytes.len() < RING_HEADER + 8 * self.size as usize {
            return None;
        }
        for slot in 0..self.size as usize {
            let base = RING_HEADER + slot * 8;
            self.used_ring[slot] = UsedElem {
                id: u32::from_le_bytes(bytes[base..base + 4].try_into().ok()?),
                len: u32::from_le_bytes(bytes[base + 4..base + 8].try_into().ok()?),
            };
        }
        Some(idx)
    }

    /// Copies one used-ring entry the device wrote into the driver's mirror.
    ///
    /// The per-entry form exists because the whole-ring one is the wrong shape
    /// for an interrupt: a completion typically brings one entry, and copying
    /// all of them re-reads every slot the device may be writing *right now*.
    /// Rejecting an out-of-range slot rather than wrapping it is deliberate --
    /// the caller derives it from a device-supplied index, and silently
    /// folding a bad one onto a live entry marks the wrong request done.
    pub fn ingest_used_slot(&mut self, slot: u16, bytes: [u8; 8]) -> bool {
        if slot >= self.size {
            return false;
        }
        self.used_ring[slot as usize] = UsedElem {
            id: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            len: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        };
        true
    }

    /// The byte length of the ring allocation this queue needs.
    pub fn ring_bytes(&self) -> usize {
        ring_layout(self.size).bytes
    }

    #[cfg(test)]
    pub fn set_avail_index_for_test(&mut self, idx: u16) {
        self.avail_idx = idx;
    }

    #[cfg(test)]
    pub fn write_used_for_test(&mut self, slot: u16, id: u16, len: u32) {
        self.used_ring[(slot % self.size) as usize] = UsedElem { id: id as u32, len };
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_completion_for_a_chain_that_was_never_published_is_refused() {
        // The used ring is device-written, so an id in it is device-supplied.
        // A chain the driver has allocated but not yet handed over cannot have
        // completed -- the device has not been told it exists. Accepting one
        // lets a device report a request the driver never made, and the
        // submitter then reads the bounce buffer as if it held its own data.
        let mut queue = SplitQueue::new(8);
        let head = queue.alloc_chain(2).expect("a fresh queue refused a chain");
        let mut used = [0u8; 8];
        used[..4].copy_from_slice(&(head as u32).to_le_bytes());
        used[4..].copy_from_slice(&512u32.to_le_bytes());
        assert!(queue.ingest_used_slot(0, used));
        assert_eq!(
            queue.take_used(0),
            None,
            "a completion for an unpublished chain was accepted"
        );
        // Once published, the same completion is legitimate.
        queue.publish(head);
        assert_eq!(queue.take_used(0), Some((head, 512)));
        // And exactly once.
        assert_eq!(queue.take_used(0), None, "the completion was consumable twice");
    }
    use super::*;
    use crate::QUEUE_SIZE;

    #[test]
    fn a_queue_hands_out_every_descriptor_and_then_refuses() {
        // The refusal is the point. A queue that wrapped and reused a live
        // descriptor would overwrite a request the device is still reading --
        // and the device would complete the *wrong* request, reporting success.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let mut heads = alloc::vec![];
        for _ in 0..QUEUE_SIZE {
            heads.push(q.alloc_chain(1).expect("a free descriptor was refused"));
        }
        assert_eq!(q.free_count(), 0);
        assert_eq!(q.alloc_chain(1), None, "a full queue handed out a live descriptor");
        // Every index distinct: a free list that linked a descriptor to itself
        // would hand the same one out twice while the count still looked right.
        heads.sort_unstable();
        heads.dedup();
        assert_eq!(heads.len(), QUEUE_SIZE as usize, "a descriptor was handed out twice");
    }

    #[test]
    fn a_chain_longer_than_the_queue_is_refused_rather_than_truncated() {
        // A truncated chain is a request whose data buffer is missing: the
        // device reads the header, finds no NEXT, and completes a request that
        // transferred nothing -- successfully.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        assert_eq!(q.alloc_chain(QUEUE_SIZE + 1), None);
        assert_eq!(q.free_count(), QUEUE_SIZE, "the refused chain consumed descriptors");
        // And zero, which would otherwise return a head naming a descriptor the
        // caller never asked for and never fills in.
        assert_eq!(q.alloc_chain(0), None, "a zero-length chain was allocated");
    }

    #[test]
    fn freeing_a_chain_returns_every_descriptor_in_it() {
        // Not just the head. A free that released only the head leaks the rest,
        // and the queue runs dry after a third of the requests it should carry
        // -- long after the code that leaked them ran.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(3).unwrap();
        assert_eq!(q.free_count(), QUEUE_SIZE - 3);
        q.free_chain(head);
        assert_eq!(q.free_count(), QUEUE_SIZE, "freeing a chain leaked descriptors");
        // And the queue is genuinely reusable afterwards, which a count alone
        // does not show: a free list whose tail points into itself would report
        // the right number and hand out the same descriptor repeatedly.
        let mut seen = alloc::vec![];
        for _ in 0..QUEUE_SIZE {
            seen.push(q.alloc_chain(1).expect("a freed descriptor was not reusable"));
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), QUEUE_SIZE as usize, "the free list handed one out twice");
    }

    #[test]
    fn a_chain_is_linked_head_to_tail_and_terminates() {
        // The device follows NEXT until a descriptor without the flag. A chain
        // whose tail kept a stale flag sends it walking into a descriptor that
        // belongs to another request.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(3).unwrap();
        let mut walked = alloc::vec![head];
        let mut idx = head;
        while let Some(next) = q.next_in_chain(idx) {
            walked.push(next);
            idx = next;
            assert!(walked.len() <= 3, "the chain did not terminate after three descriptors");
        }
        assert_eq!(walked.len(), 3, "the chain is shorter than it was asked to be");
    }

    #[test]
    fn the_available_index_wraps_at_the_ring_size_but_counts_past_it() {
        // The subtle one, and the reason this crate exists. The available
        // *index* is a free-running u16 that the device reduces modulo the
        // queue size; the *slot* it names wraps at the queue size. Conflating
        // them makes the driver publish at `idx`, which is outside the ring for
        // any queue smaller than 65536 -- i.e. always.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        for i in 0..(QUEUE_SIZE as u32 * 3) {
            let head = q.alloc_chain(1).unwrap();
            let idx = q.publish(head);
            assert_eq!(idx as u32, i + 1, "the available index did not advance monotonically");
            assert_eq!(
                q.last_slot() as u32,
                i % QUEUE_SIZE as u32,
                "the published slot did not wrap at the queue size"
            );
            q.free_chain(head);
        }
    }

    #[test]
    fn the_available_index_survives_a_u16_wrap() {
        // 65536 requests is minutes of filesystem traffic, and the wrap is
        // where a saturating counter turns into a queue that silently stops
        // publishing. Driven directly rather than by 65536 round trips.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        q.set_avail_index_for_test(u16::MAX);
        let head = q.alloc_chain(1).unwrap();
        assert_eq!(q.publish(head), 0, "the available index did not wrap to zero");
        assert_eq!(q.last_slot(), u16::MAX % QUEUE_SIZE, "the slot was wrong across the wrap");
    }

    #[test]
    fn a_used_entry_naming_a_descriptor_outside_the_ring_is_refused() {
        // The device writes the used ring, so its contents are device-supplied
        // input in exactly the sense the ELF loader's input is. An id past the
        // end of the descriptor table would index out of bounds; an id naming a
        // descriptor that was never submitted means the device completed
        // something the driver did not ask for.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        q.write_used_for_test(0, QUEUE_SIZE, 512);
        assert_eq!(q.take_used(0), None, "a used id past the ring was accepted");
        q.write_used_for_test(0, 3, 512);
        assert_eq!(q.take_used(0), None, "a used id naming a free descriptor was accepted");
    }

    #[test]
    fn a_used_entry_is_taken_exactly_once() {
        // Taking one twice frees its chain twice, which puts one descriptor on
        // the free list twice -- and the next two allocations hand the same
        // descriptor to two different requests.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(2).unwrap();
        q.publish(head);
        q.write_used_for_test(0, head, 512);
        assert_eq!(q.take_used(0), Some((head, 512)));
        assert_eq!(q.take_used(0), None, "the same completion was taken twice");
    }

    #[test]
    fn a_used_ring_shorter_than_the_queue_is_refused() {
        // `ingest_used` copies from DMA memory whose length the caller states.
        // A short buffer would read past the end of it; refusing says the
        // caller's length is wrong, which is actionable, where a truncated copy
        // silently leaves stale completions in the mirror.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let short = alloc::vec![0u8; RING_HEADER + 8 * QUEUE_SIZE as usize - 1];
        assert_eq!(q.ingest_used(&short, 0), None, "a short used ring was ingested");
        let exact = alloc::vec![0u8; RING_HEADER + 8 * QUEUE_SIZE as usize];
        assert_eq!(q.ingest_used(&exact, 7), Some(7), "an exactly-sized used ring was refused");
    }

    #[test]
    fn a_single_descriptor_serialises_the_same_as_the_whole_table() {
        // The interrupt-free path writes descriptors one at a time now. If the
        // two disagreed, the device would read a table the driver believes it
        // wrote -- the wrong-answer failure this crate is arranged around.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(3).expect("a fresh queue has descriptors");
        q.describe(head, 0x1234_5000, 16, false);
        let table = q.desc_bytes();
        let one = q.descriptor_bytes(head).expect("an in-range descriptor was refused");
        let base = head as usize * Descriptor::BYTES;
        assert_eq!(&table[base..base + Descriptor::BYTES], &one[..], "the two forms disagree");
        assert_eq!(q.descriptor_bytes(QUEUE_SIZE), None, "a descriptor past the table was served");
    }

    #[test]
    fn a_single_avail_slot_serialises_the_same_as_the_whole_ring() {
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(1).expect("a fresh queue has descriptors");
        q.publish(head);
        let ring = q.avail_slots_bytes();
        let slot = q.avail_index().wrapping_sub(1) % QUEUE_SIZE;
        let one = q.avail_slot_bytes(slot).expect("an in-range slot was refused");
        let base = slot as usize * 2;
        assert_eq!(&ring[base..base + 2], &one[..], "the two forms disagree");
        assert_eq!(u16::from_le_bytes(one), head, "the published slot does not name the chain");
        assert_eq!(q.avail_slot_bytes(QUEUE_SIZE), None, "a slot past the ring was served");
    }

    #[test]
    fn one_used_entry_can_be_ingested_without_reading_the_rest() {
        // What the interrupt path uses. The entry must land in the slot named
        // and carry the device's bytes verbatim, little-endian.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(1).expect("a fresh queue has descriptors");
        // Published, because a completion only names a chain the device was
        // actually handed -- see
        // `a_completion_for_a_chain_that_was_never_published_is_refused`.
        q.publish(head);
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&(head as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&512u32.to_le_bytes());
        assert!(q.ingest_used_slot(3, bytes), "an in-range slot was refused");
        assert_eq!(q.take_used(3), Some((head, 512)), "the entry did not land in slot 3");
    }

    #[test]
    fn a_used_slot_past_the_ring_is_refused_rather_than_wrapped() {
        // The slot comes from a device-supplied index. Wrapping it would write
        // over a live entry and mark some other request done, reported as a
        // successful completion -- so the negative direction is the one that
        // matters here.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        assert!(!q.ingest_used_slot(QUEUE_SIZE, [0u8; 8]), "a slot past the ring was accepted");
        assert!(!q.ingest_used_slot(u16::MAX, [0u8; 8]), "a wildly out-of-range slot was accepted");
    }
}
