# M2 T3 — Async Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A kernel thread can `await` a future that an interrupt handler
completes, and give up its CPU while it waits.

**Architecture:** A thread is the unit that blocks. `block_on` polls a
stack-pinned future with a `Waker` whose data word *is* the `ThreadId`; a
`Pending` poll parks the thread, and an ISR calling `wake()` unparks it. There
is no task table, no boxed future and no allocation anywhere on the wake path.
The park/unpark handshake is a three-state machine lifted into `qunix-sched`
so the lost-wakeup interleavings are host-testable.

**Tech Stack:** `core::future`, `core::task::{RawWaker, RawWakerVTable, Waker}`,
the existing `IrqSpinLock`, per-CPU `RunQueue`, LAPIC timer.

## Global Constraints

Copied from `CLAUDE.md` and the M2 spec. Every task's requirements implicitly
include this section.

- `no_std` in every crate touched here. `xtask` and `fuzz/` are the only
  host-only members and neither is touched.
- No floating point in kernel crates. The target is soft-float.
- Edition 2024 unsafe attributes: `#[unsafe(no_mangle)]`,
  `#[unsafe(link_section = "…")]`, `#[unsafe(naked)]`.
- `static mut` is banned. Use `UnsafeCell` in a `Sync` newtype and name the
  single writer.
- No TODOs, stubs, or "a later task will fix this" comments. If a future change
  is required, make it a compile error.
- Comments explain *why*. When you change behaviour, re-read the comment above
  it — several bugs here survived review because a comment described an earlier
  version of the code.
- Every commit leaves `cargo xtask test` green. Build only through
  `cargo xtask`; a bare `cargo build` fails.
- Host crates test against musl:
  `cargo test -p qunix-sched --features std --target x86_64-unknown-linux-musl`.
- Never widen `TOLERANCE_PP` to make the coverage ratchet pass.
- **No allocation and no `Arc` traffic on the wake path.** Stated by the spec
  and enforced by the design: the waker's data word is a `u64`, so there is
  nothing to allocate, clone or free.

## Design decisions this plan makes

The spec fixed the shape; three details it left open are decided here, with the
reasoning recorded because the reasoning is what the code cannot show.

### The thread is the task; there is no task table

The spec says "a syscall that awaits parks its thread and calls `schedule()`".
Taken literally that removes the need for a task slab entirely: the future is a
local in `block_on`'s frame, pinned to the kernel stack the thread already
owns, and the thing the scheduler resumes is the thread — which it already
knows how to do.

The alternative, a per-CPU slab of `Pin<Box<dyn Future>>` tasks, was rejected
because it buys nothing here and costs the exact thing the spec's enum-dispatch
decision was made to avoid: a heap allocation per spawned operation, in the
subsystem where memory pressure triggers writeback, writeback needs I/O, and
I/O needs the allocation that is already waiting. A task table becomes worth
its cost when the kernel wants *more concurrent operations than threads* —
readahead, background writeback with no thread of its own. That is a T5
question and T5 will be able to answer it against a working runtime.

The consequence to be honest about: concurrency is bounded by thread count, and
`join`ing two futures inside one thread works (they are polled by the same
`block_on`) while running them on two CPUs does not. Nothing in T4 or T5 needs
the latter.

### The waker's data word is the raw `ThreadId`, with no generation counter

`RawWaker` carries one `*const ()`. Every general-purpose executor puts an
`Arc<Task>` there; this one puts `ThreadId.0`, which makes `clone` the identity
function and `drop` a no-op — the two operations an ISR would otherwise perform
on a refcount.

That is only sound because **thread ids are never reused**:
`Scheduler::allocate_id` increments a `u64` and nothing decrements it, so a
waker that outlives its thread names an id that will never belong to anything
else, and `unpark` finds nothing in the table and returns. A generation counter
packed into the high bits would be the usual defence and is unnecessary here —
but it is unnecessary *because of a property of another function*, so Task 1
pins that property with a test and a comment at `allocate_id` naming what
depends on it. The id counter also gains a `checked_add`: a wrapped `next_id`
reintroduces reuse silently, and the first symptom would be a completion waking
an unrelated thread.

### `unpark` takes the scheduler lock rather than a lock-free ready list

The spec specifies "an ISR pushes a task id onto a lock-free per-CPU ready
list; the scheduler drains it", to avoid "a lock that a non-interrupt path
could hold — the self-deadlock `IrqSpinLock` exists to prevent".

This plan takes `SCHED` directly from the wake path instead. The reasoning:

- The hazard named is already answered. `IrqSpinLock` masks interrupts *before*
  acquiring, so no ISR can land on a CPU that holds it, and `preempt()` has
  taken `SCHED` from the timer ISR since M1. A lock-free list would be buying
  latency, not the correctness the sentence describes.
- A bounded lock-free ring has to answer "what happens when it fills", and both
  answers are worse than the lock. Dropping a wake blocks a thread forever —
  silently, and observable only as a hang. Falling back to the lock on overflow
  means shipping the lock anyway, plus a path exercised only under a condition
  no test naturally produces.
- An unbounded list needs a node per pending wake, which means allocating in
  the ISR — forbidden by the spec for stronger reasons than the one this
  mechanism was chosen to serve.

If measurement later shows the acquisition matters, the ring goes *in front of*
this path with the lock as its overflow fallback, which is the only arrangement
that has a correct answer for a full ring. Recorded in Execution Deviations as
D8 when the first task lands, so the divergence is visible from the spec.

## File structure

| File | Responsibility |
| --- | --- |
| `crates/qunix-sched/src/park.rs` (new) | `ParkState`: the park/unpark handshake as a pure state machine. Host-tested. |
| `crates/qunix-sched/src/lib.rs` (modify) | `pub mod park;` and the re-export. |
| `kernel/src/thread.rs` (modify) | `ThreadState::Blocked`; `Thread::park`. |
| `kernel/src/sched.rs` (modify) | `park`, `unpark`, `is_parked`, the `Blocked` arm of `schedule`, the parked-thread guard on the `Ready` arm, `allocate_id`'s overflow guard. |
| `kernel/src/task.rs` (new) | The waker vtable, `block_on`, `sleep_ticks` and its timer wheel. |
| `kernel/src/main.rs` (modify) | `mod task;`, the tick hook in `timer_handler`, and the in-QEMU tests. |

`park.rs` is split out of `qunix-sched/src/lib.rs` rather than appended to it
because the crate's existing content is the run queue and its bands; a state
machine about *one thread's* blocking status is a separate responsibility and
`lib.rs` is already 594 lines.

---

### Task 1: The park/unpark state machine

The whole point of this task is that the interesting behaviour — a wakeup that
arrives before the thread finishes parking — is decided by three lines that can
be tested on the host, exhaustively, without a machine.

**Files:**
- Create: `crates/qunix-sched/src/park.rs`
- Modify: `crates/qunix-sched/src/lib.rs`
- Test: in `crates/qunix-sched/src/park.rs` (`#[cfg(test)] mod tests`, matching
  the crate's existing placement)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct ParkState` — `Default`, `Clone`, `Copy`, `Debug`, `PartialEq`,
    `Eq`
  - `pub enum ParkOutcome { Parked, Cancelled }`
  - `pub enum WakeOutcome { Runnable, Noted }`
  - `ParkState::park(&mut self) -> ParkOutcome`
  - `ParkState::wake(&mut self) -> WakeOutcome`
  - `ParkState::is_parked(&self) -> bool`
  - `ParkState::has_pending(&self) -> bool`
  - Re-exported from the crate root as
    `qunix_sched::{ParkState, ParkOutcome, WakeOutcome}`

- [ ] **Step 1: Write the failing tests**

Create `crates/qunix-sched/src/park.rs` containing *only* the test module for
now, so the first run fails to compile against a type that does not exist yet:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```sh
cargo test -p qunix-sched --features std --target x86_64-unknown-linux-musl
```
Expected: FAIL. `cannot find type ParkState in this scope` — the module is not
declared yet either, so also expect the tests not to be discovered until Step 3
adds `pub mod park;`. Add that declaration now if the run reports zero tests:

```rust
// crates/qunix-sched/src/lib.rs, with the other module declarations
pub mod park;
pub use park::{ParkOutcome, ParkState, WakeOutcome};
```

- [ ] **Step 3: Write the implementation**

At the top of `crates/qunix-sched/src/park.rs`, above the test module:

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```sh
cargo test -p qunix-sched --features std --target x86_64-unknown-linux-musl
```
Expected: PASS, 7 new tests.

- [ ] **Step 5: Falsify each guard**

Not optional, and not a formality — the last two mutation passes in this repo
each found a guard that no test reached. For each mutation below, apply it, run
the command from Step 4, confirm **FAIL**, then restore:

| Mutation | Test that must fail |
| --- | --- |
| `park`: `core::mem::take(&mut self.pending)` → `self.pending` | `a_consumed_pending_wake_does_not_cancel_the_next_park` |
| `park`: drop the `pending` check entirely | `a_wake_that_arrives_before_the_park_cancels_it` |
| `wake`: `core::mem::take(&mut self.parked)` → `self.parked` | `two_wakes_against_one_park_queue_the_thread_once` |
| `wake`: return `Runnable` unconditionally | `waking_a_thread_that_is_not_parked_does_not_make_it_runnable_twice` |

If any mutation leaves the suite green, the test for that row does not exist
yet — write it before moving on.

- [ ] **Step 6: Run the full suite and commit**

```sh
cargo xtask test
```
Expected: `all 56 tests passed` and `coverage: 6 crate(s) at or above their
floor`. Grep the output for `error[` as well as the pass line — the in-QEMU
suite is built separately from the host crates and prints its own pass line
even when a host crate failed to compile.

```sh
git add crates/qunix-sched/src/park.rs crates/qunix-sched/src/lib.rs
git commit -m "feat(sched): the park/unpark handshake, as a host-tested state machine"
```

---

### Task 2: Blocked threads in the scheduler

**Files:**
- Modify: `kernel/src/thread.rs` (the `ThreadState` enum and the `Thread`
  struct)
- Modify: `kernel/src/sched.rs` (`allocate_id`, `schedule`, plus the new
  `park`, `unpark`, `is_parked`)
- Test: `kernel/src/sched.rs` (`#[cfg(test)] mod tests`, `#[test_case]`) and
  `kernel/src/main.rs` (`mod tests`, for anything needing real threads)

**Interfaces:**
- Consumes: `qunix_sched::{ParkOutcome, ParkState, WakeOutcome}` from Task 1.
- Produces:
  - `crate::thread::ThreadState::Blocked`
  - `crate::thread::Thread::park: ParkState`
  - `crate::sched::park()` — parks the calling thread; returns when woken
  - `crate::sched::unpark(id: ThreadId)` — ISR-safe; no-op for an unknown id
  - `crate::sched::is_parked(id: ThreadId) -> bool`

- [ ] **Step 1: Add the state and the field**

In `kernel/src/thread.rs`:

```rust
pub enum ThreadState {
    Ready,
    Running,
    /// Waiting for something to call `sched::unpark`. Distinct from `Ready`
    /// because a blocked thread must be in no run queue: `Ready` means "may be
    /// dispatched", and dispatching a thread waiting on an I/O completion runs
    /// it at a suspension point it has not returned from.
    Blocked,
    Exited,
}
```

and, in `Thread`:

```rust
    /// Whether this thread is parked, and whether a wakeup beat it there.
    ///
    /// Mutated only under the scheduler lock, by `sched::park` and
    /// `sched::unpark`. It is *not* derivable from `state`: the window this
    /// closes is precisely the one in which `state` has not been updated yet.
    pub park: qunix_sched::ParkState,
```

Initialise it as `ParkState::default()` in **both** `new_kernel` and
`adopt_current`. An idle thread that could not record a park would sleep
through its own wakeups the first time anything in the boot path awaits.

- [ ] **Step 2: Write the failing tests**

In `kernel/src/sched.rs`'s test module (host-independent assertions), and in
`kernel/src/main.rs`'s test module (anything needing a real second thread).
Add to `kernel/src/main.rs`'s `mod tests`:

```rust
    #[test_case]
    fn a_parked_thread_is_in_no_run_queue() {
        // The invariant the `Blocked` state exists to create. A parked thread
        // sitting in a run queue is dispatchable, and dispatching it resumes a
        // future at a suspension point it has not returned from.
        //
        // Checked across *every* CPU rather than the local one: the thread is
        // parked here, but nothing stops another CPU's queue holding a stale
        // copy of its id if a path forgot to skip the requeue.
        use qunix_hal_x86_64::percpu::{MAX_CPUS, run_queue_of};
        static PARKED: AtomicU64 = AtomicU64::new(u64::MAX);
        static RELEASE: AtomicBool = AtomicBool::new(false);

        extern "C" fn sleeper(_: u64) -> ! {
            PARKED.store(crate::sched::current_id().0, Ordering::Release);
            crate::sched::park();
            RELEASE.store(true, Ordering::Release);
            crate::sched::exit_current()
        }

        let id = crate::sched::spawn_kernel(sleeper, 0, qunix_sched::Priority::Normal);
        // Wait until it has actually parked, rather than assuming it was
        // scheduled. A test that checks too early passes for the wrong reason.
        while !crate::sched::is_parked(id) {
            crate::sched::yield_now();
        }

        for cpu in 0..MAX_CPUS {
            let Some(queue) = run_queue_of(cpu) else { continue };
            assert!(
                !queue.lock().contains(id),
                "parked {id:?} is queued on cpu {cpu} and can be dispatched"
            );
        }

        crate::sched::unpark(id);
        while !RELEASE.load(Ordering::Acquire) {
            crate::sched::yield_now();
        }
        assert_eq!(PARKED.load(Ordering::Acquire), id.0);
    }

    #[test_case]
    fn unparking_an_id_that_never_existed_is_a_no_op() {
        // A waker outlives the thread it names -- a device can complete an I/O
        // for a thread that has already exited. This must not panic and must
        // not disturb any live thread; the whole no-generation-counter
        // decision rests on it.
        let before = crate::sched::thread_count();
        crate::sched::unpark(qunix_sched::ThreadId(u64::MAX - 1));
        crate::sched::unpark(qunix_sched::ThreadId(0xdead_beef));
        assert_eq!(crate::sched::thread_count(), before);
    }

    #[test_case]
    fn a_wake_that_arrives_before_the_park_does_not_block_the_thread() {
        // The lost wakeup, end to end and on real threads this time. Task 1
        // proves the state machine; this proves the scheduler consults it,
        // which is the half a unit test cannot reach.
        static DONE: AtomicBool = AtomicBool::new(false);

        extern "C" fn racer(_: u64) -> ! {
            // Wake ourselves first, then park. The park must decline.
            crate::sched::unpark(crate::sched::current_id());
            crate::sched::park();
            DONE.store(true, Ordering::Release);
            crate::sched::exit_current()
        }

        let id = crate::sched::spawn_kernel(racer, 0, qunix_sched::Priority::Normal);
        // Bounded: this test hangs the whole suite if the wakeup was lost, and
        // a hang reports nothing. A tick budget turns it into a failure with a
        // name. 200 ticks is ~2 s at the harness's timer rate.
        let deadline = crate::TICKS.load(Ordering::Relaxed) + 200;
        while !DONE.load(Ordering::Acquire) {
            assert!(
                crate::TICKS.load(Ordering::Relaxed) < deadline,
                "{id:?} parked on a wakeup that had already arrived"
            );
            crate::sched::yield_now();
        }
    }
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo xtask test`
Expected: FAIL to compile — `cannot find function park in module crate::sched`.

- [ ] **Step 4: Implement `park`, `unpark` and `is_parked`**

In `kernel/src/sched.rs`:

```rust
/// Blocks the calling thread until something calls [`unpark`] on it.
///
/// Returns when the thread is woken, which may be spuriously: the contract is
/// only that a woken thread eventually runs, so callers must re-check their
/// condition. That is what `block_on`'s loop does.
pub fn park() {
    let me = current_id();
    loop {
        // Masked across the whole decision. The state transition and the
        // switch cannot be split by a tick: between them this CPU's `current`
        // and the thread's park state disagree, and a preemption there would
        // requeue a thread that is on its way to being blocked.
        let irq = qunix_hal_x86_64::Irq::disable_and_save();
        {
            let mut sched = SCHED.lock();
            let Some(thread) = sched.threads.get_mut(&me) else {
                // Not in the table. Only reachable if the caller is not a real
                // thread, which `block_on` prevents; returning is the only
                // thing left that is not a hang.
                qunix_hal_x86_64::Irq::restore(irq);
                return;
            };
            match thread.park.park() {
                // A wakeup was already waiting, so this park is the one that
                // must not happen.
                ParkOutcome::Cancelled => {
                    qunix_hal_x86_64::Irq::restore(irq);
                    return;
                }
                ParkOutcome::Parked => thread.state = ThreadState::Blocked,
            }
        }

        // `schedule` re-checks the park under its *own* acquisition of SCHED,
        // because the lock is released between here and there and a completion
        // in that window clears the park without queueing the thread (it is
        // still current). Switching away on this stale decision is the lost
        // wakeup in its most expensive form.
        schedule(ThreadState::Blocked);

        // Two ways to arrive here: something else ran and switched back, or
        // there was nothing runnable and `schedule` returned without
        // switching. One question answers both.
        if !is_parked(me) {
            qunix_hal_x86_64::Irq::restore(irq);
            return;
        }

        // Still parked, and still running, because nothing else was runnable.
        // Halting is the only correct move: returning would run a blocked
        // thread at a suspension point it has not returned from, and spinning
        // would occupy the CPU the completion interrupt has to be delivered
        // on. `sti; hlt` is one instruction pair for the reason `idle_loop`
        // documents -- `sti` takes effect after the following instruction, so
        // an interrupt that arrived while masked is delivered once `hlt` has
        // been entered, which is what wakes it.
        // SAFETY: no memory is touched and no stack slot is used.
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };
    }
}

/// Makes a parked thread runnable. Safe to call from an interrupt handler.
///
/// Unknown ids are ignored rather than asserted on. A `Waker` outlives the
/// thread it names -- a device completes an I/O for a thread that has already
/// exited -- and ids are never reused, so an id absent from the table can only
/// be a dead thread. Panicking here would turn every late completion into a
/// kernel panic from interrupt context.
pub fn unpark(id: ThreadId) {
    // Whether the thread must be pushed onto a run queue, decided under the
    // lock and acted on after it is released.
    let queue_at: Option<Priority> = {
        let mut sched = SCHED.lock();
        let Some(thread) = sched.threads.get_mut(&id) else {
            return;
        };
        match thread.park.wake() {
            // Not parked. The wakeup is recorded in the park state and the
            // thread's own `park` will consume it; queueing it here would put
            // a running thread in a run queue.
            WakeOutcome::Noted => None,
            WakeOutcome::Runnable => {
                thread.state = ThreadState::Ready;
                let prio = thread.priority;
                // A thread that is still *current* somewhere has not finished
                // switching away, so it needs no queueing -- it will observe
                // the cleared park when `schedule` returns to it. Queueing it
                // is the two-CPUs-one-stack failure: this CPU would push an id
                // another CPU is executing.
                if is_current_anywhere(id) { None } else { Some(prio) }
            }
        }
    };

    // The scheduler lock is released first. A run queue is a leaf lock, and
    // taking one under SCHED would give this path a lock order nothing else
    // in the kernel has -- see the module docs.
    if let Some(prio) = queue_at {
        percpu::run_queue().lock().push(id, prio);
        // Queued locally, then advertised. An idle CPU is halted with no timer
        // and no way to notice a push; this is the same pairing `spawn_kernel`
        // uses, and for the same reason it must follow the push rather than
        // precede it.
        qunix_hal_x86_64::apic::send_ipi_all_excluding_self(crate::WAKE_VECTOR);
    }
}

/// Whether `id` is currently parked.
pub fn is_parked(id: ThreadId) -> bool {
    SCHED.lock().threads.get(&id).is_some_and(|t| t.park.is_parked())
}
```

Add the import at the top of the file:

```rust
use qunix_sched::{ParkOutcome, Priority, ThreadId, WakeOutcome};
```

- [ ] **Step 5: Teach `schedule` about parked threads**

Two changes inside `schedule`'s `SCHED` critical section. Replace the `match
outgoing_state` block with:

```rust
        match outgoing_state {
            ThreadState::Exited => {
                if let Some(t) = sched.threads.get_mut(&current) {
                    t.state = ThreadState::Exited;
                }
                sched.reapable.push(current);
                // An exiting thread is deliberately not handed on: there is
                // nothing to make runnable again.
                HANDOFF_ID[cpu as usize].store(NO_THREAD, Ordering::Relaxed);
            }
            _ => {
                // Whether this thread is parked decides whether it is handed
                // on, and it is asked *here*, under this acquisition of SCHED,
                // rather than trusted from the caller. `park` released the
                // lock between marking the thread and reaching this line, and
                // an `unpark` in that window clears the park without queueing
                // the thread -- it was still current. Handing it on anyway is
                // correct: a spurious requeue costs one poll, and the opposite
                // mistake is a thread that never runs again.
                //
                // Asked for *every* outgoing state, not only `Blocked`. A
                // timer tick arrives with `Ready` and would otherwise requeue
                // a thread that is parked and merely waiting for its own
                // `hlt`, putting a parked thread in a run queue -- the one
                // thing this state exists to prevent, reached through the one
                // path that does not mention it.
                let parked = sched.threads.get(&current).is_some_and(|t| t.park.is_parked());
                let prio = sched.threads.get(&current).map_or(Priority::Normal, |t| t.priority);
                if let Some(t) = sched.threads.get_mut(&current) {
                    t.state = if parked { ThreadState::Blocked } else { ThreadState::Ready };
                }
                if parked {
                    HANDOFF_ID[cpu as usize].store(NO_THREAD, Ordering::Relaxed);
                } else {
                    // Not pushed to the run queue here. See `HANDOFF_ID`.
                    HANDOFF_PRIORITY[cpu as usize].store(prio as u8, Ordering::Relaxed);
                    HANDOFF_ID[cpu as usize].store(current.0, Ordering::Relaxed);
                }
            }
        }
```

- [ ] **Step 6: Guard the id counter**

In `Scheduler::allocate_id`, replace `self.next_id += 1` with:

```rust
    /// Allocates an id that has never been used before.
    ///
    /// **Never reusing an id is load-bearing outside this function.** A
    /// `Waker` carries a bare `ThreadId` as its data word -- no refcount, no
    /// generation -- which is only sound because an id that outlives its
    /// thread can never come to name a different one. `checked_add` rather
    /// than `+=` for the same reason: the kernel builds without overflow
    /// checks in release, so a wrapped counter would silently reintroduce
    /// reuse, and the first symptom would be a completion waking an unrelated
    /// thread.
    fn allocate_id(&mut self) -> ThreadId {
        let id = ThreadId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("thread ids exhausted");
        // `NO_THREAD` is the per-CPU "nothing is running" sentinel, so a
        // thread carrying it would be indistinguishable from an empty slot.
        assert_ne!(id.0, NO_THREAD, "thread id collided with the empty sentinel");
        id
    }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo xtask test`
Expected: `all 59 tests passed`.

- [ ] **Step 8: Falsify each guard**

| Mutation | Test that must fail |
| --- | --- |
| `schedule`: drop the `parked` branch and always hand off | `a_parked_thread_is_in_no_run_queue` |
| `unpark`: queue on `WakeOutcome::Noted` as well | `a_parked_thread_is_in_no_run_queue` |
| `unpark`: drop the `is_current_anywhere` check | the harness's `assert_no_thread_runs_twice` |
| `park`: treat `Cancelled` as `Parked` | `a_wake_that_arrives_before_the_park_does_not_block_the_thread` |
| `park`: `return` instead of `sti; hlt` in the still-parked branch | `a_parked_thread_is_in_no_run_queue` (the sleeper resumes early) |
| `unpark`: `expect` instead of ignoring an unknown id | `unparking_an_id_that_never_existed_is_a_no_op` |

If the `is_current_anywhere` row leaves the suite green, the harness check is
not reaching it — say so in the commit message rather than deleting the row;
that gap is a finding about the harness, not about this task.

- [ ] **Step 9: Commit**

```sh
git add kernel/src/thread.rs kernel/src/sched.rs kernel/src/main.rs
git commit -m "feat(sched): blocked threads, park and an ISR-safe unpark"
```

---

### Task 3: The waker and `block_on`

**Files:**
- Create: `kernel/src/task.rs`
- Modify: `kernel/src/main.rs` (add `mod task;`)
- Test: `kernel/src/task.rs` (`#[test_case]`)

**Interfaces:**
- Consumes: `crate::sched::{current_id, park, unpark}` from Task 2.
- Produces:
  - `crate::task::waker_for(id: ThreadId) -> core::task::Waker`
  - `crate::task::block_on<F: Future>(future: F) -> F::Output`

- [ ] **Step 1: Write the failing tests**

In `kernel/src/task.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// A future that is `Pending` until a flag is set, and wakes through a
    /// `Waker` it stashed on the first poll -- the shape every device driver
    /// future in T4 will have.
    struct Flag {
        polls: u64,
    }

    static FLAG_READY: AtomicBool = AtomicBool::new(false);
    static FLAG_WAKER: AtomicU64 = AtomicU64::new(u64::MAX);

    impl Future for Flag {
        type Output = u64;
        fn poll(
            mut self: core::pin::Pin<&mut Self>,
            cx: &mut core::task::Context<'_>,
        ) -> core::task::Poll<u64> {
            self.polls += 1;
            if FLAG_READY.load(Ordering::Acquire) {
                return core::task::Poll::Ready(self.polls);
            }
            // Taken from the `Waker` the executor handed us, *not* from
            // `sched::current_id()`. Reading the id directly would give the
            // right answer here and test nothing: the whole question is
            // whether the waker carries the thread it was built for, and a
            // future that bypasses it would pass with the waker plumbing
            // removed entirely.
            FLAG_WAKER.store(raw_id_of(cx.waker()), Ordering::Release);
            core::task::Poll::Pending
        }
    }

    #[test_case]
    fn a_future_that_is_ready_immediately_never_parks() {
        // The cheap path, and the one a mistake in `block_on`'s loop makes
        // expensive: parking before the first poll would block every ready
        // future until something else woke the thread.
        let value = block_on(core::future::ready(7u32));
        assert_eq!(value, 7);
    }

    #[test_case]
    fn a_pending_future_resumes_when_another_thread_wakes_it() {
        // End to end: poll returns Pending, the thread parks, a *different*
        // thread wakes it through the id the future stashed, and `block_on`
        // polls again and completes. This is the whole runtime in one test.
        FLAG_READY.store(false, Ordering::Release);
        FLAG_WAKER.store(u64::MAX, Ordering::Release);

        extern "C" fn waker_thread(_: u64) -> ! {
            // Wait until the sleeper has actually parked. Setting the flag
            // before it parks would pass through the pending-wake path
            // instead, which is a different test.
            loop {
                let raw = FLAG_WAKER.load(Ordering::Acquire);
                if raw != u64::MAX && crate::sched::is_parked(qunix_sched::ThreadId(raw)) {
                    FLAG_READY.store(true, Ordering::Release);
                    crate::sched::unpark(qunix_sched::ThreadId(raw));
                    break;
                }
                crate::sched::yield_now();
            }
            crate::sched::exit_current()
        }

        crate::sched::spawn_kernel(waker_thread, 0, qunix_sched::Priority::Normal);
        let polls = block_on(Flag { polls: 0 });
        assert!(polls >= 2, "the future completed without ever having been pending");
    }

    #[test_case]
    fn a_waker_round_trips_the_thread_id_it_was_built_from() {
        // The waker's data word *is* the id. Getting the cast wrong is silent:
        // `unpark` would take an id that names nothing, find no thread, and
        // return -- so every await would hang, with no message.
        let id = crate::sched::current_id();
        let waker = waker_for(id);
        assert_eq!(raw_id_of(&waker), id.0, "the waker does not name its own thread");
        // Cloning must not change the id, and must not allocate. The identity
        // clone is what lets an ISR clone a waker at all.
        let clone = waker.clone();
        assert_eq!(raw_id_of(&clone), id.0, "cloning a waker changed the thread it names");
    }

    #[test_case]
    fn waking_through_a_stale_waker_does_not_disturb_a_later_thread() {
        // The reason there is no generation counter. A waker kept past its
        // thread's death must be inert -- if ids were reused, this would wake
        // whichever thread inherited the number, at a suspension point it
        // never reached.
        static DEAD: AtomicU64 = AtomicU64::new(u64::MAX);

        extern "C" fn brief(_: u64) -> ! {
            DEAD.store(crate::sched::current_id().0, Ordering::Release);
            crate::sched::exit_current()
        }

        let id = crate::sched::spawn_kernel(brief, 0, qunix_sched::Priority::Normal);
        while crate::sched::thread_id_is_live(id) {
            crate::sched::yield_now();
        }
        let stale = waker_for(id);
        let before = crate::sched::thread_count();
        stale.wake();
        assert_eq!(
            crate::sched::thread_count(),
            before,
            "waking a dead thread's waker changed the thread table"
        );
        assert_eq!(DEAD.load(Ordering::Acquire), id.0);
    }
}
```

`raw_id_of` is a test-support accessor; add it to the implementation in Step 3
rather than leaving the test referring to something that does not exist.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo xtask test`
Expected: FAIL to compile — `file not found for module task`.

- [ ] **Step 3: Write the implementation**

Create `kernel/src/task.rs`:

```rust
//! The async runtime: one thread, one future, no allocation.
//!
//! # Why there is no task table
//!
//! The future being awaited is a local in [`block_on`]'s frame, pinned to the
//! kernel stack the thread already owns, and the thing the scheduler resumes
//! is the thread. A slab of `Pin<Box<dyn Future>>` tasks would buy the ability
//! to run more concurrent operations than there are threads, and would cost a
//! heap allocation per operation in the one subsystem where allocating is a
//! deadlock hazard: memory pressure triggers writeback, writeback needs I/O,
//! and I/O would need the allocation that is already waiting.
//!
//! # Why the waker holds a bare id
//!
//! A `RawWaker` carries one `*const ()`. The usual occupant is an
//! `Arc<Task>`, whose `clone` and `drop` are refcount traffic an interrupt
//! handler would have to perform -- and whose *last* drop frees memory, from
//! interrupt context, entering the heap allocator underneath whatever lock the
//! interrupted code was holding. This one carries `ThreadId.0`, so `clone` is
//! the identity and `drop` does nothing.
//!
//! That is sound only because thread ids are never reused; see
//! `sched::Scheduler::allocate_id`, which says so and is guarded. A waker that
//! outlives its thread names an id that will never belong to anything else,
//! and `sched::unpark` ignores it.

use core::future::Future;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use qunix_sched::ThreadId;

/// Identity clone: the data word is a value, not a pointer to a refcount, so
/// there is nothing to increment and nothing that could be freed.
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

/// Nothing is owned, so nothing is released. Deliberately not `unreachable!`:
/// `Waker`'s drop runs on every temporary an executor creates, and this one is
/// reached on the ordinary path.
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
    // SAFETY: the data word is a `ThreadId.0` and every function in `VTABLE`
    // requires exactly that. `u64 as *const ()` is a provenance-free integer
    // that is never dereferenced -- only cast back to `u64` in the vtable.
    unsafe { Waker::from_raw(RawWaker::new(id.0 as *const (), &VTABLE)) }
}

/// The thread id a waker built by [`waker_for`] names.
///
/// Exists so a test can assert the round trip. Getting the cast wrong is
/// otherwise silent: `unpark` would be handed an id naming nothing, find no
/// thread, return, and every `await` in the kernel would hang with no message.
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
        // for `Ready`-on-first-poll futures -- which the majority of cached
        // reads will be -- nothing ever would.
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        // A spurious wake costs one extra poll and is why this is a loop
        // rather than a single park: `park` may return without the condition
        // having become true, and the contract of `Future::poll` allows it.
        crate::sched::park();
    }
}
```

Add `mod task;` to `kernel/src/main.rs` alongside the other module
declarations.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo xtask test`
Expected: `all 63 tests passed`.

- [ ] **Step 5: Falsify each guard**

| Mutation | Test that must fail |
| --- | --- |
| `block_on`: park before the first poll | `a_future_that_is_ready_immediately_never_parks` (hangs — treat a hang as a fail and note it) |
| `block_on`: `if let Poll::Ready` → return after one poll regardless | `a_pending_future_resumes_when_another_thread_wakes_it` |
| `waker_for`: store `id.0 + 1` | `a_waker_round_trips_the_thread_id_it_was_built_from` |
| `clone`: return a waker naming `0` | `a_waker_round_trips_the_thread_id_it_was_built_from` |

A mutation whose failure mode is a hang rather than an assertion is a weakness
in the test, not in the mutation. Give the two `block_on` tests a tick deadline
like `a_wake_that_arrives_before_the_park_does_not_block_the_thread` has, so
they fail with a name instead of stopping the suite.

- [ ] **Step 6: Commit**

```sh
git add kernel/src/task.rs kernel/src/main.rs
git commit -m "feat(task): block_on and an allocation-free waker"
```

---

### Task 4: `sleep_ticks` — the interrupt handler completes a future

This is the task that answers the spec's sharpest risk. Everything before it
can be driven by another thread; this one is woken by an ISR, from interrupt
context, with no thread involved.

**Files:**
- Modify: `kernel/src/task.rs` (the timer wheel and `sleep_ticks`)
- Modify: `kernel/src/main.rs` (call the wheel from `timer_handler`)
- Test: `kernel/src/task.rs`

**Interfaces:**
- Consumes: `crate::task::{block_on, waker_for}`, `crate::sched::unpark`.
- Produces:
  - `crate::task::sleep_ticks(n: u64) -> impl Future<Output = ()>`
  - `crate::task::try_sleep_ticks(n: u64) -> Result<impl Future<Output = ()>, SleepError>`
  - `crate::task::SleepError::Full`
  - `crate::task::expire_timers(now: u64)` — called from the timer ISR

- [ ] **Step 1: Write the failing tests**

```rust
    #[test_case]
    fn a_sleep_is_woken_by_the_timer_interrupt() {
        // The milestone's sharpest risk, tested directly: nothing but the
        // interrupt handler makes this thread runnable again. If wakers cannot
        // be driven from interrupt context, this hangs, and the deadline turns
        // that into a named failure instead of a stopped suite.
        let start = crate::TICKS.load(Ordering::Relaxed);
        block_on(sleep_ticks(3));
        let elapsed = crate::TICKS.load(Ordering::Relaxed) - start;
        assert!(elapsed >= 3, "slept {elapsed} ticks, asked for 3");
        // Bounded above as well. A sleep that returns only when something
        // *else* happens to wake the thread would satisfy the lower bound on a
        // busy machine and tell us nothing about the timer path.
        assert!(elapsed < 100, "slept {elapsed} ticks for a 3-tick sleep");
    }

    #[test_case]
    fn a_sleep_of_zero_ticks_does_not_park() {
        // A deadline already in the past must be `Ready` on the first poll. If
        // it registered instead, the thread would park until the *next* tick
        // for a sleep it was told took no time -- and on a CPU whose timer is
        // masked, forever.
        let start = crate::TICKS.load(Ordering::Relaxed);
        block_on(sleep_ticks(0));
        assert_eq!(crate::TICKS.load(Ordering::Relaxed) - start, 0);
    }

    #[test_case]
    fn a_full_timer_table_refuses_rather_than_returning_a_sleep_that_never_fires() {
        // The negative direction, and the one that matters: a registration
        // that silently fails produces a future that is `Pending` forever with
        // nothing scheduled to wake it. Refusing is the only outcome the
        // caller can act on.
        //
        // The table is filled directly rather than by spawning `CAPACITY`
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
        // Otherwise the table fills permanently after `CAPACITY` sleeps and
        // every later sleep is refused -- which the test above would pass and
        // nothing else would notice until the kernel had been up a while.
        let free_before = free_timer_slots_for_test();
        block_on(sleep_ticks(1));
        assert_eq!(
            free_timer_slots_for_test(),
            free_before,
            "a completed sleep left its slot occupied"
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo xtask test`
Expected: FAIL to compile — `cannot find function sleep_ticks`.

- [ ] **Step 3: Implement the timer wheel**

Append to `kernel/src/task.rs`:

```rust
/// How many sleeps can be outstanding at once.
///
/// A fixed table rather than a list, because the ISR walks it: allocating in
/// `expire_timers` would put the heap allocator on the interrupt path, and a
/// linked list would need a node from somewhere.
const CAPACITY: usize = 64;

/// One outstanding sleep.
///
/// `deadline` is an absolute tick count, not a remaining count. A remaining
/// count would have to be decremented by the ISR for every entry on every
/// tick, and a tick missed while interrupts were masked would silently
/// lengthen every sleep in the table.
#[derive(Clone, Copy)]
struct Timer {
    deadline: u64,
    thread: ThreadId,
}

/// `IrqSpinLock`, not `SpinLock`: this is taken from the timer ISR, so a plain
/// spinlock would let a tick land on a CPU that already holds it and spin
/// against itself forever.
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

/// Wakes every thread whose deadline has passed. Called from the timer ISR.
///
/// Takes `now` as an argument rather than reading `TICKS` itself, so the
/// handler's increment and this comparison cannot disagree about which tick
/// this is.
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
    // Unparked after the timer lock is released. `unpark` takes SCHED and may
    // take a run queue; holding TIMERS across that would order three locks in
    // a way nothing else does, and this path runs from an interrupt.
    for id in woken.iter().flatten() {
        crate::sched::unpark(*id);
    }
}

/// A future that completes once `deadline` has passed.
struct Sleep {
    deadline: u64,
    /// Whether this sleeper holds a slot in [`TIMERS`].
    ///
    /// Tracked so a re-poll -- which a spurious wake produces, and which
    /// `block_on`'s loop makes routine -- does not register a second slot for
    /// one sleeper and leak it.
    registered: bool,
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: core::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if crate::TICKS.load(core::sync::atomic::Ordering::Relaxed) >= self.deadline {
            return Poll::Ready(());
        }
        // Nothing is registered here. The slot was taken by
        // `try_sleep_ticks`, before this future existed, which is why that is
        // the fallible entry point and `poll` is infallible. Registering on
        // first poll instead would mean a `poll` that can fail with no way to
        // report it, and a re-poll -- which a spurious wake makes routine --
        // would take a second slot for one sleeper and leak the first.
        Poll::Pending
    }
}

impl Drop for Sleep {
    /// Releases the slot if the sleeper is dropped before it fires.
    ///
    /// Reachable whenever an `await` is abandoned -- a cancelled read, a
    /// `select` that another arm won. Without this the slot is occupied by a
    /// thread that is no longer waiting, and after `CAPACITY` cancellations
    /// every sleep in the kernel is refused.
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
    let now = crate::TICKS.load(core::sync::atomic::Ordering::Relaxed);
    let deadline = now.saturating_add(ticks);
    // A deadline already reached takes no slot at all. Registering one would
    // park the caller until the next tick for a sleep of zero, and on a CPU
    // whose timer is masked that is forever.
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
/// Panics if the timer table is full. Callers that can do something better
/// than die should use [`try_sleep_ticks`]; this exists because most callers
/// cannot, and a silent non-sleep is worse than a named panic.
pub fn sleep_ticks(ticks: u64) -> impl Future<Output = ()> {
    match try_sleep_ticks(ticks) {
        Ok(sleep) => sleep,
        Err(SleepError::Full) => panic!("no free timer slot for a {ticks}-tick sleep"),
    }
}

/// Occupies every free timer slot. Returns how many it newly took.
///
/// The entries name `ThreadId(u64::MAX)`, which is the `NO_THREAD` sentinel and
/// therefore an id `allocate_id` can never issue -- so if one is ever expired
/// by mistake, `unpark` ignores it rather than waking a real thread.
#[cfg(test)]
pub fn fill_timer_table_for_test() -> usize {
    let mut timers = TIMERS.lock();
    let mut taken = 0;
    for slot in timers.iter_mut().filter(|s| s.is_none()) {
        *slot = Some(Timer { deadline: u64::MAX, thread: ThreadId(u64::MAX) });
        taken += 1;
    }
    taken
}

/// Empties the timer table. The entries `fill_timer_table_for_test` wrote name
/// no live thread, so nothing is woken and nothing is lost.
#[cfg(test)]
pub fn clear_timer_table_for_test() {
    TIMERS.lock()[..].fill(None);
}

#[cfg(test)]
pub fn free_timer_slots_for_test() -> usize {
    TIMERS.lock().iter().filter(|s| s.is_none()).count()
}
```

- [ ] **Step 4: Call it from the timer handler**

In `kernel/src/main.rs`, in `timer_handler`, between the tick increment and the
EOI:

```rust
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // Before the EOI and before `preempt`. A sleeper whose deadline has passed
    // must be runnable *when* the scheduler runs, not one tick later: expiring
    // after `preempt` would let this CPU pick the next thread while the one
    // this tick released is still parked.
    crate::task::expire_timers(now);
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo xtask test`
Expected: `all 67 tests passed`.

- [ ] **Step 6: Falsify each guard**

| Mutation | Test that must fail |
| --- | --- |
| `expire_timers`: `now >= t.deadline` → `now == t.deadline` | `a_sleep_is_woken_by_the_timer_interrupt` (a tick missed across a switch steps over the deadline) |
| `try_sleep_ticks`: `ok_or(SleepError::Full)` → reuse slot 0 | `a_full_timer_table_refuses_rather_than_returning_a_sleep_that_never_fires` |
| `try_sleep_ticks`: drop the `deadline <= now` short circuit | `a_sleep_of_zero_ticks_does_not_park` |
| `expire_timers`: `slot.take()` → read without clearing | `an_expired_timer_frees_its_slot` |
| `Sleep::drop`: make it a no-op | needs a test — see below |

The `Sleep::drop` row will not fail, because no test drops an unfired sleeper.
Write one before finishing the task:

```rust
    #[test_case]
    fn abandoning_a_sleep_before_it_fires_frees_its_slot() {
        // Cancellation is not hypothetical: T4's read futures are dropped
        // whenever a request is abandoned. A slot leaked per cancellation
        // fills the table permanently, and every later sleep in the kernel is
        // refused -- long after the code that leaked it ran.
        let free_before = free_timer_slots_for_test();
        drop(try_sleep_ticks(1_000_000).expect("the timer table was full"));
        assert_eq!(
            free_timer_slots_for_test(),
            free_before,
            "an abandoned sleep left its slot occupied"
        );
    }
```

- [ ] **Step 7: Commit**

```sh
git add kernel/src/task.rs kernel/src/main.rs
git commit -m "feat(task): timer sleeps, completed from the interrupt handler"
```

---

### Task 5: Make the invariant a harness check, and write the milestone up

The scheduler now has an invariant no single test owns: **a parked thread is in
no run queue, on any CPU, at any instant.** Task 2 asserts it in one test, at
one moment, for one thread. That is the same shape as the check
`assert_no_thread_runs_twice` replaced — an invariant that holds always,
verified once.

**Files:**
- Modify: `kernel/src/testing.rs`
- Modify: `kernel/src/sched.rs` (expose what the harness needs)
- Modify: `CLAUDE.md`
- Modify: `docs/superpowers/plans/2026-08-11-m2-async-runtime.md` (the Execution
  Deviations section below)

**Interfaces:**
- Consumes: `crate::sched::is_parked`, `qunix_hal_x86_64::percpu::run_queue_of`.
- Produces: `crate::sched::parked_thread_ids() -> alloc::vec::Vec<ThreadId>`.

- [ ] **Step 1: Add the accessor**

```rust
/// Every thread the scheduler currently considers parked.
///
/// Allocates, so it is not for any hot path -- it exists for the harness check
/// that no parked thread is queued, which runs once per test.
pub fn parked_thread_ids() -> alloc::vec::Vec<ThreadId> {
    SCHED
        .lock()
        .threads
        .iter()
        .filter(|(_, t)| t.park.is_parked())
        .map(|(id, _)| *id)
        .collect()
}
```

- [ ] **Step 2: Add the harness check**

In `kernel/src/testing.rs`, beside `assert_no_thread_runs_twice`:

```rust
/// Fails the run if a parked thread is sitting in any CPU's run queue.
///
/// An instantaneous invariant, like `assert_no_thread_runs_twice` and for the
/// same reason: a thread that is both parked and dispatchable will be resumed
/// at a suspension point it has not returned from, and the symptom is a future
/// polled from the wrong state somewhere else entirely. Checked after every
/// test rather than in the one test that looks for it, because the tests that
/// *stress* the runtime are not the ones that would notice.
fn assert_no_parked_thread_is_queued(name: &str) {
    use qunix_hal_x86_64::percpu::{MAX_CPUS, run_queue_of};
    for id in crate::sched::parked_thread_ids() {
        for cpu in 0..MAX_CPUS {
            let Some(queue) = run_queue_of(cpu) else { continue };
            // `try_lock`: a CPU mid-dispatch holds its queue, and a harness
            // check that blocks on it would turn a scheduling delay into a
            // hung suite. A queue that cannot be inspected is skipped rather
            // than waited for -- the invariant is checked after every test, so
            // a genuine violation is not going to be missed by every one.
            let Some(queue) = queue.try_lock() else { continue };
            assert!(
                !queue.contains(id),
                "after {name}: parked {id:?} is queued on cpu {cpu}; it can be dispatched \
                 and would resume at a suspension point it has not returned from"
            );
        }
    }
}
```

and call it from `Testable::run`, after `assert_no_thread_runs_twice(name)`.

- [ ] **Step 3: Verify the check can fail**

Temporarily make `unpark` queue on `WakeOutcome::Noted` as well, run
`cargo xtask test`, and confirm a test fails naming a parked queued thread.
Restore.

A harness check that has never been seen to fail is a harness check that might
be inert — `gate_unmeasured` in `xtask` was exactly that, and it guarded the
coverage ratchet.

- [ ] **Step 4: Update `CLAUDE.md`**

Add to the scheduler section, after the four existing bullets:

```markdown
- **A parked thread is in no run queue.** `sched::park` marks the thread and
  `schedule` declines to hand it on; `unpark` queues it only when it is parked
  *and* not still current on some CPU. All three are the same invariant reached
  three ways, and the harness checks it after every test. A parked thread that
  is dispatchable resumes at a suspension point it never returned from.
- **A `Waker` holds a bare `ThreadId` and nothing else.** No `Arc`, no
  generation counter, no allocation — which is only sound because
  `Scheduler::allocate_id` never reuses an id. Waking a dead thread is a no-op
  by construction. If ids ever become reusable, every stale completion in the
  kernel becomes a wakeup delivered to the wrong thread.
- **`block_on` polls before it parks**, and loops. A spurious wake costs one
  poll; the opposite mistake costs a thread.
```

- [ ] **Step 5: Record the deviations**

Append to the Execution Deviations section of this plan (below) anything
reality contradicted. D8 is already written; add D9 onward as they occur.

- [ ] **Step 6: Run the full suite and commit**

```sh
cargo xtask test
```
Expected: `all 68 tests passed`, `coverage: 6 crate(s) at or above their
floor`. Check for `error[` as well as the pass line.

```sh
git add kernel/src/testing.rs kernel/src/sched.rs CLAUDE.md docs/superpowers/plans/2026-08-11-m2-async-runtime.md
git commit -m "test(harness): a parked thread is in no run queue, checked after every test"
```

---

## Execution Deviations

A plan written before the code is a hypothesis; the deviations are the result.
Record what reality contradicted here rather than silently diverging. M0 needed
sixteen of these and M1 needed seven — an empty section at the end of a
milestone means they were not written down, not that there were none.

### D8 — `unpark` takes the scheduler lock instead of a lock-free ready list

**Spec said:** "An ISR pushes a task id onto a lock-free per-CPU ready list;
the scheduler drains it. The ISR does not construct an `Arc`, does not
allocate, and does not touch a lock that a non-interrupt path could hold — the
self-deadlock `IrqSpinLock` exists to prevent."

**Plan does:** `unpark` takes `SCHED`, an `IrqSpinLock`, directly.

**Why:** the deadlock the sentence describes is the one `IrqSpinLock` already
prevents — it masks interrupts before acquiring, so no ISR can land on a CPU
holding it — and `preempt()` has taken `SCHED` from the timer ISR since M1. The
lock-free list would buy latency, not correctness. It would also have to answer
"what happens when the ring is full", and both answers are worse than the lock:
dropping a wake hangs a thread silently, and falling back to the lock ships the
lock anyway plus a path no test naturally exercises. An unbounded list needs a
node per wake, which means allocating in the ISR — which the spec forbids for
stronger reasons.

**If it turns out to matter:** the ring goes in front of this path with the
lock as its overflow fallback, which is the only arrangement with a correct
answer for a full ring. That is a measured optimisation, and the benchmark
harness exists to show it is needed first.

### D9 — `PROCESSES` never existed

**Spec said:** T2 is done "except for process reaping"; `PROCESSES` grows
without bound.

**Reality:** there is no process table. `spawn` boxes a `Process`,
`user_thread_entry` consumes that box, and the address space moves into the
`Thread`, which `sched::reap` drops. The claim was copied from M1's *plan* —
where `PROCESSES` appears — rather than from the code that replaced it.

Struck at the source in both
`docs/superpowers/plans/2026-08-04-m1-processes.md` and
`docs/superpowers/specs/2026-08-05-m2-filesystems-design.md`, per the spec's own
rule that a handoff list wrong in one document and right in another is worse
than one that is simply wrong. This is the fourth stale entry found in that list
and the first that was never true, which is the more dangerous kind: it survives
a check against the plan and fails only against the source.

The process table returns in T6, where a per-process fd table gives a process
state that outlives its entry thread. Reaping is designed there, against a table
that exists.

---

## What T3 does not do

Stated so the next plan does not assume it:

- **No concurrency beyond thread count.** One thread awaits one future at a
  time. Joining futures within a thread works; running them on two CPUs does
  not. T5 decides whether background writeback needs more.
- **No cancellation of in-flight device requests.** `Sleep` releases its timer
  slot on drop, which is cancellation for the only future that exists here. A
  dropped *device* future has to tell the device, and that is T4's problem with
  T4's DMA buffers in hand.
- **No executor fairness.** A thread that never awaits is preempted by the
  timer, exactly as before; nothing here changes scheduling policy.
- **No async syscalls.** `block_on` is callable from a syscall handler and the
  handler blocks the calling thread, which is the point — but no syscall does
  it yet, because none has anything to wait for until T4.
