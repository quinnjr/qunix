#![cfg_attr(not(any(test, feature = "std")), no_std)]

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;

/// Spins a `deadlock-panic` build tolerates before calling a lock wedged.
///
/// Generous on purpose. Real contention here is a handful of critical sections
/// tens of instructions long, so twenty million iterations is several orders
/// of magnitude beyond anything a live holder can take -- even under TCG, where
/// every guest instruction is translated. Set low enough to fire on genuine
/// contention it would turn a slow machine into a failing one.
#[cfg(feature = "deadlock-panic")]
const DEADLOCK_SPINS: u64 = 20_000_000;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
    /// Blocks until the lock is free.
    ///
    /// **Not reentrant, by design.** A context that already holds this lock and
    /// calls `lock` again spins forever against itself; there is no owner field
    /// to notice, and adding one would be wrong rather than merely expensive —
    /// a recursive acquisition would hand out a second `&mut T` while the first
    /// is still live. The reentrancy that actually occurs here is an interrupt
    /// handler landing on a lock the interrupted frame holds, and the fix for
    /// that is [`IrqSpinLock`], which makes the interrupt not arrive.
    ///
    /// Code that may already hold the lock must use [`Self::try_lock`] and have
    /// a fallback, which is what the console's panic path does. The
    /// non-reentrancy is observable there rather than here: a test that
    /// demonstrated it by calling `lock` twice would hang instead of failing,
    /// so `try_lock_fails_while_held` asserts the same fact in the form that
    /// can be asserted.
    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // Bounded under `deadlock-panic`, unbounded otherwise.
        //
        // An unbounded spin is the right shipped behaviour and the worst
        // possible test behaviour: a lock that is never released -- taken
        // twice by one CPU, or with its word overwritten -- stops every
        // processor here with interrupts masked, so no timer fires, no
        // watchdog runs and no panic prints. The whole machine goes quiet and
        // the harness reports only that QEMU never exited. That is exactly how
        // this was found, and it cost a day of bisecting environments.
        //
        // The bound is not a timeout to recover from. It converts silence into
        // a panic, and the panic's backtrace names the waiter -- including, for
        // a recursive acquisition, the outer frame that already holds it.
        #[cfg(feature = "deadlock-panic")]
        let mut spins: u64 = 0;
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            // Reset on every observation that the lock is free. Without this
            // the count accumulates across acquisitions by other processors and
            // measures how long this one has been *unlucky* rather than how
            // long the lock has been held -- which on a host that multiplexes
            // the guest's processors is a routine amount.
            #[cfg(feature = "deadlock-panic")]
            {
                spins = 0;
            }
            while self.locked.load(Ordering::Relaxed) {
                #[cfg(feature = "deadlock-panic")]
                {
                    spins += 1;
                    assert!(
                        spins < DEADLOCK_SPINS,
                        "spinlock held continuously for {DEADLOCK_SPINS} spins; nothing is going \
                         to release it. Either this processor already holds it, or its word was \
                         overwritten."
                    );
                }
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

// This impl *grants* `Sync`, it does not narrow an auto-derive. The
// `PhantomData<*const ()>` above makes the guard neither `Send` nor `Sync`, so
// without this line it would be unconditionally `!Sync` and `&guard` could not
// be shared at all. The bound is `T: Sync` rather than `T: Send` because two
// threads holding `&guard` can both `deref()` and hold `&T` concurrently, which
// is unsound for `Send + !Sync` types such as `Cell`. Same bound as
// `std::sync::MutexGuard`, for the same reason.
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

    /// Work this processor must keep doing while it waits with interrupts
    /// masked.
    ///
    /// Masking to take a lock makes the waiter deaf to every interrupt --
    /// including one another processor is waiting on it to answer. A TLB
    /// shootdown is exactly that: the initiator does not return until every
    /// other CPU acknowledges, so a CPU that masks and spins for a lock the
    /// initiator holds deadlocks the machine.
    ///
    /// Defaulted to nothing, so the host tests -- which have no interrupts to
    /// be deaf to -- are unaffected.
    fn service_while_waiting() {}

    /// Records that this processor has taken an irq-masking lock.
    ///
    /// Paired with [`Self::left_lock`]. The arch layer keeps the depth so that
    /// code which re-enables interrupts by hand -- `sti; hlt` in the idle and
    /// park loops -- can refuse to do so while a lock is held. Enabling
    /// interrupts there lets a handler take a lock the interrupted frame owns,
    /// on the same processor, which no amount of masking discipline elsewhere
    /// can save.
    fn entered_lock() {}
    /// Pairs with [`Self::entered_lock`].
    fn left_lock() {}

    /// This processor's index, for reporting who holds a wedged lock.
    ///
    /// Must not read per-CPU state through a segment base a running process
    /// could have zeroed; see the arch implementation. `u32::MAX` means
    /// "unknown", which is what the host tests report.
    fn cpu_index() -> u32 {
        u32::MAX
    }
}

pub struct IrqSpinLock<T: ?Sized, I: IrqControl> {
    // PhantomData is zero-sized but must precede `inner`: `SpinLock<T>` is
    // `?Sized`, so it has to be the final field.
    _irq: PhantomData<I>,
    /// Which processor holds this, or `NO_OWNER`.
    ///
    /// Diagnostic only, and it earns its place: "the lock is held and nobody is
    /// running" and "this processor already holds it" are the same silent
    /// machine-wide stop, and they need opposite fixes. One relaxed store on a
    /// line this processor has just taken exclusively.
    owner: AtomicU32,
    inner: SpinLock<T>,
}

/// `owner` value meaning the lock is free.
const NO_OWNER: u32 = u32::MAX - 1;

unsafe impl<T: ?Sized + Send, I: IrqControl> Send for IrqSpinLock<T, I> {}
unsafe impl<T: ?Sized + Send, I: IrqControl> Sync for IrqSpinLock<T, I> {}

impl<T, I: IrqControl> IrqSpinLock<T, I> {
    pub const fn new(value: T) -> Self {
        Self { _irq: PhantomData, owner: AtomicU32::new(NO_OWNER), inner: SpinLock::new(value) }
    }
}

// Split from the `T: Sized` block above so `IrqSpinLock<dyn Trait, I>` can
// actually be locked; the field ordering exists to permit exactly that.
impl<T: ?Sized, I: IrqControl> IrqSpinLock<T, I> {
    #[inline]
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T, I> {
        let was_enabled = I::disable_and_save();
        // Not `inner.lock()`, which spins and does nothing else. Interrupts
        // are masked from the line above, so this processor cannot answer the
        // shootdown whose initiator may be holding the very lock being waited
        // for; see `IrqControl::service_while_waiting`.
        #[cfg(feature = "deadlock-panic")]
        let mut spins: u64 = 0;
        // The owner this wait has been watching. A spin count alone cannot tell
        // a wedge from starvation: on a host with fewer processors than the
        // guest has, a spinning processor burns its whole timeslice while the
        // owner is not scheduled at all, and reaches any iteration bound while
        // the lock is being acquired and released normally by everybody else.
        // That is what this detector did on CI -- it fired once with the owner
        // reading `NO_OWNER`, i.e. against a lock that was free.
        //
        // So the count measures *one owner holding it without change*, which is
        // the wedge condition, and any change of hands resets it.
        #[cfg(feature = "deadlock-panic")]
        let mut watched: u32 = NO_OWNER;
        let guard = loop {
            if let Some(guard) = self.inner.try_lock() {
                break guard;
            }
            I::service_while_waiting();
            #[cfg(feature = "deadlock-panic")]
            {
                let owner = self.owner.load(Ordering::Relaxed);
                if owner != watched {
                    // Changed hands, or was released: the lock is moving, so
                    // this is contention rather than a wedge.
                    watched = owner;
                    spins = 0;
                }
                spins += 1;
                if spins >= DEADLOCK_SPINS && owner != NO_OWNER {
                    let me = I::cpu_index();
                    // Reported rather than merely detected. "Held by this same
                    // processor" is a recursive acquisition; "held by another"
                    // with every processor spinning means the holder is not
                    // running at all, which is a lock kept across a context
                    // switch. Those are different bugs and the message has to
                    // say which.
                    panic!(
                        "irq spinlock wedged: cpu {me} waited {DEADLOCK_SPINS} spins while cpu \
                         {owner} held it without ever releasing it. Same cpu means a recursive \
                         acquisition; a different one means the owner is not on any processor -- \
                         the lock was held across a context switch."
                    );
                }
            }
            core::hint::spin_loop();
        };
        self.owner.store(I::cpu_index(), Ordering::Relaxed);
        I::entered_lock();
        IrqSpinLockGuard {
            guard: ManuallyDrop::new(guard),
            owner: &self.owner,
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
            Some(guard) => {
                self.owner.store(I::cpu_index(), Ordering::Relaxed);
                I::entered_lock();
                Some(IrqSpinLockGuard {
                    guard: ManuallyDrop::new(guard),
                    owner: &self.owner,
                    was_enabled,
                    _irq: PhantomData,
                    _not_send: PhantomData,
                })
            }
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
    owner: &'a AtomicU32,
    was_enabled: bool,
    _irq: PhantomData<I>,
    // `!Send` for a sharper reason than the plain guard: dropping this on a
    // different CPU would run `I::restore` there, using an interrupt flag
    // captured on the originating CPU — leaving that CPU masked forever.
    _not_send: PhantomData<*const ()>,
}

// As `SpinLockGuard`: grants `Sync` that the `PhantomData<*const ()>` above
// otherwise denies, at the bound that keeps `&T` sharing sound.
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
        // Cleared before the release, not after: between the two the lock is
        // free, and a processor that took it in that gap would have its own
        // owner store overwritten by this one.
        self.owner.store(NO_OWNER, Ordering::Relaxed);
        I::left_lock();
        // Release the spinlock before restoring interrupts, so an interrupt
        // handler that takes the same lock cannot deadlock against us.
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        I::restore(self.was_enabled);
    }
}

/// `IrqControl` that masks nothing.
///
/// Used by the auto-trait assertions below, and by the host tests: it makes
/// `IrqSpinLock`'s guard lifecycle testable off the machine, which is
/// otherwise reachable only from the kernel where a failure is a hang rather
/// than an assertion. It cannot witness the *restore* half of the contract —
/// its `restore` is empty — so that is covered by the tests' `CountingIrq`.
///
/// Deliberately not `pub`: an `IrqSpinLock<T, NoIrq>` in kernel code is a lock
/// that does not mask interrupts while reading as though it does, which is the
/// self-deadlock this whole type exists to prevent.
pub(crate) struct NoIrq;
impl IrqControl for NoIrq {
    fn disable_and_save() -> bool {
        false
    }
    fn restore(_: bool) {}
}

/// Witnesses whether `T: Send`, without requiring it.
///
/// The inherent `IS_SEND` applies only when `T: Send`, and an inherent
/// associated const outranks a trait one during path resolution, so
/// `SendWitness::<T>::IS_SEND` resolves to the inherent `true` for a `Send`
/// type and falls back to the trait's `false` otherwise. That makes the
/// *absence* of an auto-trait assertable at compile time, which the negative
/// assertions below depend on: every `!Send`/`!Sync` property in this crate is
/// carried by a `PhantomData` field or an `unsafe impl` bound, all of which can
/// be deleted without breaking a single positive assertion.
struct SendWitness<T: ?Sized>(PhantomData<*const T>);
trait NotSend {
    const IS_SEND: bool = false;
}
impl<T: ?Sized> NotSend for SendWitness<T> {}
impl<T: ?Sized + Send> SendWitness<T> {
    const IS_SEND: bool = true;
}

/// As [`SendWitness`], for `Sync`.
struct SyncWitness<T: ?Sized>(PhantomData<*const T>);
trait NotSync {
    const IS_SYNC: bool = false;
}
impl<T: ?Sized> NotSync for SyncWitness<T> {}
impl<T: ?Sized + Sync> SyncWitness<T> {
    const IS_SYNC: bool = true;
}

/// Compile-time assertions on the locks' and guards' auto-traits.
///
/// These are the properties that make the locks sound, and they are invisible
/// in ordinary tests: an accidental auto-derive would compile and pass every
/// runtime test while allowing a data race. Kept outside `#[cfg(test)]` so they
/// are checked on every build, including the kernel target.
///
/// Both directions are asserted, and both matter. The positive ones fail if a
/// bound is *tightened* into uselessness; the negative ones fail if one is
/// loosened into unsoundness — and a loosened bound is the one that compiles,
/// passes, and races.
const _: () = {
    // The locks themselves are shareable and sendable for `T: Send`. That
    // includes a `Send + !Sync` payload such as `Cell`: serialising access to
    // it is the entire job.
    assert!(SendWitness::<SpinLock<u32>>::IS_SEND);
    assert!(SyncWitness::<SpinLock<u32>>::IS_SYNC);
    assert!(SyncWitness::<SpinLock<core::cell::Cell<u32>>>::IS_SYNC);
    // ...but only for `T: Send`. Without that bound on the two `unsafe impl`s,
    // a `SpinLock<Rc<_>>` in a static would let two CPUs clone one `Rc` and
    // race its non-atomic count. `*const ()` stands in for any `!Send` payload
    // and needs no allocator, so this holds on the kernel target too.
    assert!(!SendWitness::<SpinLock<*const ()>>::IS_SEND);
    assert!(!SyncWitness::<SpinLock<*const ()>>::IS_SYNC);

    // The same four properties, restated for the IRQ pair: they are separate
    // types with separate `PhantomData` and separate `unsafe impl`s, so an
    // omission in one is not caught by the other.
    assert!(SendWitness::<IrqSpinLock<u32, NoIrq>>::IS_SEND);
    assert!(SyncWitness::<IrqSpinLock<u32, NoIrq>>::IS_SYNC);
    assert!(SyncWitness::<IrqSpinLock<core::cell::Cell<u32>, NoIrq>>::IS_SYNC);
    assert!(!SendWitness::<IrqSpinLock<*const (), NoIrq>>::IS_SEND);
    assert!(!SyncWitness::<IrqSpinLock<*const (), NoIrq>>::IS_SYNC);

    // A guard over a `Sync` payload is shareable...
    assert!(SyncWitness::<SpinLockGuard<'static, u32>>::IS_SYNC);
    assert!(SyncWitness::<IrqSpinLockGuard<'static, u32, NoIrq>>::IS_SYNC);
    // ...but only at `T: Sync`, not `T: Send`. Two threads holding `&guard` can
    // both `deref()` into `&T` at once, so a `Sync` guard over a `Cell` would
    // hand out concurrent `&Cell` — the bound on the `unsafe impl` is the only
    // thing preventing it, and relaxing it to `T: Send` breaks nothing that
    // does not check this.
    assert!(!SyncWitness::<SpinLockGuard<'static, core::cell::Cell<u32>>>::IS_SYNC);
    assert!(!SyncWitness::<IrqSpinLockGuard<'static, core::cell::Cell<u32>, NoIrq>>::IS_SYNC);

    // No guard is ever `Send`, whatever the payload. Releasing a lock from a
    // context that never took it is the plain guard's problem; the IRQ guard's
    // is sharper still, since its `Drop` would run `I::restore` on the wrong
    // CPU with a flag captured on another, leaving the originating CPU masked
    // forever. Both rest entirely on a zero-sized `PhantomData<*const ()>`
    // field, which is a deletion away from being lost silently.
    assert!(!SendWitness::<SpinLockGuard<'static, u32>>::IS_SEND);
    assert!(!SendWitness::<IrqSpinLockGuard<'static, u32, NoIrq>>::IS_SEND);
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
    fn a_failed_try_lock_does_not_release_the_lock() {
        // The negative half of `try_lock_fails_while_held`: returning `None` is
        // right, but a failure arm that also stored `false` -- the shape of an
        // unconditional cleanup path -- would release a lock this context does
        // not hold, and every later attempt would succeed against a live guard.
        let lock = SpinLock::new(0u32);
        let held = lock.lock();
        for _ in 0..4 {
            assert!(lock.try_lock().is_none(), "a repeated failed attempt eventually succeeded");
        }
        drop(held);
        assert!(lock.try_lock().is_some(), "the failed attempts left the lock held");
    }

    #[test]
    fn force_unlock_reclaims_a_plain_lock_whose_guard_will_never_drop() {
        // Only the `IrqSpinLock` wrapper was covered. The plain lock is what the
        // serial console's panic path actually reclaims, and its `force_unlock`
        // and `data_ptr` are separate functions from the wrapper's -- the
        // wrapper's tests would stay green if either of these were emptied out.
        let lock = SpinLock::new(99u32);

        // Stands in for the frame that was interrupted mid-critical-section and
        // will never resume: its guard is never dropped.
        core::mem::forget(lock.lock());
        assert!(lock.try_lock().is_none(), "the forgotten guard did not leave the lock held");

        unsafe { lock.force_unlock() };
        let guard = lock.try_lock().expect("force_unlock did not release the lock");
        assert_eq!(*guard, 99);
        drop(guard);

        // The panic path writes through `data_ptr` after reclaiming the lock
        // this way, so the pointer must address the guarded value itself and
        // not a copy.
        unsafe { *lock.data_ptr() = 7 };
        assert_eq!(*lock.lock(), 7, "data_ptr did not address the guarded value");
    }

    #[test]
    fn irq_spinlock_guards_the_value_and_releases_on_drop() {
        let lock: IrqSpinLock<u32, NoIrq> = IrqSpinLock::new(5);
        {
            let mut guard = lock.lock();
            *guard += 1;
            // Held: a second attempt must fail rather than hand out a second
            // `&mut` to the same value.
            assert!(lock.try_lock().is_none(), "try_lock succeeded while the lock was held");
        }
        assert_eq!(*lock.lock(), 6, "the write through the guard was lost");
    }

    /// An `IrqControl` that reports a fixed processor index.
    struct CpuSeven;
    impl IrqControl for CpuSeven {
        fn disable_and_save() -> bool {
            false
        }
        fn restore(_: bool) {}
        fn cpu_index() -> u32 {
            7
        }
    }

    #[test]
    fn a_held_lock_names_its_owner_and_a_free_one_names_nobody() {
        // The owner is what distinguishes a recursive acquisition from a lock
        // kept across a context switch, and those need opposite fixes. A field
        // that stayed at its initial value would report "held by nobody" for
        // every wedged lock -- the least useful of the two answers, and
        // indistinguishable from the bug it is meant to identify.
        let lock: IrqSpinLock<u32, CpuSeven> = IrqSpinLock::new(0);
        assert_eq!(lock.owner.load(Ordering::Relaxed), NO_OWNER, "a fresh lock claims an owner");
        {
            let _guard = lock.lock();
            assert_eq!(
                lock.owner.load(Ordering::Relaxed),
                7,
                "a held lock does not name the processor holding it"
            );
        }
        assert_eq!(
            lock.owner.load(Ordering::Relaxed),
            NO_OWNER,
            "the owner outlived the guard, so the next wedge would blame a processor that had \
             already released"
        );
    }

    #[test]
    fn try_lock_records_the_owner_too() {
        // The path a panic handler takes. An owner recorded only by `lock`
        // leaves every `try_lock` acquisition invisible, so a wedge against one
        // reports "held by nobody" -- which reads as corruption rather than as
        // the contention it is.
        let lock: IrqSpinLock<u32, CpuSeven> = IrqSpinLock::new(0);
        let guard = lock.try_lock().expect("an uncontended try_lock failed");
        assert_eq!(lock.owner.load(Ordering::Relaxed), 7, "try_lock did not record its owner");
        drop(guard);
        assert_eq!(lock.owner.load(Ordering::Relaxed), NO_OWNER);
    }

    // Thread-local, not global: the counters are read as a baseline and then
    // asserted to have moved by exactly one. Process-wide statics would make
    // that assertion depend on no *other* test using `CountingIrq`
    // concurrently, which the default parallel harness does not guarantee.
    thread_local! {
        static DISABLES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
        static RESTORES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    /// Counts its calls, so the *restore* half of the contract is observable.
    ///
    /// `NoIrq` cannot serve here: its `restore` is empty, so deleting the
    /// `I::restore(was_enabled)` from `try_lock`'s failure arm would leave any
    /// test built on it green.
    struct CountingIrq;
    impl IrqControl for CountingIrq {
        fn disable_and_save() -> bool {
            DISABLES.with(|c| c.set(c.get() + 1));
            true
        }
        fn restore(_: bool) {
            RESTORES.with(|c| c.set(c.get() + 1));
        }
    }

    #[test]
    fn irq_spinlock_try_lock_restores_state_when_it_fails() {
        let lock: IrqSpinLock<u32, CountingIrq> = IrqSpinLock::new(0);
        let held = lock.lock();
        let disables = DISABLES.with(|c| c.get());
        let restores = RESTORES.with(|c| c.get());

        assert!(lock.try_lock().is_none());
        // The failed attempt saved the flag and must have given it back. A
        // version that forgot would leave interrupts masked on every miss.
        assert_eq!(
            DISABLES.with(|c| c.get()),
            disables + 1,
            "try_lock did not save the flag"
        );
        assert_eq!(
            RESTORES.with(|c| c.get()),
            restores + 1,
            "the failed try_lock did not restore the flag it saved"
        );

        drop(held);
        assert!(lock.try_lock().is_some(), "the lock stayed held after a failed try_lock");
    }

    #[test]
    fn lock_takes_the_backoff_path_when_genuinely_contended() {
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        // `contended_across_threads_never_loses_increments` only reaches the
        // backoff loop when the scheduler happens to overlap two threads, which
        // is a property of the host's core count rather than of the code. This
        // test forces the overlap: the lock is provably held before the main
        // thread asks for it, so `try_lock` must fail and `lock` must spin.
        //
        // A `Barrier` rather than a flag and a busy-wait. The busy-wait was
        // itself scheduling-dependent -- on a CI runner the flag was already
        // set at the first check, so its body never executed, and the two lines
        // showed as a coverage regression on a commit that had not touched this
        // crate. The barrier orders the two threads without a spin at all.
        let lock = Arc::new(SpinLock::new(0u32));
        let ready = Arc::new(Barrier::new(2));

        let holder_lock = Arc::clone(&lock);
        let holder_ready = Arc::clone(&ready);
        let holder = std::thread::spawn(move || {
            let mut guard = holder_lock.lock();
            // Released only after the sleep below, so the main thread is
            // guaranteed to find the lock held when it wakes from the barrier.
            holder_ready.wait();
            std::thread::sleep(Duration::from_millis(50));
            *guard = 7;
        });

        ready.wait();
        // Blocks until the holder drops its guard, spinning in the backoff loop.
        //
        // The elapsed-time assertion is the point of the test, not decoration.
        // The barrier orders *entry* -- it guarantees the lock is held when this
        // thread wakes -- but if this thread were descheduled past the holder's
        // sleep, `try_lock` would succeed on the first attempt, the backoff loop
        // would never run, and without this the test would still pass green
        // while the coverage it exists to produce silently disappeared. That is
        // exactly the failure that made the ratchet flake on CI.
        let started = std::time::Instant::now();
        let guard = lock.lock();
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "lock() returned in {:?} without blocking; the backoff loop was not exercised",
            started.elapsed()
        );
        assert_eq!(*guard, 7, "acquired before the holder finished writing");
        drop(guard);
        holder.join().unwrap();
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

    // The guard's `Drop` releases the spinlock *before* restoring the interrupt
    // flag, so an interrupt handler that takes the same lock cannot arrive
    // while it is still held. The order is one line either way and swapping it
    // leaves every other test in this file green -- the flag still ends up
    // restored and the lock still ends up released, just in an order that
    // reintroduces the self-deadlock the type exists to prevent. This
    // `IrqControl` catches it by looking at the lock from inside `restore`.
    thread_local! {
        static RELEASE_ORDER_LOCK: core::cell::Cell<*const SpinLock<u32>> =
            const { core::cell::Cell::new(core::ptr::null()) };
        static LOCK_WAS_FREE_AT_RESTORE: core::cell::Cell<Option<bool>> =
            const { core::cell::Cell::new(None) };
    }

    struct ReleaseOrderIrq;
    impl IrqControl for ReleaseOrderIrq {
        fn disable_and_save() -> bool {
            true
        }
        fn restore(_: bool) {
            let inner = RELEASE_ORDER_LOCK.with(|c| c.get());
            if inner.is_null() {
                return;
            }
            // Stands in for the interrupt that becomes deliverable the instant
            // the flag is restored: the first thing its handler does is ask for
            // the lock. `try_lock` on the inner `SpinLock` rather than on the
            // wrapper, which would recurse back into this function.
            let taken = unsafe { (*inner).try_lock() };
            LOCK_WAS_FREE_AT_RESTORE.with(|c| c.set(Some(taken.is_some())));
        }
    }

    #[test]
    fn the_guard_releases_the_lock_before_it_restores_interrupts() {
        let lock: IrqSpinLock<u32, ReleaseOrderIrq> = IrqSpinLock::new(1);
        // The pointer names a lock owned by this frame, which outlives the drop
        // below; it is cleared before returning so no later `restore` on this
        // thread can follow it.
        RELEASE_ORDER_LOCK.with(|c| c.set(&raw const lock.inner));
        LOCK_WAS_FREE_AT_RESTORE.with(|c| c.set(None));

        drop(lock.lock());

        let observed = LOCK_WAS_FREE_AT_RESTORE.with(|c| c.get());
        RELEASE_ORDER_LOCK.with(|c| c.set(core::ptr::null()));
        assert_eq!(
            observed,
            Some(true),
            "interrupts were restored while the lock was still held: an interrupt \
             handler taking this lock would deadlock against the frame releasing it"
        );
    }

    // A contended `lock` must mask *before* it starts waiting, not after it
    // acquires. Masking after would leave a CPU preemptible while it spins, and
    // an interrupt handler that takes the same lock would then deadlock against
    // the frame it interrupted -- the whole reason this type exists.
    //
    // Signalled from inside `disable_and_save` rather than observed by polling
    // a counter: a poll loop is only reached if the waiter has already got
    // there, so its body's coverage would depend on the host's core count.
    // This ordering is forced. If `lock` acquired before masking, the waiter's
    // `disable_and_save` would not run until the holder released, the `recv`
    // below would time out, and the test fails rather than hanging.
    std::thread_local! {
        static MASK_SIGNAL: core::cell::RefCell<Option<std::sync::mpsc::Sender<usize>>> =
            const { core::cell::RefCell::new(None) };
    }
    static MASK_FIRST_DEPTH: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    struct MaskFirstIrq;
    impl IrqControl for MaskFirstIrq {
        fn disable_and_save() -> bool {
            let depth = MASK_FIRST_DEPTH.fetch_add(1, Ordering::SeqCst) + 1;
            MASK_SIGNAL.with(|slot| {
                if let Some(tx) = slot.borrow().as_ref() {
                    let _ = tx.send(depth);
                }
            });
            true
        }
        fn restore(_: bool) {
            MASK_FIRST_DEPTH.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_contended_irq_lock_masks_before_it_waits_and_leaks_no_mask() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::time::Duration;

        let lock: Arc<IrqSpinLock<u32, MaskFirstIrq>> = Arc::new(IrqSpinLock::new(0));
        let (tx, rx) = mpsc::channel();
        let ready = Arc::new(Barrier::new(2));

        // Held for the whole of the waiter's attempt, so the waiter provably
        // cannot acquire and anything it reports comes from the waiting path.
        let held = lock.lock();
        assert_eq!(MASK_FIRST_DEPTH.load(Ordering::SeqCst), 1);

        let waiter_lock = Arc::clone(&lock);
        let waiter_ready = Arc::clone(&ready);
        let waiter = std::thread::spawn(move || {
            // Only this thread reports, so the holder's own masking above is
            // not mistaken for the waiter's.
            MASK_SIGNAL.with(|slot| *slot.borrow_mut() = Some(tx));
            waiter_ready.wait();
            *waiter_lock.lock() = 7;
        });

        ready.wait();
        let depth = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a blocked lock() never masked interrupts; it waits while preemptible");
        assert_eq!(depth, 2, "the waiter did not hold its own mask while it waited");
        assert_eq!(*held, 0, "the waiter acquired the lock while it was held");

        drop(held);
        waiter.join().unwrap();
        assert_eq!(
            MASK_FIRST_DEPTH.load(Ordering::SeqCst),
            0,
            "a contended acquire/release left a mask outstanding"
        );
        assert_eq!(*lock.lock(), 7, "the waiter's write through the guard was lost");
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
