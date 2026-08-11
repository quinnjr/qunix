//! The async runtime: one thread, one future, no allocation.
//!
//! # Why there is no task table
//!
//! The future being awaited is a local in [`block_on`]'s frame, pinned to the
//! kernel stack the thread already owns, and the thing the scheduler resumes is
//! the thread. A slab of `Pin<Box<dyn Future>>` tasks would buy the ability to
//! run more concurrent operations than there are threads, and would cost a heap
//! allocation per operation in the one subsystem where allocating is a deadlock
//! hazard: memory pressure triggers writeback, writeback needs I/O, and I/O
//! would need the allocation that is already waiting.
//!
//! The cost is stated rather than hidden: concurrency is bounded by thread
//! count. Two futures awaited inside one thread work, because one `block_on`
//! polls both; running them on two processors does not.
//!
//! # Why the waker holds a bare id
//!
//! A `RawWaker` carries one `*const ()`. The usual occupant is an `Arc<Task>`,
//! whose `clone` and `drop` are refcount traffic an interrupt handler would
//! have to perform -- and whose *last* drop frees memory, from interrupt
//! context, entering the heap allocator underneath whatever lock the
//! interrupted code was holding. This one carries `ThreadId.0`, so `clone` is
//! the identity and `drop` does nothing.
//!
//! That is sound only because thread ids are never reused; see
//! `sched::Scheduler::allocate_id`, which says so and is guarded. A waker that
//! outlives its thread names an id that will never belong to anything else, and
//! `sched::unpark` ignores it.

use core::future::Future;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use qunix_sched::ThreadId;

/// Identity clone: the data word is a value, not a pointer to a refcount, so
/// there is nothing to increment and nothing that could later be freed.
///
/// # Safety
/// `data` must be a `ThreadId.0` produced by [`waker_for`].
unsafe fn clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &VTABLE)
}

/// # Safety
/// `data` must be a `ThreadId.0` produced by [`waker_for`].
unsafe fn wake(data: *const ()) {
    crate::sched::unpark(ThreadId(data as u64));
}

/// # Safety
/// `data` must be a `ThreadId.0` produced by [`waker_for`].
unsafe fn wake_by_ref(data: *const ()) {
    crate::sched::unpark(ThreadId(data as u64));
}

/// Nothing is owned, so nothing is released.
///
/// Deliberately not `unreachable!`: `Waker`'s drop runs on every temporary an
/// executor creates, so this is reached on the ordinary path rather than only
/// in error.
///
/// # Safety
/// `data` must be a `ThreadId.0` produced by [`waker_for`].
unsafe fn drop_nothing(_data: *const ()) {}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_nothing);

/// A `Waker` that unparks `id`.
///
/// Safe to hold across a thread's death and safe to call from an interrupt
/// handler: it allocates nothing, takes no lock of its own, and an id whose
/// thread has exited is ignored by `unpark`.
pub fn waker_for(id: ThreadId) -> Waker {
    // SAFETY: the data word is a `ThreadId.0`, which is exactly what every
    // function in `VTABLE` requires. `u64 as *const ()` is a provenance-free
    // integer that is never dereferenced -- only cast back to `u64`.
    unsafe { Waker::from_raw(RawWaker::new(id.0 as *const (), &VTABLE)) }
}

/// The thread id a waker built by [`waker_for`] names.
///
/// Exists so a test can assert the round trip, and so a future under test can
/// wake through the waker it was handed rather than through
/// `sched::current_id`. Getting the cast wrong is otherwise silent: `unpark`
/// would be handed an id naming nothing, find no thread, return, and every
/// `await` in the kernel would hang with no message.
#[cfg(test)]
pub fn raw_id_of(waker: &Waker) -> u64 {
    waker.data() as u64
}

/// Runs `future` to completion on the calling thread, parking between polls.
///
/// The future is pinned to this thread's kernel stack and never moves, so no
/// boxing is involved at any point.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = waker_for(crate::sched::current_id());
    let mut cx = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    loop {
        // Polled *before* parking, always. Parking first would block a future
        // that is already ready until something unrelated woke the thread, and
        // for the `Ready`-on-first-poll case -- which most cached reads will be
        // -- nothing ever would.
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        // A loop rather than a single park, because `park` may return without
        // the condition having become true: a spurious wake is always
        // permitted, and costs one extra poll.
        crate::sched::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static FLAG_READY: AtomicBool = AtomicBool::new(false);
    static FLAG_WAKER: AtomicU64 = AtomicU64::new(u64::MAX);

    /// A future that is `Pending` until a flag is set, and publishes the waker
    /// it was handed so another thread can complete it -- the shape every
    /// device future in T4 will have.
    struct Flag {
        polls: u64,
    }

    impl Future for Flag {
        type Output = u64;
        fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u64> {
            self.polls += 1;
            if FLAG_READY.load(Ordering::Acquire) {
                return Poll::Ready(self.polls);
            }
            // Taken from the `Waker` the executor handed us, *not* from
            // `sched::current_id()`. Reading the id directly would give the
            // right answer here and test nothing: the question is whether the
            // waker carries the thread it was built for, and a future that
            // bypassed it would pass with the waker plumbing removed entirely.
            FLAG_WAKER.store(raw_id_of(cx.waker()), Ordering::Release);
            Poll::Pending
        }
    }

    #[test_case]
    fn a_future_that_is_ready_immediately_never_parks() {
        // The cheap path, and the one a mistake in `block_on`'s loop makes
        // expensive: parking before the first poll would block every ready
        // future until something else happened to wake the thread -- and for a
        // future that is ready on the first poll, nothing ever would.
        //
        // A watchdog rather than a bare call, because the failure is a thread
        // parked with nothing in the system able to wake it. Left to itself
        // that is a 120-second harness timeout naming no test; with the
        // watchdog it is an assertion that says what happened.
        static WOKE: AtomicBool = AtomicBool::new(false);
        WOKE.store(false, Ordering::Release);

        extern "C" fn watchdog(sleeper: u64) -> ! {
            let deadline = crate::TICKS.load(Ordering::SeqCst) + 60;
            while crate::TICKS.load(Ordering::SeqCst) < deadline {
                if !crate::sched::is_parked(qunix_sched::ThreadId(sleeper)) {
                    crate::sched::exit_current()
                }
                crate::sched::yield_now();
            }
            WOKE.store(true, Ordering::Release);
            crate::sched::unpark(qunix_sched::ThreadId(sleeper));
            crate::sched::exit_current()
        }

        let me = crate::sched::current_id();
        crate::sched::spawn_kernel(watchdog, me.0, qunix_sched::Priority::Normal);
        assert_eq!(block_on(core::future::ready(7u32)), 7);
        assert!(
            !WOKE.load(Ordering::Acquire),
            "block_on parked on a future that was already ready; only the watchdog released it"
        );
    }

    #[test_case]
    fn a_waker_round_trips_the_thread_id_it_was_built_from() {
        // The waker's data word *is* the id. Getting the cast wrong is silent:
        // `unpark` would take an id naming nothing, find no thread, and return,
        // so every await would hang with no message.
        // A synthetic, deliberately non-zero id. Asserting against
        // `current_id()` alone is vacuous on the thread that runs the suite:
        // the boot thread's id is 0, so a waker whose data word was lost to a
        // null pointer compares *equal* to it and every mutation of `clone`
        // passes. Found by mutation, not by review.
        let synthetic = qunix_sched::ThreadId(0x1234_5678_9abc);
        let waker = waker_for(synthetic);
        assert_eq!(raw_id_of(&waker), synthetic.0, "the waker does not name its own thread");
        // Cloning must not change the id, and must not allocate. The identity
        // clone is what lets an interrupt handler clone a waker at all.
        let clone = waker.clone();
        assert_eq!(raw_id_of(&clone), synthetic.0, "cloning a waker changed the thread it names");
        // And the real thing, so the accessor is not merely self-consistent.
        let id = crate::sched::current_id();
        assert_eq!(raw_id_of(&waker_for(id)), id.0, "a live thread's waker does not name it");
    }

    #[test_case]
    fn a_pending_future_resumes_when_another_thread_wakes_it() {
        // End to end: poll returns Pending, the thread parks, a *different*
        // thread completes it through the waker the future published, and
        // `block_on` polls again and returns. This is the whole runtime.
        FLAG_READY.store(false, Ordering::Release);
        FLAG_WAKER.store(u64::MAX, Ordering::Release);

        static RESCUED: AtomicBool = AtomicBool::new(false);
        RESCUED.store(false, Ordering::Release);

        /// Completes the future, or rescues the sleeper and says so.
        ///
        /// `arg` is the sleeper's real id, captured by the spawner. The rescue
        /// is what turns every failure of this test into a named assertion:
        /// without it, a waker naming the wrong thread parks the sleeper
        /// forever, and the harness reports a 120-second timeout that names no
        /// test and gives no reason.
        extern "C" fn completer(sleeper: u64) -> ! {
            let deadline = crate::TICKS.load(Ordering::SeqCst) + 60;
            loop {
                let raw = FLAG_WAKER.load(Ordering::Acquire);
                // Waits until the sleeper has actually parked. Setting the flag
                // earlier would exercise the pending-wake path instead, which
                // is a different test.
                if raw != u64::MAX && crate::sched::is_parked(qunix_sched::ThreadId(raw)) {
                    FLAG_READY.store(true, Ordering::Release);
                    // Woken through a `Waker` built from the published id, not
                    // through `sched::unpark` directly, so the vtable's `wake`
                    // arm is on the path under test.
                    waker_for(qunix_sched::ThreadId(raw)).wake();
                    break;
                }
                if crate::TICKS.load(Ordering::SeqCst) >= deadline {
                    RESCUED.store(true, Ordering::Release);
                    FLAG_READY.store(true, Ordering::Release);
                    // The *real* id, not the published one, which is exactly
                    // what may be wrong.
                    crate::sched::unpark(qunix_sched::ThreadId(sleeper));
                    break;
                }
                crate::sched::yield_now();
            }
            crate::sched::exit_current()
        }

        let me = crate::sched::current_id();
        crate::sched::spawn_kernel(completer, me.0, qunix_sched::Priority::Normal);
        let polls = block_on(Flag { polls: 0 });
        assert!(
            !RESCUED.load(Ordering::Acquire),
            "the future never completed on its own: either it was never polled, or the waker \
             it published did not name the thread that was parked"
        );
        assert!(polls >= 2, "the future completed without ever having been pending");
        assert_eq!(
            FLAG_WAKER.load(Ordering::Acquire),
            me.0,
            "the future was polled with a waker naming some other thread"
        );
    }

    #[test_case]
    fn waking_through_a_stale_waker_does_not_disturb_a_later_thread() {
        // The reason there is no generation counter. A waker kept past its
        // thread's death must be inert -- if ids were reused, this would wake
        // whichever thread inherited the number, at a suspension point it never
        // reached.
        static DEAD: AtomicU64 = AtomicU64::new(u64::MAX);

        extern "C" fn brief(_: u64) -> ! {
            DEAD.store(crate::sched::current_id().0, Ordering::Release);
            crate::sched::exit_current()
        }

        let id = crate::sched::spawn_kernel(brief, 0, qunix_sched::Priority::Normal);
        assert!(
            crate::tests::wait_until(|| !crate::sched::thread_id_is_live(id), crate::tests::WAIT_BUDGET),
            "{id:?} never exited"
        );
        assert_eq!(DEAD.load(Ordering::Acquire), id.0);

        let stale = waker_for(id);
        let before = crate::sched::thread_count();
        stale.wake();
        assert_eq!(
            crate::sched::thread_count(),
            before,
            "waking a dead thread's waker changed the thread table"
        );
    }
}
