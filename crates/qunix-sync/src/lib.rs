#![cfg_attr(not(any(test, feature = "std")), no_std)]

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> SpinLock<T> {
    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard { lock: self, _not_send: PhantomData })
    }

    /// Releases the lock without holding a guard.
    ///
    /// Exists for the panic path: a fault taken while the console lock is held
    /// would otherwise leave the panic handler spinning on a lock the
    /// interrupted frame will never release, and the panic message would never
    /// be emitted.
    ///
    /// # Safety
    /// The caller must be certain no live guard exists, or that the context
    /// owning it will never resume. Calling this while another context is
    /// genuinely mid-critical-section allows concurrent `&mut T`.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    /// Raw pointer to the guarded value.
    ///
    /// Dereferencing the returned pointer bypasses the lock; that is only sound
    /// on a recovery path that has already established, via
    /// [`Self::force_unlock`], that no other context holding the lock will
    /// resume — see the panic path in the kernel's serial console.
    pub fn data_ptr(&self) -> *mut T {
        self.data.get()
    }
}

pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
    // Makes the guard `!Send`. A guard represents exclusive access held by one
    // execution context; moving it elsewhere and dropping it there would
    // release a lock the other context never took. `MutexGuard` is `!Send` for
    // the same reason.
    _not_send: PhantomData<*const ()>,
}

// Without this, the guard would auto-derive `Sync` from `&SpinLock<T>`, i.e.
// whenever `T: Send`. Two threads sharing `&guard` could then both `deref()`
// and hold `&T` concurrently, which is unsound for `Send + !Sync` types such as
// `Cell`. The bound must be `T: Sync`, matching `std::sync::MutexGuard`.
unsafe impl<T: ?Sized + Sync> Sync for SpinLockGuard<'_, T> {}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// Hook the arch layer installs so `IrqSpinLock` can mask interrupts.
/// Returns the previous interrupt-enable state.
pub trait IrqControl {
    fn disable_and_save() -> bool;
    fn restore(was_enabled: bool);
}

pub struct IrqSpinLock<T: ?Sized, I: IrqControl> {
    // PhantomData is zero-sized but must precede `inner`: `SpinLock<T>` is
    // `?Sized`, so it has to be the final field.
    _irq: PhantomData<I>,
    inner: SpinLock<T>,
}

unsafe impl<T: ?Sized + Send, I: IrqControl> Send for IrqSpinLock<T, I> {}
unsafe impl<T: ?Sized + Send, I: IrqControl> Sync for IrqSpinLock<T, I> {}

impl<T, I: IrqControl> IrqSpinLock<T, I> {
    pub const fn new(value: T) -> Self {
        Self { _irq: PhantomData, inner: SpinLock::new(value) }
    }
}

// Split from the `T: Sized` block above so `IrqSpinLock<dyn Trait, I>` can
// actually be locked; the field ordering exists to permit exactly that.
impl<T: ?Sized, I: IrqControl> IrqSpinLock<T, I> {
    #[inline]
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T, I> {
        let was_enabled = I::disable_and_save();
        let guard = self.inner.lock();
        IrqSpinLockGuard {
            guard: ManuallyDrop::new(guard),
            was_enabled,
            _irq: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// Returns `None` if the lock is held, restoring the interrupt state first
    /// so a failed attempt has no lasting effect.
    #[inline]
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T, I>> {
        let was_enabled = I::disable_and_save();
        match self.inner.try_lock() {
            Some(guard) => Some(IrqSpinLockGuard {
                guard: ManuallyDrop::new(guard),
                was_enabled,
                _irq: PhantomData,
                _not_send: PhantomData,
            }),
            None => {
                I::restore(was_enabled);
                None
            }
        }
    }

    /// # Safety
    /// Same contract as [`SpinLock::force_unlock`]. Note this does NOT restore
    /// interrupt state — the guard that would have done so is gone.
    pub unsafe fn force_unlock(&self) {
        unsafe { self.inner.force_unlock() };
    }

    /// See [`SpinLock::data_ptr`].
    pub fn data_ptr(&self) -> *mut T {
        self.inner.data_ptr()
    }
}

pub struct IrqSpinLockGuard<'a, T: ?Sized, I: IrqControl> {
    // ManuallyDrop rather than Option: Drop needs to release the spinlock
    // before restoring IF, and an Option would put a null check plus a cold
    // unwrap-failed call on every deref -- which is now every Box/Vec op,
    // every frame allocation, and every console byte.
    guard: ManuallyDrop<SpinLockGuard<'a, T>>,
    was_enabled: bool,
    _irq: PhantomData<I>,
    // `!Send` for a sharper reason than the plain guard: dropping this on a
    // different CPU would run `I::restore` there, using an interrupt flag
    // captured on the originating CPU — leaving that CPU masked forever.
    _not_send: PhantomData<*const ()>,
}

unsafe impl<T: ?Sized + Sync, I: IrqControl> Sync for IrqSpinLockGuard<'_, T, I> {}

impl<T: ?Sized, I: IrqControl> Deref for IrqSpinLockGuard<'_, T, I> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized, I: IrqControl> DerefMut for IrqSpinLockGuard<'_, T, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T: ?Sized, I: IrqControl> Drop for IrqSpinLockGuard<'_, T, I> {
    fn drop(&mut self) {
        // Release the spinlock before restoring interrupts, so an interrupt
        // handler that takes the same lock cannot deadlock against us.
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        I::restore(self.was_enabled);
    }
}

/// Compile-time assertions on the guards' auto-traits.
///
/// These are the properties that make the locks sound, and they are invisible
/// in ordinary tests: an accidental auto-derive would compile and pass every
/// runtime test while allowing a data race. Kept outside `#[cfg(test)]` so they
/// are checked on every build, including the kernel target.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    const fn assert_send<T: Send>() {}

    // The locks themselves are shareable and sendable for `T: Send`.
    let _ = assert_sync::<SpinLock<u32>>;
    let _ = assert_send::<SpinLock<u32>>;
    // A guard over a `Sync` payload is shareable...
    let _ = assert_sync::<SpinLockGuard<'static, u32>>;
    // ...but no guard is ever `Send`. There is no positive way to assert the
    // absence of a trait on stable, so the `PhantomData<*const ()>` fields are
    // the mechanism and this comment is the record of intent.
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_grants_mutable_access() {
        let lock = SpinLock::new(41);
        *lock.lock() += 1;
        assert_eq!(*lock.lock(), 42);
    }

    #[test]
    fn try_lock_fails_while_held() {
        let lock = SpinLock::new(0);
        let _guard = lock.lock();
        assert!(lock.try_lock().is_none());
    }

    #[test]
    fn try_lock_succeeds_after_drop() {
        let lock = SpinLock::new(0);
        drop(lock.lock());
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn contended_across_threads_never_loses_increments() {
        use std::sync::Arc;
        let lock = Arc::new(SpinLock::new(0usize));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*lock.lock(), 8000);
    }

    struct FakeIrq;
    static IRQ_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    impl IrqControl for FakeIrq {
        fn disable_and_save() -> bool {
            IRQ_DEPTH.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn restore(_was_enabled: bool) {
            IRQ_DEPTH.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn irq_lock_restores_interrupt_state_on_drop() {
        let lock: IrqSpinLock<u32, FakeIrq> = IrqSpinLock::new(7);
        assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 0);
        {
            let guard = lock.lock();
            assert_eq!(*guard, 7);
            assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 1);
        }
        assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 0);
    }

    // Each interrupt-accounting test gets its own counter type. `IRQ_DEPTH` is a
    // process-wide static and cargo runs tests on several threads at once, so a
    // shared counter would make every assertion on its value depend on which
    // other tests happen to be mid-critical-section.
    macro_rules! fake_irq {
        ($name:ident, $depth:ident) => {
            struct $name;
            static $depth: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            impl IrqControl for $name {
                fn disable_and_save() -> bool {
                    $depth.fetch_add(1, Ordering::SeqCst);
                    true
                }
                fn restore(_was_enabled: bool) {
                    $depth.fetch_sub(1, Ordering::SeqCst);
                }
            }
        };
    }

    fake_irq!(TryLockIrq, TRY_LOCK_DEPTH);

    #[test]
    fn failed_irq_try_lock_restores_interrupt_state() {
        let lock: IrqSpinLock<u32, TryLockIrq> = IrqSpinLock::new(3);
        let guard = lock.lock();
        assert_eq!(TRY_LOCK_DEPTH.load(Ordering::SeqCst), 1);

        // The failed attempt must undo its own `disable_and_save`; otherwise the
        // console's reentrancy path would mask interrupts permanently.
        assert!(lock.try_lock().is_none());
        assert_eq!(TRY_LOCK_DEPTH.load(Ordering::SeqCst), 1);

        drop(guard);
        assert_eq!(TRY_LOCK_DEPTH.load(Ordering::SeqCst), 0);
    }

    fake_irq!(ForceUnlockIrq, FORCE_UNLOCK_DEPTH);

    #[test]
    fn force_unlock_allows_a_subsequent_lock() {
        let lock: IrqSpinLock<u32, ForceUnlockIrq> = IrqSpinLock::new(99);

        // Stands in for a context that took the lock and will never resume: the
        // guard's Drop never runs, so neither the lock nor the interrupt flag is
        // released by the normal path.
        core::mem::forget(lock.lock());
        assert_eq!(FORCE_UNLOCK_DEPTH.load(Ordering::SeqCst), 1);

        unsafe { lock.force_unlock() };
        // `force_unlock` releases only the spinlock. The interrupt flag stays as
        // the forgotten guard left it -- there is no saved state to restore from.
        assert_eq!(FORCE_UNLOCK_DEPTH.load(Ordering::SeqCst), 1);

        let guard = lock.try_lock().expect("force_unlock must release the lock");
        assert_eq!(*guard, 99);
        drop(guard);

        // Reading through `data_ptr` is what the panic path does once it has
        // reclaimed the lock this way.
        assert_eq!(unsafe { *lock.data_ptr() }, 99);
    }

    fake_irq!(UnsizedIrq, UNSIZED_DEPTH);

    #[test]
    fn irq_lock_works_on_unsized_payload() {
        let sized: IrqSpinLock<[u32; 3], UnsizedIrq> = IrqSpinLock::new([1, 2, 3]);
        // The unsizing coercion is only possible because of the field ordering,
        // and `lock`/`try_lock` are only callable on the result because they
        // live in the `T: ?Sized` impl block. Both are checked here.
        let lock: &IrqSpinLock<[u32], UnsizedIrq> = &sized;

        {
            let mut guard = lock.lock();
            assert_eq!(&*guard, &[1, 2, 3]);
            guard[1] = 20;
        }
        assert_eq!(UNSIZED_DEPTH.load(Ordering::SeqCst), 0);

        let guard = lock.try_lock().expect("uncontended try_lock must succeed");
        assert_eq!(&*guard, &[1, 20, 3]);
        assert_eq!(UNSIZED_DEPTH.load(Ordering::SeqCst), 1);
        drop(guard);
        assert_eq!(UNSIZED_DEPTH.load(Ordering::SeqCst), 0);
    }
}
