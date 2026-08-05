//! Lock benchmarks.
//!
//! Every allocation, every frame operation and every console byte in the
//! kernel passes through one of these. The uncontended path is what matters
//! on a single CPU — contention only becomes interesting at M1's SMP.
//!
//! `NoopIrq` stands in for the real `IrqControl`, so the `IrqSpinLock` numbers
//! measure the wrapper's overhead (the guard, the `ManuallyDrop` drop order)
//! and *not* the `pushfq`/`cli`/`sti` the kernel actually pays. The delta
//! against `SpinLock` is therefore the floor, not the true cost.

use criterion::{Criterion, criterion_group, criterion_main};
use qunix_sync::{IrqControl, IrqSpinLock, SpinLock};
use std::hint::black_box;

/// Deliberately free, so the `IrqSpinLock` numbers isolate the *wrapper* --
/// the guard construction and the `ManuallyDrop` drop ordering -- from the
/// architecture hook.
///
/// An earlier version used atomic counters here and reported `IrqSpinLock` at
/// 2.3x `SpinLock`; almost all of that was two `lock xadd`s in the double
/// itself, not the code under test. The kernel's real hook is `pushfq`/`cli`
/// on the way in and a conditional `sti` on the way out, so add that to these
/// figures rather than reading them as the whole cost.
struct NoopIrq;

impl IrqControl for NoopIrq {
    fn disable_and_save() -> bool {
        black_box(true)
    }
    fn restore(was_enabled: bool) {
        black_box(was_enabled);
    }
}

fn bench_uncontended(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/uncontended");

    let spin = SpinLock::new(0u64);
    group.bench_function("SpinLock lock+drop", |b| {
        b.iter(|| {
            let mut guard = spin.lock();
            *guard = black_box(*guard + 1);
        })
    });

    group.bench_function("SpinLock try_lock hit", |b| {
        b.iter(|| black_box(spin.try_lock().is_some()))
    });

    let irq: IrqSpinLock<u64, NoopIrq> = IrqSpinLock::new(0);
    group.bench_function("IrqSpinLock lock+drop", |b| {
        b.iter(|| {
            let mut guard = irq.lock();
            *guard = black_box(*guard + 1);
        })
    });

    group.bench_function("IrqSpinLock try_lock hit", |b| {
        b.iter(|| black_box(irq.try_lock().is_some()))
    });

    group.finish();
}

/// The failure arm restores interrupt state before returning `None`, so it is
/// not free. `_held` keeps the lock taken for the whole measurement.
fn bench_try_lock_miss(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync/try_lock_miss");

    let spin = SpinLock::new(0u64);
    let _held = spin.lock();
    group.bench_function("SpinLock", |b| b.iter(|| black_box(spin.try_lock().is_some())));

    let irq: IrqSpinLock<u64, NoopIrq> = IrqSpinLock::new(0);
    let _irq_held = irq.lock();
    group.bench_function("IrqSpinLock", |b| b.iter(|| black_box(irq.try_lock().is_some())));

    group.finish();
}

criterion_group!(benches, bench_uncontended, bench_try_lock_miss);
criterion_main!(benches);
