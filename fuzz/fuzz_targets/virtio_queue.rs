#![no_main]
//! Structural fuzzing of the split virtqueue's descriptor bookkeeping.
//!
//! The failure this looks for is the one the allocators have: the same
//! descriptor handed to two live requests, with every call returning success.
//! When that happens the device is told one request's buffer address for
//! another request's data, so a read lands in the wrong page and the block
//! layer above reports a completed transfer. Nothing faults.
//!
//! So the target keeps an independent model of which descriptors belong to
//! which live chain and asserts, after every operation, that no descriptor is
//! in two of them, that the free count and the live descriptors account for
//! every descriptor in the table, and that the chain the queue walks is the
//! chain it handed out.
//!
//! # The used ring is device input, and is fuzzed as such
//!
//! `ingest_used_slot` takes eight raw bytes written by the device. That is the
//! one place in this crate where an attacker-or-buggy-device controls a value
//! the driver then indexes with, so the target feeds it arbitrary bytes and
//! asserts the only thing that must hold: a completion is consumable exactly
//! once, and never names a chain that was not submitted. Taking one twice frees
//! its descriptors twice, which is precisely how one descriptor ends up on the
//! free list twice.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use qunix_virtio::queue::SplitQueue;
use std::collections::{HashMap, HashSet};

const SIZE: u16 = 32;

#[derive(Arbitrary, Debug)]
enum Op {
    Alloc { n: u8 },
    Free { which: u8 },
    Describe { which: u8, position: u8, addr: u64, len: u32, writable: bool },
    Publish { which: u8 },
    /// Eight bytes as the device would write them into a used-ring slot.
    Ingest { slot: u16, bytes: [u8; 8] },
    TakeUsed { slot: u16 },
}

fuzz_target!(|ops: Vec<Op>| {
    let mut queue = SplitQueue::new(SIZE);
    // head -> every descriptor in that chain, in order.
    let mut live: HashMap<u16, Vec<u16>> = HashMap::new();
    // Heads the queue considers submitted but not yet completed. Distinct from
    // `live`: `take_used` clears the flag without returning the descriptors.
    let mut submitted: HashSet<u16> = HashSet::new();

    // Anchors the run: a queue that refused everything would make every op a
    // no-op and the run would still pass.
    let probe = queue.alloc_chain(1).expect("a fresh queue refused a single descriptor");
    queue.free_chain(probe);
    assert_eq!(queue.free_count(), SIZE, "a freed single-descriptor chain did not come back");

    for op in ops {
        let heads: Vec<u16> = live.keys().copied().collect();
        match op {
            Op::Alloc { n } => {
                // Past the queue size, so exhaustion and the zero-length
                // refusal are both reachable.
                let n = n as u16 % (SIZE + 2);
                let free_before = queue.free_count();
                match queue.alloc_chain(n) {
                    Some(head) => {
                        assert!(n > 0 && n <= free_before, "alloc_chain({n}) succeeded with {free_before} free");
                        // Walk what the queue says the chain is, rather than
                        // trusting the count: a chain that walks short is a
                        // request whose data descriptor is missing, and the
                        // device completes a transfer of nothing, successfully.
                        let mut chain = vec![head];
                        let mut idx = head;
                        while let Some(next) = queue.next_in_chain(idx) {
                            assert!(chain.len() < SIZE as usize, "chain from {head} does not terminate");
                            chain.push(next);
                            idx = next;
                        }
                        assert_eq!(chain.len(), n as usize, "alloc_chain({n}) walked {} descriptors", chain.len());
                        assert_eq!(
                            queue.free_count(),
                            free_before - n,
                            "alloc_chain({n}) took the wrong number of descriptors"
                        );
                        let owned: HashSet<u16> = live.values().flatten().copied().collect();
                        for d in &chain {
                            assert!(!owned.contains(d), "descriptor {d} is in two live chains");
                        }
                        assert!(live.insert(head, chain).is_none(), "head {head} was handed out twice");
                        submitted.insert(head);
                    }
                    None => assert!(
                        n == 0 || n > free_before,
                        "alloc_chain({n}) refused a queue with {free_before} free"
                    ),
                }
            }

            Op::Free { which } => {
                if heads.is_empty() {
                    continue;
                }
                let head = heads[which as usize % heads.len()];
                let chain = live.remove(&head).unwrap();
                let free_before = queue.free_count();
                queue.free_chain(head);
                // Every descriptor, not just the head. Releasing only the head
                // leaks the rest and the queue runs dry long after the code
                // that leaked them ran.
                assert_eq!(
                    queue.free_count(),
                    free_before + chain.len() as u16,
                    "free_chain({head}) returned the wrong number of descriptors"
                );
                submitted.remove(&head);
            }

            Op::Describe { which, position, addr, len, writable } => {
                if heads.is_empty() {
                    continue;
                }
                let head = heads[which as usize % heads.len()];
                let chain = &live[&head];
                let idx = chain[position as usize % chain.len()];
                queue.describe(idx, addr, len, writable);
            }

            Op::Publish { which } => {
                if heads.is_empty() {
                    continue;
                }
                let head = heads[which as usize % heads.len()];
                let before = queue.avail_index();
                let after = queue.publish(head);
                // Wrapping, and one per publish. A saturating index stops
                // advancing after 65535 publishes and the device never sees
                // another request.
                assert_eq!(after, before.wrapping_add(1), "publish did not advance the avail index");
                assert_eq!(after, queue.avail_index());
                // The slot is the index reduced by the queue size; conflating
                // the two writes outside the ring from the SIZE-th publish on.
                assert_eq!(queue.last_slot(), before % SIZE, "publish wrote the wrong ring slot");
                assert_eq!(
                    queue.avail_slot_bytes(queue.last_slot()),
                    Some(head.to_le_bytes()),
                    "the published slot does not name the chain"
                );
            }

            Op::Ingest { slot, bytes } => {
                let accepted = queue.ingest_used_slot(slot, bytes);
                assert_eq!(accepted, slot < SIZE, "ingest_used_slot({slot}) took an out-of-range slot");
            }

            Op::TakeUsed { slot } => {
                match queue.take_used(slot) {
                    Some((head, _)) => {
                        // The id came from the device. It must name a chain the
                        // driver actually submitted, or freeing it walks `next`
                        // links belonging to a live request.
                        assert!(
                            submitted.remove(&head),
                            "take_used returned chain {head}, which was never submitted"
                        );
                        assert!(live.contains_key(&head), "take_used returned a chain that is not live");
                    }
                    None => {}
                }
            }
        }

        // Every descriptor is either free or in exactly one live chain.
        let owned: Vec<u16> = live.values().flatten().copied().collect();
        let unique: HashSet<u16> = owned.iter().copied().collect();
        assert_eq!(owned.len(), unique.len(), "a descriptor is in two live chains");
        assert_eq!(
            queue.free_count() as usize + owned.len(),
            SIZE as usize,
            "descriptors are missing: {} free, {} live, {SIZE} total",
            queue.free_count(),
            owned.len()
        );
    }

    // Drain, so a chain that could not be returned fails here rather than
    // ending the run with the queue quietly short of capacity.
    for head in live.keys().copied().collect::<Vec<_>>() {
        queue.free_chain(head);
    }
    assert_eq!(queue.free_count(), SIZE, "the queue did not come back to full after draining");
    // And a drained queue is a usable one, not merely one whose counter says so.
    assert!(queue.alloc_chain(SIZE).is_some(), "the drained queue refused a full-width chain");
});
