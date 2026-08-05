//! Pure-logic HAL benchmarks.
//!
//! Almost nothing in this crate can be benchmarked on the host: GDT, IDT, APIC
//! and serial all touch hardware, and paging needs real page tables. What
//! remains is the flag composition on the mapping path, included because it
//! runs once per `map` call.
//!
//! Expect unremarkable numbers. This exists so a regression would be visible,
//! not because the function is suspected of being slow.

use criterion::{Criterion, criterion_group, criterion_main};
use qunix_hal_x86_64::paging::PageFlags;
use std::hint::black_box;

fn bench_flags(c: &mut Criterion) {
    let mut group = c.benchmark_group("hal/page_flags");
    group.bench_function("compose", |b| {
        b.iter(|| {
            black_box(
                black_box(PageFlags::PRESENT)
                    | PageFlags::WRITABLE
                    | PageFlags::NO_CACHE
                    | PageFlags::NO_EXECUTE,
            )
        })
    });
    group.finish();
}

criterion_group!(benches, bench_flags);
criterion_main!(benches);
