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

pub mod park;
pub use park::{ParkOutcome, ParkState, WakeOutcome};

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

    /// Ids at or above this were pushed into the idle band by the tests below.
    ///
    /// A stolen id can be checked against it without the queue having to tell
    /// us which band it came from -- `steal` returns a bare id, and the
    /// property under test is precisely that the band it came from was not
    /// the idle one.
    const IDLE_ID_BASE: u64 = 100;

    /// The queue's full contents in the order `pop` would serve them.
    ///
    /// Used where the assertion is "nothing moved": comparing `len` alone
    /// cannot distinguish a no-op from a reorder.
    fn drain(q: &mut RunQueue) -> alloc::vec::Vec<ThreadId> {
        let mut out = alloc::vec::Vec::new();
        while let Some(id) = q.pop() {
            out.push(id);
        }
        out
    }

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
        // Which band it kept is the load-bearing half, and `len` alone cannot
        // see it: a `push` implemented as "remove, then enqueue at the new
        // priority" would also report 1 here while having silently promoted a
        // queued thread. Promotion out of the idle band is the dangerous
        // direction -- `runnable_len` would then count an idle thread and the
        // kernel's steal guard would migrate it onto another CPU's stack.
        assert_eq!(q.runnable_len(), 1);
        q.push(B, Priority::Idle);
        q.push(B, Priority::High);
        assert_eq!(
            q.runnable_len(),
            1,
            "a second push promoted a queued idle thread into a stealable band"
        );
        assert_eq!(q.pop(), Some(A), "the first push's band did not win");
    }

    #[test]
    fn pushing_a_queued_thread_again_does_not_move_it_to_the_back() {
        // The duplicate check must be a no-op, not a re-enqueue. A re-enqueue
        // keeps `len` at 1 per thread -- so the existing duplicate tests stay
        // green -- while resetting the pusher's place in the FIFO, which is
        // exactly the starvation the band's queue discipline exists to bound:
        // a thread woken repeatedly would keep jumping the line.
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(B, Priority::Normal);
        q.push(C, Priority::Normal);
        q.push(A, Priority::Normal);
        assert_eq!(q.len(), 3);
        assert_eq!(q.pop(), Some(A), "the re-pushed thread lost its place in line");
        assert_eq!(q.pop(), Some(B));
        assert_eq!(q.pop(), Some(C));
    }

    #[test]
    fn a_refused_push_leaves_the_queue_byte_for_byte_unchanged() {
        // "It returned without duplicating" is weaker than "it changed
        // nothing". Compare the whole observable state, not just `len`.
        let mut q = RunQueue::new();
        q.push(A, Priority::High);
        q.push(B, Priority::Normal);
        q.push(C, Priority::Idle);
        let before = drain(&mut q);

        let mut after_refusal = RunQueue::new();
        after_refusal.push(A, Priority::High);
        after_refusal.push(B, Priority::Normal);
        after_refusal.push(C, Priority::Idle);
        // Every one of these is a duplicate, at its own band and at others'.
        for prio in Priority::ORDER {
            after_refusal.push(A, prio);
            after_refusal.push(B, prio);
            after_refusal.push(C, prio);
        }
        assert_eq!(after_refusal.len(), 3);
        assert_eq!(after_refusal.runnable_len(), 2);
        assert_eq!(drain(&mut after_refusal), before, "a refused push still moved something");
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
    fn steal_prefers_the_normal_band_over_the_idle_one() {
        // `steal_prefers_the_highest_band_like_pop` only ever compares Idle
        // against High, so a `steal` that scanned Normal *after* Idle would
        // pass it. Normal-versus-Idle is the pairing the kernel actually hits:
        // High is reserved and most work is Normal.
        let mut q = RunQueue::new();
        q.push(ThreadId(IDLE_ID_BASE), Priority::Idle);
        q.push(A, Priority::Normal);
        assert_eq!(q.steal(), Some(A), "an idle thread was stolen over normal work");
    }

    #[test]
    fn steal_never_returns_an_idle_thread_while_runnable_len_is_nonzero() {
        // This is the exact contract `kernel/src/sched.rs::take_next` relies
        // on. It does not ask `steal` to refuse idle threads; it checks
        // `runnable_len() == 0` and skips the queue, then trusts that a
        // non-zero count means whatever `steal` hands back came from a
        // non-idle band. Stealing an idle thread runs a CPU on the stack
        // another CPU booted on, so the two halves must compose for *every*
        // queue shape, not just the one the other steal tests happen to build.
        //
        // Every arrangement of up to three real threads over the two runnable
        // bands, each with idle threads queued before, between and after.
        for shape in 0..27u32 {
            let mut q = RunQueue::new();
            let mut real = 0;
            for slot in 0..3 {
                q.push(ThreadId(IDLE_ID_BASE + slot), Priority::Idle);
                match (shape / 3u32.pow(slot as u32)) % 3 {
                    0 => {}
                    1 => {
                        q.push(ThreadId(slot + 1), Priority::Normal);
                        real += 1;
                    }
                    _ => {
                        q.push(ThreadId(slot + 1), Priority::High);
                        real += 1;
                    }
                }
            }
            assert_eq!(q.runnable_len(), real, "runnable_len miscounted shape {shape}");
            // Drain every runnable thread the way a thief would, checking the
            // guard before each attempt exactly as `take_next` does.
            for _ in 0..real {
                assert_ne!(q.runnable_len(), 0);
                let stolen = q.steal().expect("runnable_len was non-zero but steal found nothing");
                assert!(
                    stolen.0 < IDLE_ID_BASE,
                    "shape {shape}: stole idle thread {stolen:?} while runnable_len was non-zero"
                );
            }
            assert_eq!(q.runnable_len(), 0);
            // The idle threads are all still here: nothing above took one.
            assert_eq!(q.len(), 3, "shape {shape}: an idle thread went missing");
        }
    }

    #[test]
    fn steal_hands_back_the_idle_thread_when_that_is_all_there_is() {
        // Deliberately asserting the *absence* of a guard. `steal` does not
        // refuse idle threads and must not start doing so silently: the
        // refusal lives in the caller's `runnable_len() == 0` check, and a
        // guard added here would make that check look redundant and invite its
        // removal. If this ever needs changing, change `take_next` first.
        let mut q = RunQueue::new();
        q.push(ThreadId(IDLE_ID_BASE), Priority::Idle);
        assert_eq!(q.runnable_len(), 0, "the caller's guard would not have fired");
        assert_eq!(q.steal(), Some(ThreadId(IDLE_ID_BASE)));
    }

    #[test]
    fn steal_does_not_disturb_the_order_of_what_it_leaves_behind() {
        // The owner pops from the front while a thief takes the back. If a
        // steal reordered the remainder, the owner's FIFO guarantee would hold
        // only on queues no one ever stole from.
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        q.push(B, Priority::Normal);
        q.push(C, Priority::Normal);
        assert_eq!(q.steal(), Some(C));
        assert_eq!(drain(&mut q), alloc::vec![A, B], "the steal reordered the survivors");
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
    fn a_failed_remove_leaves_every_band_in_the_same_order() {
        // `len` survives a reorder, and `remove` walks all three bands looking
        // for its target -- a scan that mutated as it went (rotating a band to
        // search it, say) would leave the count right and the order wrong.
        let mut q = RunQueue::new();
        for (id, prio) in
            [(A, Priority::High), (B, Priority::High), (C, Priority::Normal), (ThreadId(IDLE_ID_BASE), Priority::Idle)]
        {
            q.push(id, prio);
        }
        let mut untouched = RunQueue::new();
        for (id, prio) in
            [(A, Priority::High), (B, Priority::High), (C, Priority::Normal), (ThreadId(IDLE_ID_BASE), Priority::Idle)]
        {
            untouched.push(id, prio);
        }
        assert!(!q.remove(ThreadId(999)), "removed a thread that was never queued");
        assert_eq!(drain(&mut q), drain(&mut untouched), "a failed remove reordered the queue");
    }

    #[test]
    fn remove_takes_out_one_thread_and_only_that_thread() {
        // Removing the target must not take its neighbours with it: the caller
        // is descheduling one thread that blocked or exited, and a queue that
        // dropped a bystander loses it permanently -- nothing ever re-queues a
        // thread the scheduler has already forgotten.
        let mut q = RunQueue::new();
        q.push(A, Priority::High);
        q.push(B, Priority::Normal);
        q.push(C, Priority::Normal);
        q.push(ThreadId(IDLE_ID_BASE), Priority::Idle);
        assert!(q.remove(B));
        assert!(!q.contains(B));
        assert!(q.contains(A) && q.contains(C) && q.contains(ThreadId(IDLE_ID_BASE)));
        assert_eq!(drain(&mut q), alloc::vec![A, C, ThreadId(IDLE_ID_BASE)]);
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
    fn a_lower_band_never_runs_while_a_higher_one_is_non_empty() {
        // The existing ordering tests each hold one thread per band, so a
        // `pop` that served one thread from the top band and then fell through
        // would pass them. Assert the negative directly: at every step, if a
        // higher band still holds anything, what came out was not from a lower
        // one.
        let mut q = RunQueue::new();
        let high = [ThreadId(10), ThreadId(11)];
        let normal = [ThreadId(20), ThreadId(21)];
        let idle = [ThreadId(IDLE_ID_BASE), ThreadId(IDLE_ID_BASE + 1)];
        // Interleaved arrival, so arrival order and band order disagree.
        q.push(idle[0], Priority::Idle);
        q.push(normal[0], Priority::Normal);
        q.push(high[0], Priority::High);
        q.push(idle[1], Priority::Idle);
        q.push(normal[1], Priority::Normal);
        q.push(high[1], Priority::High);

        let mut high_left = 2;
        let mut normal_left = 2;
        while let Some(id) = q.pop() {
            if high.contains(&id) {
                high_left -= 1;
            } else if normal.contains(&id) {
                assert_eq!(high_left, 0, "{id:?} ran while {high_left} High threads waited");
                normal_left -= 1;
            } else {
                assert!(idle.contains(&id));
                assert_eq!(high_left, 0, "the idle band ran while High work waited");
                assert_eq!(normal_left, 0, "the idle band ran while Normal work waited");
            }
        }
        assert_eq!((high_left, normal_left), (0, 0));
    }

    #[test]
    fn runnable_len_counts_the_high_band_too() {
        // `runnable_len` sums two named bands. Written with `Normal` twice --
        // an easy copy-paste -- it would still report 0 for an idle-only queue
        // and 1 for one Normal thread, so the existing test cannot see it,
        // while a CPU holding only High work would advertise nothing to steal
        // and that work would sit unrun until its owner got round to it.
        let mut q = RunQueue::new();
        q.push(A, Priority::High);
        assert_eq!(q.runnable_len(), 1, "a High thread is not counted as runnable work");
        q.push(B, Priority::Normal);
        q.push(ThreadId(IDLE_ID_BASE), Priority::Idle);
        assert_eq!(q.runnable_len(), 2);
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn runnable_len_falls_when_a_runnable_thread_is_removed_but_not_for_an_idle_one() {
        let mut q = RunQueue::new();
        q.push(A, Priority::High);
        q.push(B, Priority::Normal);
        q.push(ThreadId(IDLE_ID_BASE), Priority::Idle);
        assert!(q.remove(ThreadId(IDLE_ID_BASE)));
        assert_eq!(q.runnable_len(), 2, "removing an idle thread changed the runnable count");
        assert!(q.remove(A));
        assert_eq!(q.runnable_len(), 1);
        assert!(q.remove(B));
        assert_eq!(q.runnable_len(), 0);
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

    #[test]
    fn contains_is_false_after_a_steal() {
        // `push` refuses duplicates by asking `contains`, so a `contains` that
        // still reported a stolen thread would refuse to re-queue it on the
        // CPU that stole it -- the thread would be lost, not merely delayed.
        let mut q = RunQueue::new();
        q.push(A, Priority::Normal);
        assert!(q.steal().is_some());
        assert!(!q.contains(A), "a stolen thread is still reported as queued");
        q.push(A, Priority::Normal);
        assert_eq!(q.pop(), Some(A), "a stolen thread could not be queued again");
    }
}
