//! Whether a thread is parked, and whether a wakeup beat it there.
//!
//! This is three lines of logic guarding the one race in the async runtime
//! that cannot be found by inspection: a completion interrupt that lands
//! between the poll returning `Pending` and the context switch that stops the
//! thread. It lives here, in a host-tested crate with no machine state, so
//! that every interleaving can be written down as a test rather than
//! reproduced in QEMU.
//!
//! The asymmetry is deliberate and is the safety property: a *spurious* wake
//! costs one extra poll, and a *lost* wake is a thread that never runs again.
//! Every ambiguous case therefore resolves towards spurious.

/// A thread's blocking status.
///
/// Not `Sync` and not internally synchronised: the caller must serialise
/// `park` and `wake` against each other. *Which* lock does that is the
/// kernel's business, not this crate's -- `qunix-sched` deliberately knows
/// nothing about CPUs, and a type here should not assert an invariant it has
/// no way to enforce or even observe. The kernel restates the obligation where
/// it can be checked, on `Thread::park`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParkState {
    parked: bool,
    pending: bool,
}

/// What [`ParkState::park`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    /// The thread is parked and must not run until it is woken.
    Parked,
    /// A wakeup had already arrived, so the park did not happen and the caller
    /// must keep running. Consuming that wakeup is what makes this safe to
    /// treat as "poll again".
    Cancelled,
}

/// What [`ParkState::wake`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    /// The thread was parked and is now runnable. **This is the only outcome
    /// that permits the caller to push the thread onto a run queue** -- doing
    /// so for a thread that never parked would queue one that is still
    /// running, and two CPUs would then resume one context.
    Runnable,
    /// The thread was not parked. The wakeup is remembered so the next park
    /// returns [`ParkOutcome::Cancelled`] instead of sleeping through it.
    Noted,
}

impl ParkState {
    /// Parks the thread, unless a wakeup is already waiting.
    pub fn park(&mut self) -> ParkOutcome {
        // Consumed, not merely read. Leaving it set makes every subsequent
        // park return `Cancelled`, which turns `block_on` into a spin loop
        // that produces correct answers while burning a CPU -- a failure no
        // assertion about results would catch.
        if core::mem::take(&mut self.pending) {
            return ParkOutcome::Cancelled;
        }
        self.parked = true;
        ParkOutcome::Parked
    }

    /// Records a wakeup, and reports whether it made the thread runnable.
    pub fn wake(&mut self) -> WakeOutcome {
        if core::mem::take(&mut self.parked) {
            WakeOutcome::Runnable
        } else {
            self.pending = true;
            WakeOutcome::Noted
        }
    }

    pub fn is_parked(&self) -> bool {
        self.parked
    }

    pub fn has_pending(&self) -> bool {
        self.pending
    }
}

/// What a thread that is exiting with nothing to switch to should do.
///
/// Split out of `sched::exit_current` because the decision cannot be reached in
/// the kernel's own test harness: `take_next` falls back to the calling
/// processor's idle thread, which `publish_handoff` returns to its run queue on
/// every switch away, so with more than one processor "nothing runnable at all"
/// effectively never happens under test. The branch was therefore shipped
/// unexercised in both directions -- reverting it to an unconditional panic
/// left the whole suite green.
///
/// Unfalsifiable and untestable are different problems. The state is
/// unreachable; the *rule* is three lines of arithmetic, and moving it here
/// makes both directions assertable on the host, which is the same trade
/// [`ParkState`] makes for the lost-wakeup race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDisposition {
    /// Something is parked, so a wake can still arrive and make work
    /// runnable. The exiting thread must halt and look again rather than
    /// declare the machine finished.
    HaltAndWait,
    /// Nothing is runnable and nothing is blocked, so nothing can ever become
    /// runnable. There is no correct behaviour left.
    NothingLeft,
}

/// Decides what an exiting thread with an empty run queue should do.
///
/// `anything_else_alive` must mean *any other thread exists at all* — parked,
/// or running on another processor — not merely "parked". An exiting thread
/// sees an empty run queue routinely while other processors are busy, because
/// a queue holds only what is waiting, and concluding the machine is finished
/// from that is wrong. Getting this input wrong panics a healthy kernel, which
/// is why the parameter is named for the question rather than for the first
/// answer that seemed to fit it.
pub const fn exit_disposition(anything_else_alive: bool) -> ExitDisposition {
    if anything_else_alive { ExitDisposition::HaltAndWait } else { ExitDisposition::NothingLeft }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_live_thread_means_the_exiting_thread_must_wait() {
        // The direction that was shipped unexercised. Before threads could
        // block, "nothing runnable" meant "nothing left", and exiting on it
        // panicked. A run queue holds only what is *waiting*, so it is empty
        // both when a thread is parked and when another processor is busy
        // running one -- panicking on either kills a healthy kernel.
        assert_eq!(exit_disposition(true), ExitDisposition::HaltAndWait);
    }

    #[test]
    fn nothing_parked_and_nothing_runnable_is_the_end_of_the_machine() {
        // And the other direction, which must stay a panic: with nothing
        // blocked, nothing can ever become runnable again, so halting would
        // wedge the processor in silence instead of saying so.
        assert_eq!(exit_disposition(false), ExitDisposition::NothingLeft);
    }

    #[test]
    fn a_fresh_thread_is_neither_parked_nor_pending() {
        let state = ParkState::default();
        assert!(!state.is_parked());
        assert!(!state.has_pending());
    }

    #[test]
    fn parking_then_waking_makes_the_thread_runnable() {
        let mut state = ParkState::default();
        assert_eq!(state.park(), ParkOutcome::Parked);
        assert!(state.is_parked());
        assert_eq!(state.wake(), WakeOutcome::Runnable);
        assert!(!state.is_parked(), "a woken thread is still parked");
    }

    #[test]
    fn a_wake_that_arrives_before_the_park_cancels_it() {
        // The lost wakeup, and the only reason this type exists. A completion
        // interrupt can land between the poll that returned `Pending` and the
        // switch that stops the thread. If `park` ignored the wake already
        // recorded, the thread would sleep on an event that has happened and
        // nothing would ever wake it again -- a hang whose cause is a window
        // of a few instructions.
        let mut state = ParkState::default();
        assert_eq!(state.wake(), WakeOutcome::Noted);
        assert!(!state.is_parked(), "a wake arriving first parked the thread");
        assert_eq!(state.park(), ParkOutcome::Cancelled, "the pending wake was lost");
    }

    #[test]
    fn a_consumed_pending_wake_does_not_cancel_the_next_park() {
        // The failure that *looks* like it works. If `park` left `pending` set
        // after consuming it, every later park would return `Cancelled`, so
        // `block_on` would poll in a tight loop forever: correct results,
        // burning a CPU, and no assertion about wakeups would notice.
        let mut state = ParkState::default();
        state.wake();
        assert_eq!(state.park(), ParkOutcome::Cancelled);
        assert!(!state.has_pending(), "the pending wake was sticky");
        assert_eq!(state.park(), ParkOutcome::Parked, "the second park was cancelled too");
    }

    #[test]
    fn waking_a_thread_that_is_not_parked_does_not_make_it_runnable_twice() {
        // `Runnable` is what tells the caller to push the thread onto a run
        // queue. Returning it for a thread that was never parked would queue a
        // *running* thread, which is the two-CPUs-on-one-stack failure the
        // whole scheduler is arranged to prevent.
        let mut state = ParkState::default();
        assert_eq!(state.wake(), WakeOutcome::Noted);
        assert_eq!(state.wake(), WakeOutcome::Noted, "a second wake queued the thread");
    }

    #[test]
    fn two_wakes_against_one_park_queue_the_thread_once() {
        // Two devices can complete for one thread. The second wake must not
        // report `Runnable`: the thread is already on a run queue, and pushing
        // it again puts one id in the queue twice, so two CPUs can pop it.
        let mut state = ParkState::default();
        state.park();
        assert_eq!(state.wake(), WakeOutcome::Runnable);
        assert_eq!(state.wake(), WakeOutcome::Noted, "the thread was queued twice");
    }

    #[test]
    fn the_pending_flag_left_by_a_spurious_wake_survives_to_the_next_park() {
        // Deliberately asserting the *unpleasant* half of the contract: a wake
        // with no park outstanding is remembered, so the next `await` in that
        // thread returns immediately with a spurious poll. That is correct --
        // a spurious poll is always allowed, a lost wakeup never is -- and it
        // is written down so nobody "fixes" it into the lost-wakeup direction.
        let mut state = ParkState::default();
        state.wake();
        assert!(state.has_pending());
        assert_eq!(state.park(), ParkOutcome::Cancelled);
    }
}
