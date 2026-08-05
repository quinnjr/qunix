//! Buddy allocator benchmarks.
//!
//! These run on the host against a `Vec`-backed [`FrameBacking`], not the
//! kernel's HHDM. That difference matters when reading the numbers: in the
//! kernel every `read_link`/`write_link` is a volatile access to a *different
//! physical frame*, so the free-list walks below are cache- and TLB-hostile in
//! a way a contiguous `Vec` is not. Treat these as measuring algorithmic shape
//! and instruction count, not absolute kernel cost.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use qunix_mm::{FrameBacking, PAGE_SIZE, buddy::BuddyAllocator};
use std::cell::UnsafeCell;
use std::hint::black_box;

/// Flat buffer standing in for physical memory, mirroring the test double.
struct VecBacking {
    base: u64,
    mem: UnsafeCell<Vec<u8>>,
}

impl VecBacking {
    fn new(base: u64, len: usize) -> Self {
        Self { base, mem: UnsafeCell::new(vec![0u8; len]) }
    }
}

// Raw volatile access, deliberately mirroring the kernel's `HhdmBacking`.
//
// The unit tests use a bounds-checked slicing version, which is right for a
// test and wrong for a benchmark: slicing plus `try_into` plus `from_le_bytes`
// costs more than the allocator operation being measured, so the numbers end
// up reporting the harness. This version is the same two instructions the
// kernel issues.
impl FrameBacking for VecBacking {
    unsafe fn read_link(&self, pa: u64) -> u64 {
        let mem = unsafe { &*self.mem.get() };
        unsafe { mem.as_ptr().add((pa - self.base) as usize).cast::<u64>().read_volatile() }
    }
    unsafe fn write_link(&self, pa: u64, value: u64) {
        let mem = unsafe { &mut *self.mem.get() };
        unsafe { mem.as_mut_ptr().add((pa - self.base) as usize).cast::<u64>().write_volatile(value) };
    }
}

const BASE: u64 = 0x10_0000;

fn allocator(bytes: usize) -> BuddyAllocator<VecBacking> {
    let mut a = BuddyAllocator::new(VecBacking::new(BASE, bytes));
    unsafe { a.add_region(BASE, bytes as u64) };
    a
}

/// `add_region` is the boot path: it runs once per usable memory region and
/// decomposes each into naturally aligned blocks. On a large machine this is
/// the single largest piece of allocator work the kernel ever does.
fn bench_add_region(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy/add_region");
    for mib in [4usize, 64, 256] {
        let bytes = mib << 20;
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_with_input(BenchmarkId::from_parameter(format!("{mib}MiB")), &bytes, |b, &bytes| {
            b.iter_batched(
                || VecBacking::new(BASE, bytes),
                |backing| {
                    let mut a = BuddyAllocator::new(backing);
                    unsafe { a.add_region(BASE, bytes as u64) };
                    black_box(a.free_bytes())
                },
                criterion::BatchSize::LargeInput,
            )
        });
    }
    group.finish();
}

/// The hot path: a single-page allocation and its matching free. `alloc` may
/// split down from a larger order and `free` may coalesce back up, so this
/// measures the full round trip rather than either half in isolation.
fn bench_alloc_free_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy/alloc_free");
    for order in [0u8, 4, 9] {
        group.bench_with_input(BenchmarkId::from_parameter(format!("order{order}")), &order, |b, &order| {
            let mut a = allocator(64 << 20);
            b.iter(|| {
                let pa = a.alloc(black_box(order)).expect("out of frames");
                unsafe { a.free(black_box(pa), order) };
            })
        });
    }
    group.finish();
}

/// Allocation with no free/coalesce in between, so every call splits a block
/// further. This isolates `alloc`'s split loop and the `nonempty` bitmask
/// search from `free`'s merge cost.
fn bench_alloc_drain(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy/alloc_drain");
    group.throughput(Throughput::Elements(1024));
    group.bench_function("1024x order0", |b| {
        b.iter_batched_ref(
            || allocator(16 << 20),
            |a| {
                for _ in 0..1024 {
                    black_box(a.alloc(0));
                }
            },
            criterion::BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// `free`'s two shapes. Coalescing walks up to `MAX_ORDER` levels, reading a
/// tag out of a different frame at each; refusing stops at the first level.
/// The refusing case is the common one in a running kernel, so its cost is
/// what actually matters.
fn bench_free_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy/free");

    // Buddy is allocated, so the merge is refused immediately.
    group.bench_function("no_coalesce", |b| {
        b.iter_batched_ref(
            || {
                let mut a = allocator(16 << 20);
                let keep = a.alloc(0).unwrap();
                let free_me = a.alloc(0).unwrap();
                (a, keep, free_me)
            },
            |(a, _keep, free_me)| unsafe { a.free(black_box(*free_me), 0) },
            criterion::BatchSize::LargeInput,
        )
    });

    // Every buddy free, so the merge climbs as far as the region allows.
    group.bench_function("full_coalesce", |b| {
        b.iter_batched_ref(
            || {
                let mut a = allocator(1 << 20);
                let mut pages = Vec::new();
                while let Some(pa) = a.alloc(0) {
                    pages.push(pa);
                }
                let last = pages.pop().unwrap();
                for pa in pages {
                    unsafe { a.free(pa, 0) };
                }
                (a, last)
            },
            |(a, last)| unsafe { a.free(black_box(*last), 0) },
            criterion::BatchSize::LargeInput,
        )
    });

    group.finish();
}

/// Isolates split/merge churn from the rest of `alloc`/`free`.
///
/// Hypothesis for order-0 costing 5.6x order-9: a lone `alloc(0)` must split a
/// large block all the way down, and the matching `free` coalesces it all the
/// way back, so the pair pays ~2*log2(region) list operations. If that is the
/// cause, keeping several order-0 blocks parked on the free list -- so neither
/// splitting nor merging is needed -- should collapse the cost.
fn bench_order0_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("buddy/order0_churn");

    // Nothing parked: every alloc splits down, every free merges up.
    group.bench_function("cold (splits and merges)", |b| {
        let mut a = allocator(64 << 20);
        b.iter(|| {
            let pa = a.alloc(black_box(0)).expect("out of frames");
            unsafe { a.free(black_box(pa), 0) };
        })
    });

    // Several order-0 blocks already free and non-buddy, so `alloc` pops one
    // directly and `free` finds its buddy busy and stops immediately.
    group.bench_function("warm (parked order-0 blocks)", |b| {
        let mut a = allocator(64 << 20);
        // Take pairs and hold one of each, so the freed halves cannot coalesce.
        let mut held = Vec::new();
        for _ in 0..64 {
            let keep = a.alloc(0).unwrap();
            let park = a.alloc(0).unwrap();
            unsafe { a.free(park, 0) };
            held.push(keep);
        }
        b.iter(|| {
            let pa = a.alloc(black_box(0)).expect("out of frames");
            unsafe { a.free(black_box(pa), 0) };
        });
        black_box(&held);
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_order0_churn,
    bench_add_region,
    bench_alloc_free_roundtrip,
    bench_alloc_drain,
    bench_free_paths
);
criterion_main!(benches);
