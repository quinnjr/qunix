#![no_main]
//! Structural fuzzing of the buffer cache's slot table.
//!
//! Same failure shape as the allocators, one layer up. The cache will not crash
//! when it is wrong: every operation returns successfully and a caller reads
//! another block's bytes, or a write it was told succeeded is never queued. So
//! the target does not look for panics. It maintains an independent model of
//! which key lives in which slot and what may be done with it, and asserts
//! after every operation that:
//!
//! * no two slots hold the same key -- two slots for one block diverge the
//!   moment either is written, and `lookup` answers from only one of them;
//! * `lookup` finds exactly the slot the model put the key in;
//! * `victim` and `evict` refuse every dirty, in-flight or pinned slot;
//! * `dirty_slots` reports exactly the set the model dirtied.
//!
//! # The harness must respect the contract it is testing
//!
//! The cache's mutators assert: `mark_clean` refuses a slot the device owns,
//! `unpin` refuses an unbalanced release, `begin_io` refuses a slot that
//! already has io outstanding. Those are the invariants under test, so a
//! harness that called them anyway would report the crate's own refusal as a
//! crash -- three of the first four "bugs" in the buddy target were exactly
//! that mistake. Every operation below is therefore selected from the slots the
//! *model* says it is legal for, and the assertions are about the answer rather
//! than the absence of a fault.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use qunix_bcache::{BlockKey, Cache, InsertError, SlotState};
use std::collections::HashMap;

const SLOTS: usize = 16;

#[derive(Arbitrary, Debug)]
enum Op {
    Insert { dev: u8, block: u8 },
    Lookup { dev: u8, block: u8 },
    /// `ok` is whether the io succeeded; a failed write must leave the slot
    /// dirty rather than clean.
    EndIo { which: u8, ok: bool },
    MarkDirty { which: u8 },
    MarkClean { which: u8 },
    BeginIo { which: u8 },
    Pin { which: u8 },
    Unpin { which: u8 },
    Evict { which: u8 },
}

/// What the model believes about one occupied slot.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Occupant {
    key: BlockKey,
    state: SlotState,
    pins: u32,
    /// Mirrors the cache's own "dirtied while io was outstanding" bit, which
    /// has no accessor -- it is only observable through what `end_io` does.
    dirtied_during_io: bool,
}

/// Picks one of `slots` by an arbitrary byte, or `None` if there are none.
fn pick(slots: &[usize], which: u8) -> Option<usize> {
    (!slots.is_empty()).then(|| slots[which as usize % slots.len()])
}

fuzz_target!(|ops: Vec<Op>| {
    let mut cache = Cache::<SLOTS>::new();
    let mut model: HashMap<usize, Occupant> = HashMap::new();

    // Anchors the run. Without it a `Cache` that refused everything would make
    // every op below a no-op and the run would still pass.
    let probe = cache.insert(BlockKey { dev: 0, block: 0 }).expect("a fresh table refused a block");
    // Stated as a refusal, not as a disjunction: `a || b` short-circuits, so a
    // version where `evict` wrongly accepted an unfilled slot would satisfy the
    // assert and then fail somewhere else, reporting the wrong thing.
    assert!(!cache.evict(probe), "a slot still being filled was evicted");
    cache.end_io(probe, SlotState::Clean);
    assert!(cache.evict(probe), "a clean unpinned slot refused eviction");

    for op in ops {
        // Slots grouped by what the model says may legally be done to them.
        let occupied: Vec<usize> = model.keys().copied().collect();
        let resident: Vec<usize> =
            model.iter().filter(|(_, o)| o.state != SlotState::InFlight).map(|(i, _)| *i).collect();
        let in_flight: Vec<usize> =
            model.iter().filter(|(_, o)| o.state == SlotState::InFlight).map(|(i, _)| *i).collect();
        let pinned: Vec<usize> =
            model.iter().filter(|(_, o)| o.pins > 0).map(|(i, _)| *i).collect();

        match op {
            Op::Insert { dev, block } => {
                let key = BlockKey { dev: dev as u32, block: block as u64 };
                let resident_already = model.values().any(|o| o.key == key);
                let full = model.len() == SLOTS;
                match cache.insert(key) {
                    Ok(slot) => {
                        assert!(!resident_already, "{key:?} was given a second slot");
                        assert!(!full, "a full table handed out a slot");
                        assert!(model.insert(slot, Occupant {
                            key,
                            // A claimed slot is not a filled one: nothing has
                            // been read into it yet.
                            state: SlotState::InFlight,
                            pins: 0,
                            dirtied_during_io: false,
                        }).is_none(), "slot {slot} was handed out while occupied");
                    }
                    // The refusal names itself now, so the model asserts *which*
                    // one rather than reconstructing it from its own state --
                    // a caller that could not tell the two apart is the bug
                    // this signature exists to prevent.
                    Err(InsertError::AlreadyResident(slot)) => {
                        assert!(!full || resident_already, "a full table reported residency");
                        assert_eq!(
                            model.get(&slot).map(|o| o.key),
                            Some(key),
                            "insert named a slot that does not hold {key:?}"
                        );
                    }
                    Err(InsertError::Full) => assert!(
                        full && !resident_already,
                        "{key:?} was refused as full by a table that was not full"
                    ),
                }
            }

            Op::Lookup { dev, block } => {
                let key = BlockKey { dev: dev as u32, block: block as u64 };
                let expected = model.iter().find(|(_, o)| o.key == key).map(|(i, _)| *i);
                assert_eq!(cache.lookup(key), expected, "lookup disagreed with the model");
            }

            Op::BeginIo { which } => {
                let Some(slot) = pick(&resident, which) else { continue };
                let occupant = model.get_mut(&slot).unwrap();
                assert_eq!(cache.begin_io(slot), occupant.state, "begin_io lost the prior state");
                occupant.state = SlotState::InFlight;
            }

            Op::EndIo { which, ok } => {
                let Some(slot) = pick(&in_flight, which) else { continue };
                let occupant = model.get_mut(&slot).unwrap();
                let asked = if ok { SlotState::Clean } else { SlotState::Dirty };
                cache.end_io(slot, asked);
                // A write that landed during the io survives it, whatever the
                // io itself concluded -- otherwise the write is lost and no
                // read through the cache can reveal it.
                occupant.state = if occupant.dirtied_during_io { SlotState::Dirty } else { asked };
                occupant.dirtied_during_io = false;
            }

            Op::MarkDirty { which } => {
                let Some(slot) = pick(&occupied, which) else { continue };
                cache.mark_dirty(slot);
                let occupant = model.get_mut(&slot).unwrap();
                if occupant.state == SlotState::InFlight {
                    occupant.dirtied_during_io = true;
                } else {
                    occupant.state = SlotState::Dirty;
                }
            }

            Op::MarkClean { which } => {
                let Some(slot) = pick(&resident, which) else { continue };
                cache.mark_clean(slot);
                model.get_mut(&slot).unwrap().state = SlotState::Clean;
            }

            Op::Pin { which } => {
                let Some(slot) = pick(&occupied, which) else { continue };
                cache.pin(slot);
                model.get_mut(&slot).unwrap().pins += 1;
            }

            Op::Unpin { which } => {
                let Some(slot) = pick(&pinned, which) else { continue };
                cache.unpin(slot);
                model.get_mut(&slot).unwrap().pins -= 1;
            }

            Op::Evict { which } => {
                let Some(slot) = pick(&occupied, which) else { continue };
                let occupant = *model.get(&slot).unwrap();
                let reusable = occupant.state == SlotState::Clean && occupant.pins == 0;
                assert_eq!(
                    cache.evict(slot),
                    reusable,
                    "evict disagreed with the model about slot {slot}: {occupant:?}"
                );
                if reusable {
                    model.remove(&slot);
                }
            }
        }

        check(&cache, &model);
    }

    // Drain, so a table that leaked a slot into an unreusable state fails here
    // rather than silently ending the run with capacity missing.
    for (slot, occupant) in model.clone() {
        for _ in 0..occupant.pins {
            cache.unpin(slot);
        }
        if cache.state_of(slot) == Some(SlotState::InFlight) {
            cache.end_io(slot, SlotState::Clean);
        }
        cache.mark_clean(slot);
        assert!(cache.evict(slot), "slot {slot} could not be released after being quiesced");
        model.remove(&slot);
    }
    for slot in 0..SLOTS {
        assert_eq!(cache.state_of(slot), Some(SlotState::Free), "slot {slot} survived the drain");
    }
    // And an emptied table is a usable one, not merely an empty-looking one.
    for block in 0..SLOTS as u64 {
        assert!(cache.insert(BlockKey { dev: 0, block }).is_ok(), "the drained table refused");
    }
});

/// Every invariant that must hold after every operation.
fn check(cache: &Cache<SLOTS>, model: &HashMap<usize, Occupant>) {
    // Two slots holding one key is the cache's aliasing bug: `lookup` answers
    // from one, writeback flushes the other, and the read afterwards serves
    // bytes that were overwritten.
    let mut seen: HashMap<BlockKey, usize> = HashMap::new();
    for slot in 0..SLOTS {
        let Some(key) = cache.key_of(slot) else {
            assert!(!model.contains_key(&slot), "slot {slot} lost the block the model put there");
            assert_eq!(cache.pins_of(slot), Some(0), "a free slot reports pins");
            continue;
        };
        if let Some(other) = seen.insert(key, slot) {
            panic!("{key:?} is in slots {other} and {slot}");
        }
        let occupant = model.get(&slot).unwrap_or_else(|| panic!("slot {slot} holds {key:?}, which the model never put there"));
        assert_eq!(occupant.key, key, "slot {slot} holds the wrong block");
        assert_eq!(cache.state_of(slot), Some(occupant.state), "slot {slot} is in the wrong state");
        // The count `kernel::bcache::store` reads to decide whether a live
        // `&[u8]` exists over this slot's frame. A wrong one there is a write
        // through a raw pointer aliasing a live shared reference, so it is
        // asserted directly rather than inferred from an `evict` outcome.
        assert_eq!(
            cache.pins_of(slot),
            Some(occupant.pins),
            "slot {slot} reports the wrong pin count"
        );
    }

    // The three refusals, checked against the model rather than against the
    // cache's own opinion of them.
    if let Some(victim) = cache.victim() {
        match model.get(&victim) {
            None => {} // a free slot: always fair game, and preferred
            Some(occupant) => {
                assert_eq!(
                    occupant.state,
                    SlotState::Clean,
                    "victim offered slot {victim} in state {:?}",
                    occupant.state
                );
                assert_eq!(occupant.pins, 0, "victim offered pinned slot {victim}");
            }
        }
        // Free before evicting: a clean slot offered while one is free costs
        // hit rate for nothing, and nothing but this would notice.
        if model.len() < SLOTS {
            assert!(
                !model.contains_key(&victim),
                "victim evicted slot {victim} while {} slots were free",
                SLOTS - model.len()
            );
        }
    } else {
        assert!(
            model.len() == SLOTS
                && model.values().all(|o| o.state != SlotState::Clean || o.pins > 0),
            "victim refused a table holding a reusable slot"
        );
    }

    // A dirty slot missing from this is a write that was acknowledged and never
    // reached the disk, which no later read through the cache can reveal.
    let reported: Vec<usize> = cache.dirty_slots().collect();
    let expected: Vec<usize> = {
        let mut v: Vec<usize> =
            model.iter().filter(|(_, o)| o.state == SlotState::Dirty).map(|(i, _)| *i).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(reported, expected, "the writeback set does not match what was dirtied");
}
