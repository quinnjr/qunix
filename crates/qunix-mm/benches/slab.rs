//! Kernel heap benchmarks.
//!
//! `SlabHeap` sits behind `#[global_allocator]`, so `alloc`/`dealloc` here are
//! on the path of every `Box`, `Vec` and `String` the kernel creates. Host
//! numbers omit the `IrqSpinLock` the kernel wraps this in, which adds a
//! `pushfq`/`cli`/`popfq` around each call.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use qunix_mm::slab::SlabHeap;
use std::alloc::Layout;
use std::hint::black_box;

/// Gives the heap a real, page-aligned host allocation to manage.
fn heap_with(bytes: usize) -> (SlabHeap, Box<[u8]>) {
    let backing = vec![0u8; bytes + 4096].into_boxed_slice();
    let aligned = (backing.as_ptr() as usize + 4095) & !4095;
    let mut heap = SlabHeap::new();
    unsafe { heap.set_backing(aligned, bytes) };
    (heap, backing)
}

/// Carving a fresh block from the bump region versus popping one off a free
/// list. The bump path is what a growing kernel pays; the free-list path is
/// steady state.
fn bench_alloc_dealloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("slab/alloc_dealloc");
    // One per size-class family: smallest, a 3/2 step, a power of two, largest.
    for size in [8usize, 24, 512, 2048] {
        group.bench_with_input(BenchmarkId::from_parameter(format!("{size}B")), &size, |b, &size| {
            let (mut heap, _backing) = heap_with(8 << 20);
            let layout = Layout::from_size_align(size, 8).unwrap();
            b.iter(|| {
                let p = unsafe { heap.alloc(black_box(layout)) };
                unsafe { heap.dealloc(p, layout) };
            })
        });
    }
    group.finish();
}

/// First touch of a class, so every iteration carves fresh bump space rather
/// than reusing a freed block.
fn bench_bump_carve(c: &mut Criterion) {
    let mut group = c.benchmark_group("slab/bump_carve");
    group.throughput(Throughput::Elements(512));
    group.bench_function("512x 64B, no reuse", |b| {
        b.iter_batched_ref(
            || heap_with(8 << 20),
            |(heap, _backing)| {
                let layout = Layout::from_size_align(64, 8).unwrap();
                for _ in 0..512 {
                    black_box(unsafe { heap.alloc(layout) });
                }
            },
            criterion::BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// The large-block path: anything above the largest class or aligned stricter
/// than 16 rounds to a power-of-two extent and uses the parallel free lists.
fn bench_large_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("slab/large");

    group.bench_function("9000B oversized", |b| {
        let (mut heap, _backing) = heap_with(32 << 20);
        let layout = Layout::from_size_align(9000, 16).unwrap();
        b.iter(|| {
            let p = unsafe { heap.alloc(black_box(layout)) };
            unsafe { heap.dealloc(p, layout) };
        })
    });

    // A DMA-shaped request: small but strictly aligned, which also takes the
    // large path because align exceeds MAX_CLASS_ALIGN.
    group.bench_function("4KiB/4KiB-aligned", |b| {
        let (mut heap, _backing) = heap_with(32 << 20);
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        b.iter(|| {
            let p = unsafe { heap.alloc(black_box(layout)) };
            unsafe { heap.dealloc(p, layout) };
        })
    });

    group.finish();
}

/// Sweeps every size across the class ladder, so the 3/2 boundary comparison
/// in `class_for` is exercised on both sides rather than at one point.
fn bench_class_ladder(c: &mut Criterion) {
    let mut group = c.benchmark_group("slab/class_ladder");
    group.throughput(Throughput::Elements(2048));
    group.bench_function("alloc+dealloc across 1..=2048", |b| {
        let (mut heap, _backing) = heap_with(8 << 20);
        b.iter(|| {
            for size in 1..=2048usize {
                let layout = Layout::from_size_align(size, 8).unwrap();
                let p = unsafe { heap.alloc(black_box(layout)) };
                unsafe { heap.dealloc(p, layout) };
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_alloc_dealloc, bench_bump_carve, bench_large_path, bench_class_ladder);
criterion_main!(benches);
