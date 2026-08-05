#![cfg_attr(not(any(test, feature = "std")), no_std)]

//! Scheduling policy: run queues and the choice of what runs next.
//!
//! Deliberately knows nothing about CPUs, stacks, page tables or context
//! switching. A thread is an opaque [`ThreadId`]; whether that id names
//! something with a stack is the kernel's problem. That separation is what
//! lets the policy be tested on the host, where a wrong answer is a failed
//! assertion rather than a machine that stops responding.
//!
//! One run queue belongs to one CPU. It is not internally synchronised — the
//! kernel wraps it in a lock, which keeps the decision about *which* lock and
//! how long it is held where the kernel can see it.

extern crate alloc;

use alloc::collections::VecDeque;

/// Opaque handle to something schedulable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u64);

/// Scheduling band.
///
/// Three bands, not a numeric nice value: strict priority with a handful of
/// bands is enough to express "the idle thread must never preempt real work"
/// and "the timer tick must not be starved", which is all M1 needs. A weighted
/// scheme without a workload to tune it against would be guesswork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Runs only when nothing else can. There is one idle thread per CPU and it
    /// must never be chosen over runnable work.
    Idle = 0,
    Normal = 1,
    High = 2,
}

impl Priority {
    /// Highest first, so `pop` scans in the order it wants to serve.
    const ORDER: [Priority; 3] = [Priority::High, Priority::Normal, Priority::Idle];

    fn band(self) -> usize {
        self as usize
    }
}

/// One CPU's queue of runnable threads.
///
/// Strict priority between bands, FIFO within a band. FIFO matters: it is what
/// bounds how long a thread waits behind its equals, and a stack would let a
/// steady arrival rate starve whatever is at the bottom indefinitely.
#[derive(Debug, Default)]
pub struct RunQueue {
    /// Indexed by `Priority as usize`.
    bands: [VecDeque<ThreadId>; 3],
}

impl RunQueue {
    pub const fn new() -> Self {
        Self { bands: [VecDeque::new(), VecDeque::new(), VecDeque::new()] }
    }

    /// Makes `id` runnable at `prio`.
    ///
    /// A thread already queued is *not* enqueued twice: a double push would let
    /// `pop` hand the same thread to two CPUs, which is the scheduler's version
    /// of handing out the same memory twice. The check is a scan, which is
    /// acceptable while run queues are short and is the reason `remove` exists
    /// rather than a lazy tombstone scheme.
    pub fn push(&mut self, id: ThreadId, prio: Priority) {
        if self.contains(id) {
            return;
        }
        self.bands[prio.band()].push_back(id);
    }

    /// The next thread to run: front of the highest non-empty band.
    pub fn pop(&mut self) -> Option<ThreadId> {
        for prio in Priority::ORDER {
            if let Some(id) = self.bands[prio.band()].pop_front() {
                return Some(id);
            }
        }
        None
    }

    /// Takes work for another CPU: the *back* of the highest non-empty band.
    ///
    /// The back, not the front, for two reasons. The front is what this CPU is
    /// about to run, so taking it maximises the chance of stealing a thread
    /// whose data is still warm in this CPU's cache. And taking from the
    /// opposite end means a thief and the owner contend only when the band has
    /// one element left.
    ///
    /// Highest band first, matching `pop`: stealing low-priority work while
    /// high-priority work waits would invert the policy the bands express.
    pub fn steal(&mut self) -> Option<ThreadId> {
        for prio in Priority::ORDER {
            if let Some(id) = self.bands[prio.band()].pop_back() {
                return Some(id);
            }
        }
        None
    }

    /// Removes `id` wherever it is queued. `true` if it was there.
    ///
    /// Needed because a thread can stop being runnable while queued — it
    /// blocks, or it exits — and leaving a stale id for `pop` to return means
    /// dispatching to a thread that no longer exists.
    pub fn remove(&mut self, id: ThreadId) -> bool {
        for band in &mut self.bands {
            if let Some(pos) = band.iter().position(|&queued| queued == id) {
                band.remove(pos);
                return true;
            }
        }
        false
    }

    pub fn contains(&self, id: ThreadId) -> bool {
        self.bands.iter().any(|band| band.contains(&id))
    }

    pub fn len(&self) -> usize {
        self.bands.iter().map(VecDeque::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Runnable threads excluding the idle band.
    ///
    /// What "is there work to do" actually means: the idle thread is always
    /// queued, so `is_empty` is false even on a completely idle CPU, and a
    /// steal decision based on `len` would move idle threads between CPUs.
    pub fn runnable_len(&self) -> usize {
        self.bands[Priority::High.band()].len() + self.bands[Priority::Normal.band()].len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ThreadId = ThreadId(1);
    const B: ThreadId = ThreadId(2);
    const C: ThreadId = ThreadId(3);

    #[test]
    fn a_new_queue_is_empty() {
        let q = RunQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.runnable_len(), 0);
    }

    #[test]
    fn pop_returns_none_when_empty() {
        assert_eq!(RunQueue::new().pop(), None);
    }

    #[test]
    fn steal_returns_none_when_empty() {
        assert_eq!(RunQueue::new().steal(), None);
    }

    #[test]
    fn higher_priority_runs_first_regardless_of_arrival_order() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Idle);
        q.push(B, Priority::Normal);
        q.push(C, Priority::High);
        assert_eq!(q.pop(), Some(C));
        assert_eq!(q.pop(), Some(B));
        assert_eq!(q.pop(), Some(A));
    }

    #[test]
    fn a_band_is_fifo_so_equals_cannot_starve_each_other() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(B, Priority::Normal);
        q.push(C, Priority::Normal);
        assert_eq!(q.pop(), Some(A));
        assert_eq!(q.pop(), Some(B));
        assert_eq!(q.pop(), Some(C));
    }

    #[test]
    fn the_idle_band_never_runs_while_real_work_is_queued() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Idle);
        q.push(B, Priority::Normal);
        // The negative direction: the idle thread must be refused, not merely
        // deprioritised. A queue that returned it here would let a CPU idle
        // with work pending. One `pop`, checked twice -- `assert_ne!` evaluates
        // its argument, so popping inside both assertions consumes two threads.
        let first = q.pop();
        assert_ne!(first, Some(A), "the idle thread ran while normal work was queued");
        assert_eq!(first, Some(B));
        assert_eq!(q.pop(), Some(A), "the idle thread never ran");
    }

    #[test]
    fn pushing_a_queued_thread_twice_does_not_queue_it_twice() {
        // A double push lets `pop` hand one thread to two CPUs.
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(A, Priority::Normal);
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop(), Some(A));
        assert_eq!(q.pop(), None, "the same thread was queued twice");
    }

    #[test]
    fn pushing_a_queued_thread_at_a_new_priority_does_not_duplicate_it() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(A, Priority::High);
        assert_eq!(q.len(), 1, "the thread now exists in two bands");
    }

    #[test]
    fn steal_takes_from_the_back_so_the_owner_keeps_its_next_thread() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(B, Priority::Normal);
        assert_eq!(q.steal(), Some(B), "steal took the thread the owner runs next");
        assert_eq!(q.pop(), Some(A));
    }

    #[test]
    fn steal_prefers_the_highest_band_like_pop() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Idle);
        q.push(B, Priority::High);
        // Stealing low-priority work while high-priority work waits inverts the
        // policy the bands exist to express.
        assert_eq!(q.steal(), Some(B));
    }

    #[test]
    fn steal_of_a_single_element_band_empties_it() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        assert_eq!(q.steal(), Some(A));
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn remove_takes_a_queued_thread_out_of_the_middle() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(B, Priority::Normal);
        q.push(C, Priority::Normal);
        assert!(q.remove(B));
        assert_eq!(q.pop(), Some(A));
        assert_eq!(q.pop(), Some(C), "removing B disturbed the order of its neighbours");
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn remove_reports_false_for_a_thread_that_was_never_queued() {
        // The negative direction: a `true` here would let the caller believe it
        // had descheduled a thread that is still going to run.
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        assert!(!q.remove(B));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn remove_finds_a_thread_in_any_band() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Idle);
        q.push(B, Priority::High);
        assert!(q.remove(A));
        assert!(q.remove(B));
        assert!(q.is_empty());
    }

    #[test]
    fn a_removed_thread_can_be_queued_again() {
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        assert!(q.remove(A));
        q.push(A, Priority::Normal);
        assert_eq!(q.pop(), Some(A), "the duplicate check outlived the removal");
    }

    #[test]
    fn runnable_len_ignores_the_idle_band() {
        // A CPU with only its idle thread has nothing worth stealing; counting
        // it would migrate idle threads between CPUs forever.
        let mut q = RunQueue::new();
        q.push(A, Priority::Idle);
        assert_eq!(q.len(), 1);
        assert_eq!(q.runnable_len(), 0);
        q.push(B, Priority::Normal);
        assert_eq!(q.runnable_len(), 1);
    }

    #[test]
    fn contains_tracks_membership_across_pop_and_push() {
        let mut q = RunQueue::new();
        assert!(!q.contains(A));
        q.push(A, Priority::Normal);
        assert!(q.contains(A));
        q.pop();
        assert!(!q.contains(A), "a popped thread is still reported as queued");
    }
}
