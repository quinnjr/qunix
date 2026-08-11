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

/// How many sleeps can be outstanding at once.
///
/// A fixed table rather than a list, because the timer interrupt walks it:
/// allocating in `expire_timers` would put the heap allocator on the interrupt
/// path, and a linked list would need a node from somewhere.
const CAPACITY: usize = 64;

/// One outstanding sleep.
///
/// `deadline` is an absolute tick count, not a remaining count. A remaining
/// count would have to be decremented by the handler for every entry on every
/// tick, and a tick missed while interrupts were masked -- which happens on
/// every context switch -- would silently lengthen every sleep in the table.
#[derive(Clone, Copy)]
struct Timer {
    deadline: u64,
    thread: ThreadId,
}

/// `IrqSpinLock`, not `SpinLock`: this is taken from the timer interrupt, so a
/// plain spinlock would let a tick land on a processor that already holds it
/// and spin against itself forever.
static TIMERS: qunix_sync::IrqSpinLock<[Option<Timer>; CAPACITY], qunix_hal_x86_64::Irq> =
    qunix_sync::IrqSpinLock::new([None; CAPACITY]);

/// Why a sleep could not be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepError {
    /// Every timer slot is occupied. Refused rather than dropped: a sleep that
    /// silently failed to register is `Pending` forever with nothing scheduled
    /// to wake it, which is a hung thread rather than a slow one.
    Full,
}

/// Wakes every thread whose deadline has passed. Called from the timer handler.
///
/// Takes `now` rather than reading `TICKS` itself, so the handler's increment
/// and this comparison cannot disagree about which tick this is.
pub fn expire_timers(now: u64) {
    let mut woken: [Option<ThreadId>; CAPACITY] = [None; CAPACITY];
    let mut count = 0;
    {
        let mut timers = TIMERS.lock();
        for slot in timers.iter_mut() {
            // `>=`, not `==`. A tick can be missed -- interrupts are masked
            // across every context switch -- and an equality test would step
            // straight over the deadline and leave the thread parked forever.
            if slot.is_some_and(|t| now >= t.deadline) {
                woken[count] = slot.take().map(|t| t.thread);
                count += 1;
            }
        }
    }
    // Unparked after the timer lock is released. `unpark` takes the scheduler
    // lock and may take a run queue; holding TIMERS across that would order
    // three locks in a way nothing else in the kernel does, from an interrupt.
    for id in woken.iter().flatten() {
        crate::sched::unpark(*id);
    }
}

/// A future that completes once `deadline` has passed.
struct Sleep {
    deadline: u64,
    /// Whether this sleeper holds a slot in [`TIMERS`].
    ///
    /// A deadline already in the past takes no slot, so this is not simply
    /// "always true"; it is also what tells `drop` whether there is anything to
    /// release.
    registered: bool,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: core::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if crate::TICKS.load(core::sync::atomic::Ordering::SeqCst) >= self.deadline {
            return Poll::Ready(());
        }
        // Nothing is registered here. The slot was taken by `try_sleep_ticks`,
        // before this future existed, which is why that is the fallible entry
        // point and `poll` is infallible. Registering on first poll instead
        // would mean a `poll` that can fail with no way to report it, and a
        // re-poll -- which a spurious wake makes routine -- would take a second
        // slot for one sleeper and leak the first.
        //
        // The waker is deliberately unused: `expire_timers` unparks the thread
        // by id, which is the same thing this future's waker would do, and
        // storing a waker per timer would put a `Waker` clone on the interrupt
        // path for no gain. That is only correct because the thread *is* the
        // task -- see the module docs.
        Poll::Pending
    }
}

impl Drop for Sleep {
    /// Releases the slot if the sleeper is dropped before it fires.
    ///
    /// Reachable whenever an `await` is abandoned -- a cancelled read, a
    /// `select` another arm won. Without this the slot is held by a thread that
    /// is no longer waiting, and after `CAPACITY` cancellations every sleep in
    /// the kernel is refused.
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let me = crate::sched::current_id();
        let mut timers = TIMERS.lock();
        for slot in timers.iter_mut() {
            if slot.is_some_and(|t| t.thread == me && t.deadline == self.deadline) {
                *slot = None;
                return;
            }
        }
    }
}

/// Sleeps for `ticks` timer ticks, or refuses if no timer slot is free.
pub fn try_sleep_ticks(ticks: u64) -> Result<impl Future<Output = ()>, SleepError> {
    let now = crate::TICKS.load(core::sync::atomic::Ordering::SeqCst);
    let deadline = now.saturating_add(ticks);
    // A deadline already reached takes no slot at all. Registering one would
    // park the caller until the next tick for a sleep of zero, and on a
    // processor whose timer is masked that is forever.
    if deadline <= now {
        return Ok(Sleep { deadline, registered: false });
    }
    let me = crate::sched::current_id();
    let mut timers = TIMERS.lock();
    let slot = timers.iter_mut().find(|s| s.is_none()).ok_or(SleepError::Full)?;
    *slot = Some(Timer { deadline, thread: me });
    Ok(Sleep { deadline, registered: true })
}

/// Sleeps for `ticks` timer ticks.
///
/// Panics if the timer table is full. Callers that can do something better than
/// die should use [`try_sleep_ticks`]; this exists because most cannot, and a
/// silent non-sleep is worse than a named panic.
pub fn sleep_ticks(ticks: u64) -> impl Future<Output = ()> {
    match try_sleep_ticks(ticks) {
        Ok(sleep) => sleep,
        Err(SleepError::Full) => panic!("no free timer slot for a {ticks}-tick sleep"),
    }
}

/// Occupies every free timer slot. Returns how many it newly took.
///
/// The entries name `NO_THREAD`, an id `allocate_id` can never issue, so one
/// expired by mistake is ignored by `unpark` rather than waking a real thread.
#[cfg(test)]
pub fn fill_timer_table_for_test() -> usize {
    let mut timers = TIMERS.lock();
    let mut taken = 0;
    for slot in timers.iter_mut().filter(|s| s.is_none()) {
        *slot = Some(Timer {
            deadline: u64::MAX,
            thread: ThreadId(qunix_hal_x86_64::percpu::NO_THREAD),
        });
        taken += 1;
    }
    taken
}

/// Registers a timer with an arbitrary deadline, naming no live thread.
///
/// Exists so `expire_timers` can be tested directly. Every other route into the
/// table goes through `try_sleep_ticks`, which refuses a deadline that has
/// already passed -- so the case the `>=` comparison exists for is unreachable
/// from the public API, and mutating it to `==` failed no test.
#[cfg(test)]
pub fn insert_timer_for_test(deadline: u64) {
    let mut timers = TIMERS.lock();
    let slot = timers.iter_mut().find(|s| s.is_none()).expect("the timer table was full");
    *slot = Some(Timer { deadline, thread: ThreadId(qunix_hal_x86_64::percpu::NO_THREAD) });
}

/// Empties the timer table.
#[cfg(test)]
pub fn clear_timer_table_for_test() {
    TIMERS.lock()[..].fill(None);
}

#[cfg(test)]
pub fn free_timer_slots_for_test() -> usize {
    TIMERS.lock().iter().filter(|s| s.is_none()).count()
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
    fn a_sleep_is_woken_by_the_timer_interrupt() {
        // The milestone's sharpest risk, tested directly: nothing but the timer
        // handler makes this thread runnable again. If a `Waker` cannot be
        // driven from interrupt context, this is where it shows.
        let start = crate::TICKS.load(Ordering::SeqCst);
        block_on(sleep_ticks(3));
        let elapsed = crate::TICKS.load(Ordering::SeqCst) - start;
        assert!(elapsed >= 3, "slept {elapsed} ticks, asked for 3");
        // Bounded above as well. A sleep that returned only when something
        // *else* happened to wake the thread would satisfy the lower bound on a
        // busy machine and say nothing about the timer path.
        assert!(elapsed < 100, "slept {elapsed} ticks for a 3-tick sleep");
    }

    #[test_case]
    fn a_sleep_of_zero_ticks_does_not_park() {
        // A deadline already in the past must be `Ready` on the first poll. If
        // it registered instead, the thread would park until the *next* tick
        // for a sleep it was told took no time -- and on a processor whose
        // timer is masked, forever.
        let free_before = free_timer_slots_for_test();
        let start = crate::TICKS.load(Ordering::SeqCst);
        block_on(sleep_ticks(0));
        assert_eq!(crate::TICKS.load(Ordering::SeqCst) - start, 0, "a zero-tick sleep waited");

        // Asserted while the future is still alive. Checking after it is
        // dropped proves nothing: `Sleep::drop` releases the slot, so a
        // zero-tick sleep that wrongly took one would look identical.
        let sleep = try_sleep_ticks(0).expect("a zero-tick sleep was refused");
        assert_eq!(
            free_timer_slots_for_test(),
            free_before,
            "a zero-tick sleep took a timer slot; a deadline already reached needs no timer"
        );
        drop(sleep);
    }

    #[test_case]
    fn a_full_timer_table_refuses_rather_than_returning_a_sleep_that_never_fires() {
        // The negative direction, and the one that matters: a registration that
        // silently failed produces a future that is `Pending` forever with
        // nothing scheduled to wake it. Refusing is the only outcome a caller
        // can act on.
        //
        // The table is filled directly rather than by spawning CAPACITY
        // threads, because the assertion is about the registration path, not
        // about the scheduler's ability to hold that many threads.
        fill_timer_table_for_test();
        assert_eq!(free_timer_slots_for_test(), 0, "the table did not fill");
        assert!(
            matches!(try_sleep_ticks(1), Err(SleepError::Full)),
            "a full timer table accepted another sleeper"
        );
        clear_timer_table_for_test();
        assert!(try_sleep_ticks(1).is_ok(), "the table stayed full after being cleared");
    }

    #[test_case]
    fn an_expired_timer_frees_its_slot() {
        // Otherwise the table fills permanently after CAPACITY sleeps and every
        // later sleep in the kernel is refused -- long after the code that
        // filled it ran.
        let free_before = free_timer_slots_for_test();
        block_on(sleep_ticks(1));
        assert_eq!(
            free_timer_slots_for_test(),
            free_before,
            "a completed sleep left its slot occupied"
        );
    }

    #[test_case]
    fn a_deadline_already_passed_is_expired_rather_than_stepped_over() {
        // `expire_timers` compares `now >= deadline`, and the reason is a tick
        // it never saw: interrupts are masked across every context switch, and
        // `try_sleep_ticks` can be preempted between reading `TICKS` and
        // inserting its slot. An `==` test steps straight over the deadline and
        // leaves the sleeper parked forever.
        //
        // Driven directly rather than through a sleep, because every public
        // route into the table refuses a deadline that has already passed -- so
        // the case the comparison exists for is unreachable from the API, and
        // mutating `>=` to `==` failed no test at all.
        //
        // The entry names `NO_THREAD`, an id `allocate_id` can never issue, so
        // expiring it wakes nothing.
        let free_before = free_timer_slots_for_test();
        let now = crate::TICKS.load(Ordering::SeqCst);
        insert_timer_for_test(now);
        assert_eq!(free_timer_slots_for_test(), free_before - 1, "the timer was not registered");

        // A *later* tick than the deadline, never the exact one.
        expire_timers(now + 5);
        assert_eq!(
            free_timer_slots_for_test(),
            free_before,
            "a deadline that had already passed was stepped over; the sleeper would wait forever"
        );
    }

    #[test_case]
    fn abandoning_a_sleep_before_it_fires_frees_its_slot() {
        // Cancellation is not hypothetical: T4's read futures are dropped
        // whenever a request is abandoned. A slot leaked per cancellation fills
        // the table permanently, and every later sleep is refused.
        let free_before = free_timer_slots_for_test();
        drop(try_sleep_ticks(1_000_000).expect("the timer table was full"));
        assert_eq!(
            free_timer_slots_for_test(),
            free_before,
            "an abandoned sleep left its slot occupied"
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
