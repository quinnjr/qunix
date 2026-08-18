//! Buffer cache bookkeeping benchmarks.
//!
//! Every read the filesystem issues starts with a `lookup`, so its cost is paid
//! on the hit path as well as the miss path — it is the one operation here that
//! is never amortised against I/O. `victim` and `dirty_slots` are paid only
//! under pressure and at `sync` respectively.
//!
//! The table is a linear scan, so every figure below is per-slot: at `SLOTS`
//! entries a miss touches all of them. That is the number to watch when the
//! cache is sized, and it is why the *miss* is benchmarked as well as the hit —
//! a miss is the worst case and it is also the case a cold filesystem is in.
//!
//! Nothing here allocates, and that is the point of the crate rather than an
//! artefact of the benchmark: the flush path runs when memory is already gone.

use criterion::{Criterion, criterion_group, criterion_main};
use qunix_bcache::{BlockKey, Cache, SlotState};
use std::hint::black_box;

/// Large enough that the linear scan dominates the call overhead, and in the
/// range a real cache would be sized at.
const SLOTS: usize = 64;

/// A full table of clean, unpinned blocks.
fn full() -> Cache<SLOTS> {
    let mut cache = Cache::<SLOTS>::new();
    for block in 0..SLOTS as u64 {
        let slot = cache.insert(BlockKey { dev: 0, block }).expect("the table refused a block");
        cache.end_io(slot, SlotState::Clean);
    }
    cache
}

fn bench_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("bcache/lookup");
    let cache = full();

    // The last slot: a hit still scans the whole table, so this is the hit's
    // real cost rather than the cost of finding slot 0.
    let last = BlockKey { dev: 0, block: SLOTS as u64 - 1 };
    group.bench_function("hit, last slot", |b| b.iter(|| black_box(cache.lookup(black_box(last)))));

    let first = BlockKey { dev: 0, block: 0 };
    group.bench_function("hit, first slot", |b| {
        b.iter(|| black_box(cache.lookup(black_box(first))))
    });

    // A miss scans every slot and finds nothing, which is what a cold cache
    // pays on every access.
    let absent = BlockKey { dev: 9, block: 9 };
    group.bench_function("miss", |b| b.iter(|| black_box(cache.lookup(black_box(absent)))));

    group.finish();
}

fn bench_admission(c: &mut Criterion) {
    let mut group = c.benchmark_group("bcache/admission");

    // Claiming a slot in a table with one free. Two full passes: `insert`
    // scans once for the duplicate check and once for a free slot.
    //
    // The freed slot is the *last* one, deliberately. Freeing slot 0 -- which
    // is what evicting block 0 does -- makes the free-slot scan hit on its
    // first element, so the measurement is one pass and a bit, and a regression
    // in that scan would not show up here at all.
    group.bench_function("insert into a nearly full table", |b| {
        let mut cache = full();
        let slot = cache.lookup(BlockKey { dev: 0, block: SLOTS as u64 - 1 }).unwrap();
        cache.evict(slot);
        let mut block = SLOTS as u64;
        b.iter(|| {
            block += 1;
            let key = BlockKey { dev: 0, block };
            let slot = cache.insert(key).expect("the table refused a block");
            black_box(slot);
            cache.end_io(slot, SlotState::Clean);
            cache.evict(slot);
        })
    });

    // Under pressure the table is full and the scan runs to the first reusable
    // slot; with every slot clean that is slot 0, which is the cheap case.
    let cache = full();
    group.bench_function("victim, full table", |b| b.iter(|| black_box(cache.victim())));

    // The expensive case: only the last slot is reusable, so the scan runs the
    // whole table twice -- once looking for a free slot, once for a clean one.
    let mut pressured = full();
    for block in 0..SLOTS as u64 - 1 {
        let slot = pressured.lookup(BlockKey { dev: 0, block }).unwrap();
        pressured.mark_dirty(slot);
    }
    group.bench_function("victim, one reusable slot", |b| {
        b.iter(|| black_box(pressured.victim()))
    });

    group.finish();
}

fn bench_writeback(c: &mut Criterion) {
    let mut group = c.benchmark_group("bcache/writeback");

    // What `sync` walks. Every slot dirty is the worst case and also the one
    // that matters, because that is the state a write-heavy workload leaves.
    let mut dirty = full();
    for block in 0..SLOTS as u64 {
        let slot = dirty.lookup(BlockKey { dev: 0, block }).unwrap();
        dirty.mark_dirty(slot);
    }
    group.bench_function("dirty_slots, all dirty", |b| {
        b.iter(|| black_box(dirty.dirty_slots().count()))
    });

    let clean = full();
    group.bench_function("dirty_slots, none dirty", |b| {
        b.iter(|| black_box(clean.dirty_slots().count()))
    });

    group.finish();
}

fn bench_pinning(c: &mut Criterion) {
    let mut group = c.benchmark_group("bcache/pinning");

    // Paid twice per buffer access, so it is on the hot path even though it is
    // only a counter.
    let mut cache = full();
    group.bench_function("pin+unpin", |b| {
        b.iter(|| {
            cache.pin(black_box(0));
            cache.unpin(black_box(0));
        })
    });

    group.finish();
}

criterion_group!(benches, bench_lookup, bench_admission, bench_writeback, bench_pinning);
criterion_main!(benches);
