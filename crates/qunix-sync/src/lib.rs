#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
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

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard { lock: self })
    }
}

pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

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
    _irq: core::marker::PhantomData<I>,
    inner: SpinLock<T>,
}

unsafe impl<T: ?Sized + Send, I: IrqControl> Send for IrqSpinLock<T, I> {}
unsafe impl<T: ?Sized + Send, I: IrqControl> Sync for IrqSpinLock<T, I> {}

impl<T, I: IrqControl> IrqSpinLock<T, I> {
    pub const fn new(value: T) -> Self {
        Self { _irq: core::marker::PhantomData, inner: SpinLock::new(value) }
    }

    pub fn lock(&self) -> IrqSpinLockGuard<'_, T, I> {
        let was_enabled = I::disable_and_save();
        let guard = self.inner.lock();
        IrqSpinLockGuard { guard: Some(guard), was_enabled, _irq: core::marker::PhantomData }
    }
}

pub struct IrqSpinLockGuard<'a, T: ?Sized, I: IrqControl> {
    guard: Option<SpinLockGuard<'a, T>>,
    was_enabled: bool,
    _irq: core::marker::PhantomData<I>,
}

impl<T: ?Sized, I: IrqControl> Deref for IrqSpinLockGuard<'_, T, I> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().unwrap()
    }
}

impl<T: ?Sized, I: IrqControl> DerefMut for IrqSpinLockGuard<'_, T, I> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().unwrap()
    }
}

impl<T: ?Sized, I: IrqControl> Drop for IrqSpinLockGuard<'_, T, I> {
    fn drop(&mut self) {
        // Release the spinlock before restoring interrupts, so an interrupt
        // handler that takes the same lock cannot deadlock against us.
        self.guard.take();
        I::restore(self.was_enabled);
    }
}

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
}
