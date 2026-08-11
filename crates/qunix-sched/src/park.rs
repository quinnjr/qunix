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

/// A thread's blocking status, owned by its `Thread` and mutated only under the
/// scheduler lock.
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

#[cfg(test)]
mod tests {
    use super::*;

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
