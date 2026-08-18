//! Split virtqueue benchmarks.
//!
//! Every block request allocates a three-descriptor chain, fills it, publishes
//! it, and frees it on completion — under the driver's lock, with interrupts
//! masked. So these are not background costs: they are added to the latency of
//! every I/O and they are paid with the machine's interrupt response blocked.
//!
//! The serialisation group is the one that changed a decision. `desc_bytes`
//! writes the whole descriptor table; `descriptor_bytes` writes one. A request
//! touches three descriptors, so the per-descriptor form does three small
//! writes where the whole-table form does one large one — the ratio between
//! these two numbers is what says whether that trade is worth making at a given
//! queue size, and the answer changes with `SIZE`.

use criterion::{Criterion, criterion_group, criterion_main};
use qunix_virtio::queue::SplitQueue;
use std::hint::black_box;

/// The size the driver uses.
const SIZE: u16 = 64;

/// The three descriptors a block request needs: header, data, status.
const CHAIN: u16 = 3;

fn bench_request(c: &mut Criterion) {
    let mut group = c.benchmark_group("virtio/request");

    // The whole submission path's bookkeeping, minus the MMIO. Allocation walks
    // the free list `CHAIN` times and the free walks the chain back.
    let mut queue = SplitQueue::new(SIZE);
    group.bench_function("alloc+describe+publish+free", |b| {
        b.iter(|| {
            let head = queue.alloc_chain(CHAIN).expect("the queue refused a chain");
            let mut idx = head;
            loop {
                queue.describe(idx, black_box(0x1000), black_box(512), false);
                match queue.next_in_chain(idx) {
                    Some(next) => idx = next,
                    None => break,
                }
            }
            black_box(queue.publish(head));
            queue.free_chain(head);
        })
    });

    let mut alloc_only = SplitQueue::new(SIZE);
    group.bench_function("alloc+free", |b| {
        b.iter(|| {
            let head = alloc_only.alloc_chain(CHAIN).expect("the queue refused a chain");
            black_box(head);
            alloc_only.free_chain(head);
        })
    });

    group.finish();
}

fn bench_serialisation(c: &mut Criterion) {
    let mut group = c.benchmark_group("virtio/serialisation");

    let mut queue = SplitQueue::new(SIZE);
    let head = queue.alloc_chain(CHAIN).expect("the queue refused a chain");

    // What the driver actually writes per request: the chain's descriptors, one
    // at a time.
    group.bench_function("descriptor_bytes x chain", |b| {
        b.iter(|| {
            let mut idx = head;
            loop {
                black_box(queue.descriptor_bytes(idx));
                match queue.next_in_chain(idx) {
                    Some(next) => idx = next,
                    None => break,
                }
            }
        })
    });

    // What it wrote before: all `SIZE` descriptors, to publish three.
    group.bench_function("desc_bytes, whole table", |b| b.iter(|| black_box(queue.desc_bytes())));

    // The avail ring, same shape: one slot per publish against the whole ring.
    group.bench_function("avail_slot_bytes", |b| {
        b.iter(|| black_box(queue.avail_slot_bytes(black_box(0))))
    });
    group.bench_function("avail_slots_bytes, whole ring", |b| {
        b.iter(|| black_box(queue.avail_slots_bytes()))
    });

    group.finish();
}

fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("virtio/completion");

    // The interrupt handler's path. `ingest_used_slot` takes the eight bytes
    // the device wrote; `take_used` validates the id in them before the driver
    // acts on it.
    let mut queue = SplitQueue::new(SIZE);
    let head = queue.alloc_chain(CHAIN).expect("the queue refused a chain");
    let mut used = [0u8; 8];
    used[..4].copy_from_slice(&(head as u32).to_le_bytes());
    used[4..].copy_from_slice(&512u32.to_le_bytes());

    group.bench_function("ingest_used_slot", |b| {
        b.iter(|| black_box(queue.ingest_used_slot(black_box(0), black_box(used))))
    });

    // A completion the driver acts on, end to end: the device's bytes go in,
    // the id in them is validated, and the chain comes back. Measured as one
    // cycle because `take_used` consumes the completion -- a loop that only
    // called it would be measuring the refusal path from the second iteration
    // on, and reporting it as the accepted one.
    group.bench_function("ingest+take_used+free, one completion", |b| {
        b.iter(|| {
            let head = queue.alloc_chain(CHAIN).expect("the queue refused a chain");
            let mut used = [0u8; 8];
            used[..4].copy_from_slice(&(head as u32).to_le_bytes());
            used[4..].copy_from_slice(&512u32.to_le_bytes());
            queue.ingest_used_slot(0, used);
            let (completed, _) = black_box(queue.take_used(0)).expect("a live chain was refused");
            queue.free_chain(completed);
        })
    });

    // The refusal. An id naming a chain that was never submitted is what a
    // buggy or hostile device sends, and the driver pays this check on every
    // completion it does accept as well.
    let mut forged = [0u8; 8];
    forged[..4].copy_from_slice(&(SIZE as u32 - 1).to_le_bytes());
    queue.ingest_used_slot(1, forged);
    group.bench_function("take_used, id never submitted", |b| {
        b.iter(|| black_box(queue.take_used(black_box(1))))
    });

    group.finish();
}

criterion_group!(benches, bench_request, bench_serialisation, bench_completion);
criterion_main!(benches);
