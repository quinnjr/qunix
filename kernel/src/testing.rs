use qunix_hal_x86_64::{print, println};

/// The verdict values and the host statuses xtask matches on live in
/// `qunix-abi`, which both targets build, so there is one definition rather
/// than two copies checked against each other.
pub use qunix_abi::ExitCode;

/// Signals the verdict to the host and halts.
pub fn exit_qemu(code: ExitCode) -> ! {
    unsafe { qunix_hal_x86_64::port::outl(0xf4, code as u32) };
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

/// Machine state that a test must leave exactly as it found it.
///
/// Every field here is something a test can change that silently alters what
/// *later* tests mean, while failing nothing itself. That is the failure this
/// harness is worst at catching on its own: the verdict is one port write, so
/// anything that degrades behaviour without halting still reports green.
///
/// It is not hypothetical. `apic_timer_fires_and_advances_the_tick_counter`
/// ended with a bare `interrupts::disable()`. Every test that ran after it on
/// that processor therefore ran with interrupts masked, so preemption was
/// absent for all of them -- including the tests whose entire purpose is to
/// observe preemption. The suite was green and had not been testing what it
/// claimed for as long as that line existed. It survived a full milestone and
/// four review passes, because nothing compared the machine before a test with
/// the machine after it.
///
/// The fields are deliberately not "everything observable". They are the state
/// that is *global, sticky, and invisible*: changing it has no immediate effect
/// the changing test would notice, and no later test announces that it is
/// running under the wrong conditions.
///
/// Two candidates were considered and rejected, because a harness field that
/// fails for a legitimate reason is worse than no field at all:
///
/// - **Free frames.** It would catch a leaked address space, which is exactly
///   the shape of state this wants. It cannot be used: several tests map a
///   *higher-half* scratch address and unmap it with plain `unmap`, which
///   leaks the three intermediate tables on purpose — `unmap_and_prune`
///   refuses the shared kernel half, so those frames are genuinely
///   unreclaimable. The check would fail five honest tests.
/// - **Live thread count.** Threads finish and are reaped on processors this
///   thread never yields to, so a count sampled the instant a test ends is a
///   sample of the scheduler's timing, not of the test's tidiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MachineState {
    /// `RFLAGS.IF`. A test that leaves interrupts masked disables the timer,
    /// and with it preemption, for everything that follows.
    interrupts_enabled: bool,
    /// The scheduler's preemption flag. Same consequence as the above, reached
    /// a different way, so checking only one of them would leave the other as
    /// the next silent hole -- a guard applied to one of two places, which is
    /// the defect class this project keeps finding.
    preemption_enabled: bool,
    /// CR3. A test that activates an address space and does not switch back
    /// leaves every later test walking a page table it does not own -- and one
    /// that may be freed underneath it.
    root_frame: u64,
    /// Which processor the harness thread is on, read through `GS`.
    ///
    /// Two different failures land here. `GS.base` is repointed or zeroed --
    /// ring 3 can do the latter with `mov gs, ax`, and CLAUDE.md records that
    /// one of the two entry paths back from ring 3 forgot to restore it -- and
    /// every later `gs:`-relative read in the suite silently names the wrong
    /// CPU's block or linear address 0x20. Or the harness thread genuinely
    /// migrated, which would mean an idle-band thread was stolen: it adopted
    /// the stack its CPU booted on, so two CPUs would be on one stack.
    ///
    /// Safe to compare because the harness thread cannot legitimately move.
    /// It adopted the boot context, so it is in the idle band, and
    /// `RunQueue::runnable_len` excludes that band from what a thief may take.
    cpu_id: u32,
    /// Wake IPIs that could not be sent for want of a LAPIC base.
    ///
    /// Must not move during a test. Every processor has its LAPIC up by the
    /// time the suite runs, so a drop here means a wake was posted from a
    /// processor that could not advertise it -- and the thread it was meant for
    /// is left waiting on a timer tick that may not be coming. Counted rather
    /// than asserted at the source because `unpark` runs from interrupt
    /// context, where a panic fires again on every subsequent tick.
    wake_ipis_dropped: u64,
    /// Processes killed by a ring-3 fault.
    ///
    /// A test that expects one says so with [`expect_process_kills`]; every
    /// other test must cause none. That direction is the point. A fault taken
    /// in ring 0 must *panic*, and the check deciding which is a single
    /// comparison on the saved CS -- if it ever answered "user" for a kernel
    /// fault, the kernel would quietly reap the faulting thread and carry on,
    /// and every existing assertion in this suite would still hold. This is
    /// the only thing that would notice.
    process_kills: u64,
}

/// Kills the running test has declared it will cause.
///
/// Consumed by the harness after each test, so a declaration cannot leak into
/// the next one.
static EXPECTED_KILLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Declares that this test will kill `n` processes by faulting them.
///
/// A declaration rather than a blanket exemption: the harness then asserts the
/// count moved by *exactly* `n`, so a test that expects one kill and gets two,
/// or none, fails.
pub fn expect_process_kills(n: u64) {
    EXPECTED_KILLS.store(n, core::sync::atomic::Ordering::Release);
}

/// Fails the run if two processors name the same thread as the one they are
/// running.
///
/// Not a before/after comparison — it is an invariant that holds at every
/// instant, and the whole scheduler is arranged around it. A thread is removed
/// from a run queue before it is dispatched, and the outgoing thread is not
/// requeued until after its context is saved; either rule broken puts two CPUs
/// on one stack, and the symptom is memory corruption somewhere else entirely.
///
/// Checked after *every* test rather than in the one test that looks for it.
/// `every_processor_runs_a_thread_of_its_own` sorts before both work-stealing
/// tests, so as a single test it inspects the scheduler only in the state it
/// was in before anything stressed it.
fn assert_no_thread_runs_twice(name: &str) {
    use qunix_hal_x86_64::percpu::{MAX_CPUS, NO_THREAD, current_thread_of};
    for cpu in 0..MAX_CPUS {
        let Some(id) = current_thread_of(cpu) else { continue };
        if id == NO_THREAD {
            continue;
        }
        for other in (cpu + 1)..MAX_CPUS {
            let Some(other_id) = current_thread_of(other) else { continue };
            assert!(
                other_id != id || other_id == NO_THREAD,
                "after {name}: cpu {cpu} and cpu {other} both report running thread {id}; \
                 two processors are executing on one kernel stack"
            );
        }
    }
}

/// Fails the run if a parked thread is sitting in any processor's run queue.
///
/// An instantaneous invariant, like [`assert_no_thread_runs_twice`] and for the
/// same reason: a thread that is both parked and dispatchable will be resumed
/// at a suspension point it has not returned from, and the symptom is a future
/// polled from a state it never reached, somewhere else entirely.
///
/// Checked after *every* test rather than inside the one test that looks for
/// it. The tests that stress the runtime are not the ones that would notice --
/// `a_parked_thread_is_in_no_run_queue` inspects one thread at one instant,
/// which is the same shape as the single-test check that missed a whole
/// milestone's worth of leaked interrupt state.
fn assert_no_parked_thread_is_queued(name: &str) {
    use qunix_hal_x86_64::percpu::{MAX_CPUS, run_queue_of};
    for id in crate::sched::parked_thread_ids() {
        for cpu in 0..MAX_CPUS {
            let Some(queue) = run_queue_of(cpu) else { continue };
            // `try_lock`: a processor mid-dispatch holds its queue, and a
            // harness check that blocked on it would turn a scheduling delay
            // into a hung suite. A queue that cannot be inspected is skipped
            // rather than waited for -- this runs after every test, so a real
            // violation will not be missed by all of them.
            let Some(queue) = queue.try_lock() else { continue };
            // Both facts sampled while this queue is held, and the parked one
            // re-read *first* so the pair is as close to simultaneous as this
            // check can make it.
            //
            // A single re-check covers only one direction. The earlier version
            // re-read `is_parked` after finding the id queued, which handles
            // "queued, then legitimately unparked" but not its mirror: unparked
            // and pushed, then popped, dispatched, and parked again by the time
            // of the re-read. That reports a violation on a correct run, and a
            // harness that fails honest runs is worse than one that misses a
            // violation -- CLAUDE.md's ratchet section is about exactly that.
            //
            // Note the lock order: `is_parked` takes SCHED while this queue
            // guard is alive, which is the reverse of the order the scheduler
            // documents. It is safe only because nothing else does so, and
            // `publish_handoff`'s enforcement covers the push itself, so this
            // scan is a backstop rather than the primary guard.
            if crate::sched::is_parked(id) && queue.contains(id) {
                panic!(
                    "after {name}: parked {id:?} is queued on cpu {cpu}; it can be dispatched, \
                     and would resume at a suspension point it has not returned from"
                );
            }
        }
    }
}

impl MachineState {
    fn capture() -> Self {
        // SAFETY: a read of the live CR3 through the HHDM, which boot maps for
        // all of RAM. `AddressSpace::active` only reads the register and wraps
        // it; nothing is dereferenced here.
        let root_frame = unsafe {
            qunix_hal_x86_64::paging::AddressSpace::active(crate::boot::hhdm_offset()).root_frame()
        };
        Self {
            interrupts_enabled: x86_64::instructions::interrupts::are_enabled(),
            preemption_enabled: crate::sched::preemption_enabled(),
            root_frame,
            cpu_id: qunix_hal_x86_64::percpu::cpu_id(),
            wake_ipis_dropped: crate::sched::wake_ipi_dropped_count(),
            process_kills: crate::syscall::process_kills(),
        }
    }

    /// Fails the run if `self` differs from `before`, naming the field.
    ///
    /// A panic rather than a warning, deliberately. A warning here would be a
    /// line of output nobody reads in a suite that already prints one line per
    /// test, and CLAUDE.md is explicit that a required change should be an
    /// error rather than a note. The test that leaked state is named, because
    /// the test that *fails* from it can be any later one.
    fn assert_restored(&self, before: &Self, name: &str, expected_kills: u64) {
        assert_eq!(
            self.interrupts_enabled, before.interrupts_enabled,
            "{name} left interrupts {}; every later test on this processor would run \
             with the wrong interrupt state, and preemption would be silently absent",
            if self.interrupts_enabled { "enabled" } else { "masked" }
        );
        assert_eq!(
            self.preemption_enabled, before.preemption_enabled,
            "{name} left preemption {}; later tests would not be preempted as they expect",
            if self.preemption_enabled { "enabled" } else { "disabled" }
        );
        assert_eq!(
            self.root_frame, before.root_frame,
            "{name} left CR3 at {:#x} rather than {:#x}; later tests would walk an address \
             space they do not own, which may also be freed underneath them",
            self.root_frame, before.root_frame
        );
        assert_eq!(
            self.cpu_id, before.cpu_id,
            "{name} started on cpu {} and ended on cpu {}; either GS was repointed, in which \
             case every later gs:-relative read names the wrong block, or an idle-band thread \
             was stolen, in which case two cpus are on one stack",
            before.cpu_id, self.cpu_id
        );
        assert_eq!(
            self.wake_ipis_dropped, before.wake_ipis_dropped,
            "{name} dropped {} wake IPI(s): a wake was posted from a processor with no LAPIC \
             base, so the thread it named is waiting on a tick that may never come",
            self.wake_ipis_dropped - before.wake_ipis_dropped
        );
        assert_eq!(
            self.process_kills,
            before.process_kills + expected_kills,
            "{name} killed {} process(es) by faulting; it declared {expected_kills}. A kill it \
             did not ask for means a ring-0 fault took the ring-3 path and reaped a kernel \
             thread instead of panicking",
            self.process_kills - before.process_kills
        );
    }
}

pub trait Testable {
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        let name = core::any::type_name::<T>();
        print!("{name} ... ");
        // Cleared before the body, not after: a declaration left behind by a
        // test that never reached its `expect_process_kills` would otherwise
        // excuse a kill in the *next* test.
        EXPECTED_KILLS.store(0, core::sync::atomic::Ordering::Release);
        let before = MachineState::capture();
        self();
        // After the test body and before "ok" is printed, so a leak is
        // attributed to the test that caused it rather than to whichever test
        // later trips over it.
        let expected = EXPECTED_KILLS.swap(0, core::sync::atomic::Ordering::AcqRel);
        MachineState::capture().assert_restored(&before, name, expected);
        assert_no_thread_runs_twice(name);
        assert_no_parked_thread_is_queued(name);
        println!("ok");
    }
}

pub fn runner(tests: &[&dyn Testable]) {
    println!("running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    println!("all {} tests passed", tests.len());
    exit_qemu(ExitCode::Success);
}
