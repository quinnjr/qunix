#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(custom_test_frameworks)]
#![test_runner(crate::testing::runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

mod boot;
mod frames;
mod heap;
mod panic;
mod process;
mod sched;
mod smp;
mod syscall;
mod task;
mod thread;
mod vmspace;
mod testing;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use qunix_hal_x86_64::println;
use x86_64::structures::idt::InterruptStackFrame;

pub static TICKS: AtomicU64 = AtomicU64::new(0);

static LAPIC_MAPPED: AtomicBool = AtomicBool::new(false);

extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    // First, before `preempt` reaches `gs:`. A timer tick is the one interrupt
    // that routinely arrives while ring 3 is running, and ring 3 can zero the
    // hidden `GS.base` with `mov gs, ax`. `preempt` -> `schedule` ->
    // `percpu::set_kernel_stack` dereferences `gs:[0x20]`, so without this the
    // scheduler writes a kernel stack pointer through a base the running
    // process chose. Unconditional rather than gated on the frame's CS: it is
    // idempotent, and a gate is one more place to get the direction wrong.
    // SAFETY: this CPU's per-CPU block was installed during boot.
    unsafe { qunix_hal_x86_64::percpu::restore_gs_base() };

    // A bus-locked RMW, deliberately: the ~20-40 cycles it costs once per 10 ms
    // are unmeasurable, and a per-CPU timer on more than one core makes a
    // load/store pair lose counts.
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // Before the EOI and before `preempt`. A sleeper whose deadline has passed
    // must be runnable *when* the scheduler runs, not one tick later: expiring
    // after `preempt` would let this processor pick its next thread while the
    // one this tick released was still parked.
    crate::task::expire_timers(now);
    // EOI before any switch. Switching first would leave the LAPIC waiting for
    // an EOI that only arrives when this thread runs again, so the CPU would
    // take no further timer interrupts until then -- which, if the thread is
    // waiting on something a timer drives, is never.
    qunix_hal_x86_64::apic::eoi();
    crate::sched::preempt();
}

/// Maps the local APIC's MMIO page into the HHDM range, uncacheable.
///
/// Limine's HHDM usually covers RAM only, leaving this page absent, but the
/// protocol allows it to span the low 4 GiB; either way the mapping this
/// installs is the only one with guaranteed-uncacheable flags.
pub fn map_lapic() {
    use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

    // Idempotency is tracked explicitly rather than inferred from the page
    // being present. `translate` reports presence, not cacheability, so an
    // early return on "already mapped" would silently accept a write-back
    // mapping of MMIO created by someone else. This flag means the only
    // mapping we skip is the one we know we made, with known-good flags.
    if LAPIC_MAPPED.load(Ordering::Acquire) {
        return;
    }

    let hhdm = boot::hhdm_offset();
    let phys = qunix_hal_x86_64::apic::phys_base();
    let va = hhdm + phys;
    let mut space = unsafe { AddressSpace::active(hhdm) };
    // The Limine protocol permits the HHDM to span the low 4 GiB, which covers
    // the LAPIC at 0xFEE0_0000, so a pre-existing mapping is a legal bootloader
    // configuration rather than an error. It is still unusable as-is --
    // `translate` reports presence, not cacheability, and a write-back mapping
    // of the LAPIC silently corrupts register access -- so replace it instead
    // of trusting it. `unmap`, not `unmap_and_prune`: these tables are the
    // bootloader's, and pruning would hand firmware-owned frames to our
    // allocator.
    if space.translate(va).is_some() {
        // SAFETY: nothing in the kernel has touched the LAPIC yet -- apic::init
        // runs after this function -- so no reference derived from `va` exists.
        unsafe {
            space.unmap(va).expect(
                "LAPIC page is covered by a huge HHDM mapping; cannot make it uncacheable \
                 without splitting the parent entry",
            );
        }
    }
    unsafe {
        space
            .map(
                va,
                phys,
                PageFlags::PRESENT
                    | PageFlags::WRITABLE
                    | PageFlags::NO_CACHE
                    | PageFlags::NO_EXECUTE,
                &mut || frames::alloc(0),
            )
            .expect("failed to map the local APIC");
    }
    LAPIC_MAPPED.store(true, Ordering::Release);
}

/// Vector an idle CPU is woken on when work appears.
///
/// A halted CPU with an empty run queue has no timer of its own to wake it and
/// no way to notice a push, so `sched::spawn_kernel` sends this after queueing.
pub const WAKE_VECTOR: u8 = 34;

/// Does nothing but acknowledge. Leaving `hlt` is the entire effect: the idle
/// loop re-reads the run queues the instant it resumes.
///
/// Deliberately touches no `gs:`-relative state. This can land while ring 3 is
/// running, and ring 3 is free to have zeroed the hidden GS base with
/// `mov gs, ax`; a handler that reads nothing through `GS` needs no repair, and
/// not needing one is cheaper and harder to get wrong than doing one.
extern "x86-interrupt" fn wake_handler(_frame: InterruptStackFrame) {
    qunix_hal_x86_64::apic::eoi();
}

/// Registers the interrupt vectors that are this CPU's own.
///
/// Both are per-CPU because the IDT is: `idt::set_handler` writes the table of
/// the CPU it is called on, so every CPU must run this or it triple-faults on
/// the first timer tick or wakeup IPI it is sent.
///
/// Separate from `apic::init` so tests can install the handlers before enabling
/// interrupts.
pub fn install_local_vectors() {
    unsafe {
        qunix_hal_x86_64::idt::set_handler(qunix_hal_x86_64::apic::TIMER_VECTOR, timer_handler);
        qunix_hal_x86_64::idt::set_handler(WAKE_VECTOR, wake_handler);
    };
}

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    qunix_hal_x86_64::serial::init();
    assert!(boot::base_revision_supported(), "limine base revision unsupported");
    println!("qunix: booted");

    // GDT, TSS and IDT are all per-CPU now and are installed together, because
    // the IDT's double-fault gate names an IST index that only means anything
    // once this CPU's TSS is loaded.
    unsafe { qunix_hal_x86_64::percpu::install_bsp() };
    println!("qunix: per-cpu block installed (cpu {})", qunix_hal_x86_64::percpu::cpu_id());

    // One walk, one number. The previous pair printed two slightly different
    // totals -- the raw memmap sum, then the allocator's -- because frames::init
    // skips the first megabyte and rounds region edges to pages.
    frames::init();
    let stats = frames::stats();
    println!(
        "qunix: hhdm at {:#x}, {} MiB of frames available ({} KiB dropped at region edges, \
         {} KiB refused, {} KiB malformed, {} KiB excluded below 1 MiB)",
        boot::hhdm_offset(),
        stats.free_bytes / (1024 * 1024),
        stats.edge_dropped_bytes / 1024,
        stats.refused_region_bytes / 1024,
        stats.malformed_region_bytes / 1024,
        stats.excluded_low_bytes / 1024,
    );

    heap::init();
    println!(
        "qunix: kernel heap online ({} KiB bump headroom)",
        heap::bump_remaining() / 1024
    );

    // Recorded while CR3 still holds the kernel's own tables and nothing has
    // activated a process address space. Every later address space copies its
    // kernel half from this, rather than from whatever CR3 happens to be.
    // SAFETY: no `VmSpace` has been activated at this point in boot.
    let kernel_root = unsafe { vmspace::record_kernel_root() };
    println!("qunix: kernel page-table root at {kernel_root:#x}");

    sched::init();
    // Not a demonstration for its own sake: this is the first code to run on a
    // stack the kernel allocated rather than the one Limine handed it, so a
    // fault here is the difference between "the scheduler compiles" and "the
    // scheduler works" on real hardware paths the tests cannot reach.
    let greeter = sched::spawn_kernel(greet, 0, qunix_sched::Priority::Normal);
    println!("qunix: scheduler online, spawned {greeter:?}");
    sched::yield_now();
    println!(
        "qunix: back on the boot thread, {} live, {} runnable",
        sched::thread_count(),
        sched::runnable_count()
    );

    install_local_vectors();
    map_lapic();
    // SAFETY: map_lapic() has just mapped the LAPIC page uncacheable at this
    // exact HHDM offset.
    unsafe { qunix_hal_x86_64::apic::init(boot::hhdm_offset()) };
    // The `SYSCALL` MSRs are per-CPU, so this is bring-up rather than something
    // the first user thread can do for itself: a thread that started on the BSP
    // and was later stolen by a processor which never ran this would raise #UD
    // on its next syscall.
    // SAFETY: this CPU's per-CPU block and GDT are installed.
    unsafe { crate::syscall::init() };
    qunix_hal_x86_64::apic::start_timer(0b1011, 10_000_000);
    x86_64::instructions::interrupts::enable();
    // The BSP can service an IPI from here on: IDT loaded, LAPIC enabled,
    // interrupts unmasked. Before this point a shootdown initiated by any other
    // CPU would wait on an acknowledgement the BSP could not send — which is
    // why the mask is set here and not in `install_bsp`.
    qunix_hal_x86_64::percpu::mark_online();
    // Only now: preemption before this point would let a tick switch threads
    // while the scheduler still had no thread table, and before the APIC timer
    // exists there is nothing to drive it anyway.
    sched::set_preemption(true);
    println!("qunix: apic timer running, preemption enabled");

    match boot::module("init") {
        Some(image) => match process::spawn_elf(image) {
            Ok(id) => println!("qunix: loaded init ({} bytes) as {id:?}", image.len()),
            Err(e) => println!("qunix: init failed to load: {e:?}"),
        },
        // Not fatal: the kernel is usable without userspace, and a missing
        // module is a boot configuration problem rather than a kernel fault.
        None => println!("qunix: no init module; continuing without userspace"),
    }
    // Hand the CPU over so init actually runs before this thread moves on.
    sched::yield_now();

    let started = smp::start_all();
    // Bounded: a firmware that lists a CPU it cannot start would otherwise hang
    // the boot, which is worse than running with fewer cores.
    let all = smp::wait_for_all(200_000_000);
    println!(
        "qunix: smp {}/{} application processors online (of {} cpus){}",
        smp::online_count(),
        started,
        smp::cpu_count(),
        if all { "" } else { " -- TIMED OUT" }
    );

    #[cfg(test)]
    test_main();

    // The bootstrap processor becomes an ordinary scheduling CPU like the rest,
    // rather than halting with a run queue it would never look at again.
    sched::idle_loop();
}

/// The first thread the kernel ever schedules.
extern "C" fn greet(_: u64) -> ! {
    println!("qunix: hello from {:?} on its own stack", sched::current_id());
    sched::exit_current();
}

#[cfg(test)]
mod tests {
    /// Rendezvous for the context-switch test: where the main thread's context
    /// is parked so the child can switch back to it.
    ///
    /// A static rather than a captured local because the child entry point is
    /// an `extern "C" fn` -- it takes one `u64` and closes over nothing.
    static SWITCH_MAIN_CTX: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(0);
    static SWITCH_CHILD_RAN: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(0);

    extern "C" fn switch_child(arg: u64) -> ! {
        use core::sync::atomic::Ordering;
        SWITCH_CHILD_RAN.store(arg, Ordering::SeqCst);
        // Switch back to whoever started us.
        //
        // The static holds the *address of the slot*, not the context: `switch`
        // writes the outgoing context through `from`, so the slot is only
        // filled in once control has left the main thread -- which is after
        // this static was set. Dereferencing it here is what reads the value
        // that switch just stored. Treating the slot address as the context
        // itself jumps into the main thread's stack and executes it.
        let slot = SWITCH_MAIN_CTX.load(Ordering::SeqCst)
            as *mut *mut qunix_hal_x86_64::context::Context;
        let main_ctx = unsafe { *slot };
        let mut scratch: *mut qunix_hal_x86_64::context::Context = core::ptr::null_mut();
        unsafe { qunix_hal_x86_64::context::switch(&raw mut scratch, main_ctx) };
        unreachable!("the child was resumed after handing control back");
    }

    #[test_case]
    fn context_switch_runs_a_new_thread_and_comes_back() {
        use alloc::boxed::Box;
        use core::sync::atomic::Ordering;
        use qunix_hal_x86_64::context;

        crate::frames::init();
        crate::heap::init();
        SWITCH_CHILD_RAN.store(0, Ordering::SeqCst);

        // Leaked: the child's saved context lives on this stack, and the child
        // is never resumed to unwind it, so freeing it here would hand a live
        // stack back to the allocator.
        let stack = Box::leak(alloc::vec![0u8; context::MIN_STACK].into_boxed_slice());
        let raw_top = stack.as_ptr() as u64 + context::MIN_STACK as u64;
        let stack_top = raw_top & !0xf;

        let child = unsafe { context::init_kernel_stack(stack_top, switch_child, 0xC0FFEE) };
        let mut here: *mut context::Context = core::ptr::null_mut();
        SWITCH_MAIN_CTX.store((&raw mut here) as u64, Ordering::SeqCst);

        // Control leaves here and comes back only when the child switches back.
        unsafe { context::switch(&raw mut here, child) };

        assert_eq!(
            SWITCH_CHILD_RAN.load(Ordering::SeqCst),
            0xC0FFEE,
            "the child never ran, or did not receive its argument"
        );
        assert!(!here.is_null(), "switch did not record this thread's context");
    }

    #[test_case]
    fn context_switch_preserves_callee_saved_registers() {
        use alloc::boxed::Box;
        use core::sync::atomic::Ordering;
        use qunix_hal_x86_64::context;

        crate::frames::init();
        crate::heap::init();
        SWITCH_CHILD_RAN.store(0, Ordering::SeqCst);

        let stack = Box::leak(alloc::vec![0u8; context::MIN_STACK].into_boxed_slice());
        let stack_top = (stack.as_ptr() as u64 + context::MIN_STACK as u64) & !0xf;
        let child = unsafe { context::init_kernel_stack(stack_top, switch_child, 1) };
        let mut here: *mut context::Context = core::ptr::null_mut();
        SWITCH_MAIN_CTX.store((&raw mut here) as u64, Ordering::SeqCst);

        // The point of saving rbx/r12-r15 is that they survive. The child
        // deliberately runs arbitrary code in between; if `switch` dropped a
        // register the ABI makes it responsible for, these would come back
        // changed and the corruption would surface in an unrelated caller.
        // `clobber_abi("C")` requires explicit output registers, so the two
        // values come back in rax/rdx rather than compiler-chosen ones.
        let (a, b): (u64, u64);
        unsafe {
            core::arch::asm!(
                "mov r12, 0x1111",
                "mov r13, 0x2222",
                "call {switch}",
                "mov rax, r12",
                "mov rdx, r13",
                switch = sym context::switch,
                out("rax") a,
                out("rdx") b,
                in("rdi") &raw mut here,
                in("rsi") child,
                clobber_abi("C"),
            );
        }
        assert_eq!(SWITCH_CHILD_RAN.load(Ordering::SeqCst), 1, "the child never ran");
        assert_eq!(a, 0x1111, "r12 was not preserved across the switch");
        assert_eq!(b, 0x2222, "r13 was not preserved across the switch");
    }

    static SPAWN_LOG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    extern "C" fn sched_worker(arg: u64) -> ! {
        use core::sync::atomic::Ordering;
        // Each worker sets its own bit, so the assertion can tell "all ran"
        // from "one ran three times".
        SPAWN_LOG.fetch_or(1u64 << arg, Ordering::SeqCst);
        crate::sched::exit_current();
    }

    extern "C" fn sched_yielder(arg: u64) -> ! {
        use core::sync::atomic::Ordering;
        // Yields back before finishing, so the test exercises a thread being
        // requeued and resumed rather than only run-to-completion.
        SPAWN_LOG.fetch_or(1u64 << arg, Ordering::SeqCst);
        crate::sched::yield_now();
        SPAWN_LOG.fetch_or(1u64 << (arg + 8), Ordering::SeqCst);
        crate::sched::exit_current();
    }

    #[test_case]
    fn scheduler_runs_every_spawned_thread_exactly_once() {
        use core::sync::atomic::Ordering;
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        SPAWN_LOG.store(0, Ordering::SeqCst);

        for i in 0..3u64 {
            crate::sched::spawn_kernel(sched_worker, i, Priority::Normal);
        }
        // A wait rather than a fixed number of yields: a worker may be stolen
        // and run on another CPU, in which case this thread's yields return
        // immediately without having run anything.
        wait_until(|| SPAWN_LOG.load(Ordering::SeqCst) == 0b111, WAIT_BUDGET);

        assert_eq!(
            SPAWN_LOG.load(Ordering::SeqCst),
            0b111,
            "not every spawned thread ran, or one ran twice"
        );
    }

    #[test_case]
    fn a_yielding_thread_is_requeued_and_resumed() {
        use core::sync::atomic::Ordering;
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        SPAWN_LOG.store(0, Ordering::SeqCst);

        crate::sched::spawn_kernel(sched_yielder, 0, Priority::Normal);
        wait_until(|| SPAWN_LOG.load(Ordering::SeqCst) & (1 << 8) != 0, WAIT_BUDGET);

        // Bit 0 is "started", bit 8 is "resumed after yielding". A scheduler
        // that dropped the thread on yield would set only the first.
        assert_eq!(SPAWN_LOG.load(Ordering::SeqCst) & 1, 1, "the thread never started");
        assert_eq!(
            SPAWN_LOG.load(Ordering::SeqCst) & (1 << 8),
            1 << 8,
            "a yielding thread was never resumed"
        );
    }

    #[test_case]
    fn exited_threads_are_reaped_so_their_stacks_are_freed() {
        use qunix_sched::Priority;

        use core::sync::atomic::Ordering;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        SPAWN_LOG.store(0, Ordering::SeqCst);

        let before = quiesce();
        let mut ids = [qunix_sched::ThreadId(0); 4];
        for (i, slot) in ids.iter_mut().enumerate() {
            *slot = crate::sched::spawn_kernel(sched_worker, i as u64, Priority::Normal);
        }
        // "The count grew by four" is no longer a fact the spawner can observe:
        // an application processor may have stolen, run and reaped one before
        // this line. That each thread really existed is established by its bit
        // in the log instead.
        wait_until(|| SPAWN_LOG.load(Ordering::SeqCst) == 0b1111, WAIT_BUDGET);
        assert_eq!(SPAWN_LOG.load(Ordering::SeqCst), 0b1111, "not every thread ran");

        // The negative direction, per thread rather than in aggregate: an
        // exited thread must actually leave the table. A scheduler that only
        // marked them would grow without bound and leak a 16 KiB stack each.
        for id in ids {
            assert!(
                wait_until_reaped(id),
                "{id:?} exited but was never reaped; its stack is still allocated"
            );
        }
        assert_eq!(
            quiesce(),
            before,
            "the thread table did not return to its size before the spawns"
        );
    }

    #[test_case]
    fn yield_without_other_threads_returns_rather_than_hanging() {
        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        // Drain first: the harness shares one scheduler with `kmain`, which has
        // already spawned threads, so "nothing else runnable" has to be
        // established rather than assumed. Without this the test passes even
        // when `yield_now` switched away and came back, which is the case the
        // name says is absent.
        wait_until(|| crate::sched::runnable_count() == 0, WAIT_BUDGET);
        assert_eq!(crate::sched::runnable_count(), 0, "could not reach an empty run queue");

        // Preemption off, so a switch can only come from `yield_now` itself.
        let was = crate::sched::set_preemption(false);
        for _ in 0..3 {
            crate::sched::yield_now();
        }
        crate::sched::set_preemption(was);
        assert_eq!(crate::sched::current_id(), qunix_sched::ThreadId(0));
        // The property the name claims: with nothing runnable, no switch
        // happened at all. `current_id` alone is also true of a thread that
        // switched away and came straight back.
        assert_eq!(
            crate::sched::runnable_count(),
            0,
            "yielding with an empty queue queued something"
        );
    }

    static SPIN_RAN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static SPIN_STARTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static SPIN_STOP: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);

    /// Never calls `yield_now`, so nothing but a timer tick can take the CPU
    /// from it.
    ///
    /// It does watch a stop flag, which is not a weakening of the test: the
    /// in-QEMU harness runs every test against one kernel and one scheduler, so
    /// a thread that truly never terminates is inherited by every later test.
    /// An earlier version of this omitted the flag and hung the next test.
    extern "C" fn spinner(arg: u64) -> ! {
        use core::sync::atomic::Ordering;
        SPIN_STARTED.fetch_or(1u64 << arg, Ordering::SeqCst);
        while !SPIN_STOP.load(Ordering::SeqCst) {
            SPIN_RAN.fetch_add(1, Ordering::SeqCst);
            core::hint::spin_loop();
        }
        crate::sched::exit_current();
    }

    #[test_case]
    fn the_timer_preempts_a_thread_that_never_yields() {
        use core::sync::atomic::Ordering;
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );
        SPIN_RAN.store(0, Ordering::SeqCst);
        SPIN_STARTED.store(0, Ordering::SeqCst);
        SPIN_STOP.store(false, Ordering::SeqCst);
        let before = quiesce();

        // One spinner per application processor. This thread masks interrupts
        // for the duration, so the bootstrap processor provably dispatches none
        // of them.
        //
        // It used to spawn one *more* spinner than there are processors able to
        // run them, and assert that all of them started -- on the reasoning
        // that the extra one could only run if a tick preempted a resident
        // spinner. That reasoning was wrong twice over, and the test passed
        // anyway:
        //
        // - The extra spinner is queued on the bootstrap processor, which is
        //   masked and never schedules. Reaching it requires *migration*, and
        //   an application processor steals only when its own run queue is
        //   empty at the moment it schedules. Once it holds a spinner and its
        //   own idle thread, it never looks elsewhere again, so the extra
        //   spinner starves regardless of how often anything is preempted. The
        //   assertion was about work stealing, not preemption, and it was false
        //   even about that.
        // - It passed because the old code re-enabled interrupts *before*
        //   asserting, so the bootstrap processor dispatched the extra spinner
        //   itself -- exactly the coincidence the comment above claimed to have
        //   excluded.
        //
        // What proves preemption is asked of the processor it happens on: an
        // application processor running a thread that never yields reaches its
        // idle loop again only by being preempted, because nothing else gives
        // the idle thread back its CPU. `sched::idle_rounds` counts that.
        let cpus = crate::smp::cpu_count() as u64;
        assert!(cpus >= 2, "qemu must be launched with -smp; only {cpus} cpu(s) reported");
        let spinners = cpus - 1;
        assert!(spinners <= 64, "the spinner bitmap is a u64");

        let was = crate::sched::set_preemption(true);
        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        let ticks_before = crate::TICKS.load(Ordering::SeqCst);

        // Masked before the first spawn rather than after the last, for the
        // reason spelled out in `work_stranded_on_a_processor_that_stops_
        // scheduling_is_migrated`: a tick in that window dispatches a spinner
        // on this processor, which is the one thing this test says it has
        // excluded ("the bootstrap processor provably dispatches none of
        // them"). One fewer spinner than processors means this test recovers
        // where the other would hang, so the consequence here is a false
        // premise rather than a stopped suite -- which is worse to leave,
        // because it fails nothing.
        x86_64::instructions::interrupts::disable();
        for i in 0..spinners {
            crate::sched::spawn_kernel(spinner, i, Priority::Normal);
        }

        let all = u64::MAX >> (64 - spinners);
        let started_in_time = wait_by_ticks(|| SPIN_STARTED.load(Ordering::SeqCst) == all, MIGRATION_TICKS);
        assert_ne!(
            started_in_time,
            Err(TickWait::TickSourceStalled),
            "the tick counter stopped advancing; no processor is taking timer interrupts"
        );

        // Sampled only once every spinner is resident, so growth from before
        // they started cannot be mistaken for growth caused by preempting one.
        let idle_before: [u64; 64] = core::array::from_fn(|cpu| {
            if (cpu as u64) < cpus { crate::sched::idle_rounds(cpu as u32) } else { 0 }
        });
        // 20 ticks rather than the 40 the migration test allows: this waits
        // for one preemption on any one processor, which is a single timer
        // interrupt away, where migration additionally needs the stranded
        // thread found and moved.
        let mut preempted = 0u64;
        let _ = wait_by_ticks(
            || {
                preempted = (1..cpus)
                    .filter(|cpu| {
                        crate::sched::idle_rounds(*cpu as u32) > idle_before[*cpu as usize]
                    })
                    .count() as u64;
                preempted > 0
            },
            PREEMPTION_TICKS,
        );
        // Interrupts stay masked through every assertion below and through the
        // store that stops the spinners. Re-enabling here instead left a window
        // this thread could not survive: it adopted the boot context, so it is
        // an *idle*-band thread, and there are now `cpus` Normal-band spinners
        // that never yield. A tick landing anywhere in that window preempts
        // this thread in favour of one of them, and nothing can ever dispatch
        // it again -- the only thing that stops the spinners is the
        // `SPIN_STOP` store it never reaches. The suite then hangs until the
        // harness timeout, which reports no test name and no reason.
        //
        // It survived locally because the window is a few hundred instructions
        // and a tick is ~10 ms. Under TCG on a CI runner the same window is
        // long enough in wall-clock for a tick to land in it. This is the
        // hazard CLAUDE.md states in full: a test that spins rather than yields
        // must mask interrupts *for the duration*, and the duration does not
        // end at the spin -- it ends when the threads that could starve this
        // one have been told to stop.
        //
        // Nothing below needs interrupts. Every assertion reads an atomic or
        // takes an `IrqSpinLock`, both of which are correct while masked, and a
        // failing assertion panics, which halts the machine and does not need
        // to be scheduled.

        assert_eq!(
            SPIN_STARTED.load(Ordering::SeqCst),
            all,
            "only {} of {spinners} spinners started on {} application processors",
            SPIN_STARTED.load(Ordering::SeqCst).count_ones(),
            cpus - 1
        );
        assert!(SPIN_RAN.load(Ordering::SeqCst) > 0, "no spinner made progress");
        // The assertion the test is named for. Every application processor is
        // running a thread that never yields, so reaching the idle loop again
        // has exactly one cause. Nothing here is a proxy: not `TICKS`, which
        // moves whether or not a tick switches anything, and not "control came
        // back to this thread", which says only that this processor is idle.
        assert!(
            preempted > 0,
            "no application processor returned to its idle loop in 20 ticks; a thread that \
             never yields kept its processor, so the timer is not preempting"
        );
        assert!(
            crate::TICKS.load(Ordering::SeqCst) > ticks_before,
            "no timer tick was taken; preemption cannot be what shared the processors"
        );
        // The preempted spinners must still be runnable, not reaped.
        assert!(
            crate::sched::thread_count() > before,
            "the preempted threads disappeared instead of staying runnable"
        );

        // Wind them down, or every later test inherits threads that never end.
        // Still masked, so this store cannot be preempted away from.
        SPIN_STOP.store(true, Ordering::SeqCst);

        // Only now. From here the spinners are all on their way out, so being
        // preempted costs this thread a delay rather than its existence: the
        // run queues drain and an idle-band thread is dispatchable again.
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
        wait_until(|| crate::sched::thread_count() == before, WAIT_BUDGET);
        crate::sched::set_preemption(was);
        assert_eq!(
            crate::sched::thread_count(),
            before,
            "the spinners did not exit; later tests would inherit them"
        );
    }

    #[test_case]
    fn work_stranded_on_a_processor_that_stops_scheduling_is_migrated() {
        use core::sync::atomic::Ordering;
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );
        // Settled *before* the flags are armed, not after. `quiesce` waits for
        // the run queues to drain, and a spinner surviving from an earlier
        // test loops on `!SPIN_STOP` -- so clearing the flag first tells any
        // such thread to keep going and guarantees the wait it is about to do
        // can never succeed. Safe today only because the preceding test
        // asserts its own threads are gone, which is a cross-test coupling
        // nothing states; this ordering does not depend on it.
        let before = quiesce();
        SPIN_STARTED.store(0, Ordering::SeqCst);
        SPIN_STOP.store(false, Ordering::SeqCst);
        // `SPIN_RAN` is deliberately not reset: this test never reads it, and
        // resetting a counter it does not assert on only invites the next
        // reader to wonder which of the two is the mistake.

        // One more spinner than there are processors able to run them, all
        // queued here -- `spawn_kernel` queues locally by design -- and then
        // this processor stops scheduling by masking interrupts. The extra
        // spinner can therefore only run if some other processor comes back for
        // it after it has already taken one.
        //
        // That is the case `take_next` used to fail. An application processor's
        // own idle thread is pushed onto its run queue the moment it switches
        // away, so its local queue is never empty again; a `take_next` that
        // popped whatever was local would take that idle thread and never reach
        // the steal loop. Every processor then cycled between its own resident
        // spinner and its own idle thread while real work sat one queue away,
        // and the extra spinner never started at all.
        //
        // Deterministic, not a race: it failed on every run before the fix and
        // the failure was not timing-dependent, because no amount of waiting
        // makes a CPU that never looks elsewhere look elsewhere.
        let cpus = crate::smp::cpu_count() as u64;
        assert!(cpus >= 2, "qemu must be launched with -smp; only {cpus} cpu(s) reported");
        let spinners = cpus;
        assert!(spinners <= 64, "the spinner bitmap is a u64");

        let was = crate::sched::set_preemption(true);
        let was_enabled = x86_64::instructions::interrupts::are_enabled();

        // Masked *before* the first spawn, not after the last. A tick landing
        // between the final `spawn_kernel` and the mask dispatches a spinner
        // on this very processor -- which defeats the premise the whole test
        // rests on, because a spinner this processor ran itself did not have
        // to be migrated to start. With the old, broken `take_next` that is
        // enough to make every spinner start and the test pass with the bug
        // present: this CPU takes one locally, and the three application
        // processors take the rest on their first schedule, when their queues
        // genuinely are empty.
        //
        // It is also the window that cannot be recovered from. There is one
        // spinner per processor here, so a tick that hands this thread's CPU
        // to a Normal-band spinner leaves this idle-band thread with nowhere
        // to run -- and it is the only thing that ever sets `SPIN_STOP`. The
        // sibling test survives the same window only because it spawns one
        // fewer spinner and some processor therefore runs out of work.
        //
        // `spawn_kernel` is correct under a mask: it takes `SCHED`, which is
        // an `IrqSpinLock`, pushes to a leaf run queue, and sends an IPI --
        // none of which need interrupts enabled on the sending processor, and
        // the application processors still wake and steal.
        x86_64::instructions::interrupts::disable();

        // Sampled before anything is queued. `steal_count` counts since boot
        // and is never reset, so an absolute `> 0` is satisfied by steals that
        // earlier tests caused -- including the steals the *old* ordering
        // performed, since an application processor's first dispatch after
        // boot does find its queue empty. Only a delta says this test caused
        // one.
        let steals_before = crate::sched::steal_count();
        for i in 0..spinners {
            crate::sched::spawn_kernel(spinner, i, Priority::Normal);
        }

        let all = u64::MAX >> (64 - spinners);
        let waited = wait_by_ticks(|| SPIN_STARTED.load(Ordering::SeqCst) == all, MIGRATION_TICKS);

        let started = SPIN_STARTED.load(Ordering::SeqCst);
        let stolen = crate::sched::steal_count() - steals_before;
        SPIN_STOP.store(true, Ordering::SeqCst);
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }

        assert_ne!(
            waited,
            Err(TickWait::TickSourceStalled),
            "the tick counter stopped advancing; no processor is taking timer interrupts, so \
             nothing was learned about migration"
        );
        assert_eq!(
            started,
            all,
            "only {} of {spinners} spinners started; {} were queued on a processor that had \
             stopped scheduling, and no other processor came back for them",
            started.count_ones(),
            spinners - started.count_ones() as u64
        );
        // The mechanism, asserted as an exact count rather than a presence.
        // Every spinner was queued on this processor and this processor
        // dispatched nothing while masked, so every one of them had to be
        // migrated -- which makes "this processor ran one itself" arithmetically
        // impossible rather than merely unlikely. A `> 0` here would be
        // satisfied by a single steal from any earlier test.
        assert!(
            stolen >= spinners,
            "{} spinners started but only {stolen} of {spinners} were stolen ({} steal \
             attempt(s) abandoned on a locked queue); a thread queued on a processor that \
             dispatches nothing can only start by migrating, so any shortfall means this \
             processor ran work it was supposed to be unable to run",
            started.count_ones(),
            crate::sched::steal_contention_count()
        );

        wait_until(|| crate::sched::thread_count() == before, WAIT_BUDGET);
        crate::sched::set_preemption(was);
        assert_eq!(
            crate::sched::thread_count(),
            before,
            "the spinners did not exit; later tests would inherit them"
        );
    }

    #[test_case]
    fn a_parked_thread_is_in_no_run_queue() {
        use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        // The invariant the `Blocked` state exists to create. A parked thread
        // sitting in a run queue is dispatchable, and dispatching it resumes a
        // future at a suspension point it has not returned from.
        //
        // Checked across *every* CPU rather than the local one: the thread is
        // parked elsewhere, and nothing stops another CPU's queue holding its
        // id if a path forgot to skip the requeue.
        static PARKED: AtomicU64 = AtomicU64::new(u64::MAX);
        static RELEASED: AtomicBool = AtomicBool::new(false);

        extern "C" fn sleeper(_: u64) -> ! {
            PARKED.store(crate::sched::current_id().0, Ordering::Release);
            crate::sched::park();
            RELEASED.store(true, Ordering::Release);
            crate::sched::exit_current()
        }

        RELEASED.store(false, Ordering::Release);
        PARKED.store(u64::MAX, Ordering::Release);
        let id = crate::sched::spawn_kernel(sleeper, 0, qunix_sched::Priority::Normal);

        // Waited for rather than assumed. A check that runs before the thread
        // has parked passes for the wrong reason.
        assert!(
            wait_until(|| crate::sched::is_parked(id), WAIT_BUDGET),
            "{id:?} never parked"
        );

        // The same fully-inspecting scan the other two window tests use. This
        // one used to take each lock blocking while its siblings used
        // `try_lock`; three scans of one invariant should not disagree about
        // how they look at it, and the bounded-retry form is the one that
        // cannot pass without having looked.
        assert_eq!(
            queue_holding(id),
            Ok(None),
            "parked {id:?} is queued and can be dispatched, or a run queue could not be \
             inspected"
        );
        assert_eq!(PARKED.load(Ordering::Acquire), id.0);
        assert!(!RELEASED.load(Ordering::Acquire), "the sleeper ran on past its park");

        crate::sched::unpark(id);
        assert!(
            wait_until(|| RELEASED.load(Ordering::Acquire), WAIT_BUDGET),
            "{id:?} was unparked and never resumed"
        );
        assert!(wait_until_reaped(id), "the sleeper never exited");
    }

    #[test_case]
    fn a_thread_woken_while_it_is_still_running_is_not_queued() {
        // The guard `unpark` gates its queueing on, with the race genuinely
        // forced rather than waited for.
        //
        // A thread that has marked itself parked but has not yet reached the
        // context switch is parked *and* still current. A wake arriving in that
        // window must not push it onto a run queue: it is executing, and a
        // second processor that popped it would resume a context that is still
        // in use -- two processors on one kernel stack.
        //
        // Two things make this deterministic rather than lucky, and the first
        // was learned the hard way:
        //
        // - **This processor is masked for the whole experiment**, so it
        //   dispatches nothing and the sleeper can only run by being stolen
        //   onto another processor. An earlier version polled with
        //   `wait_until`, which *yields* -- so this processor dispatched the
        //   sleeper itself, and `park` masks interrupts, so the window was held
        //   on the very processor that needed to observe it. The test then
        //   passed or failed according to which processor won, which is a
        //   statement about scheduling and not about the guard.
        // - **The sleeper asks the scheduler to hold the window open**
        //   (`widen_next_park`). The window is a few instructions wide
        //   otherwise, and CLAUDE.md is explicit that a contention-dependent
        //   path must have its contention created rather than hoped for.
        use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        static ARMED: AtomicBool = AtomicBool::new(false);
        static FINISHED: AtomicBool = AtomicBool::new(false);
        static WHO: AtomicU64 = AtomicU64::new(u64::MAX);

        extern "C" fn sleeper(_: u64) -> ! {
            let me = crate::sched::current_id();
            WHO.store(me.0, Ordering::Release);
            crate::sched::widen_next_park(me);
            ARMED.store(true, Ordering::Release);
            crate::sched::park();
            FINISHED.store(true, Ordering::Release);
            crate::sched::exit_current()
        }

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );
        ARMED.store(false, Ordering::Release);
        FINISHED.store(false, Ordering::Release);
        WHO.store(u64::MAX, Ordering::Release);

        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        // Masked before the spawn, so this processor cannot take the sleeper
        // even for an instant.
        x86_64::instructions::interrupts::disable();
        let id = crate::sched::spawn_kernel(sleeper, 0, qunix_sched::Priority::Normal);

        // Spin-only waits from here: yielding would hand this processor to the
        // sleeper, which is the thing being prevented. `wait_by_ticks` never
        // yields, and ticks keep advancing because the other processors have
        // timers of their own.
        let widened_before = crate::sched::widened_count();
        let armed = wait_by_ticks(|| ARMED.load(Ordering::Acquire), RESCUE_TICKS);
        let in_window = wait_by_ticks(
            || crate::sched::is_parked(id) && crate::sched::is_running_somewhere(id),
            RESCUE_TICKS,
        );

        // The wake under test, delivered while the thread is provably parked
        // and provably still executing on another processor.
        let queued_on = if in_window.is_ok() {
            crate::sched::unpark(id);
            // Every queue inspected, not merely tried. A queue skipped for
            // contention is indistinguishable from a clean one, and the
            // assertion below is a *negative* -- so a skip would let it pass
            // without having looked.
            queue_holding(id)
        } else {
            Ok(None)
        };

        // Restored before any assertion, so a failure does not also leave the
        // suite with a masked processor and a spinner it cannot stop.
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
        // Unconditional: if the window never opened, this is what releases the
        // sleeper so the assertions below can fail cleanly rather than hang.
        crate::sched::unpark(id);

        assert!(armed.is_ok(), "{id:?} never armed the park window: {armed:?}");
        assert_eq!(
            crate::sched::widened_count(),
            widened_before + 1,
            "the park window was never actually held open; whatever this test observed, it was \
             not the forced race it claims to be"
        );
        assert!(
            in_window.is_ok(),
            "never observed {id:?} parked while still running: {in_window:?}. This processor \
             dispatches nothing while masked, so the sleeper must have been stolen and held the \
             window on the processor that stole it"
        );
        assert_eq!(
            queued_on,
            Ok(None),
            "{queued_on:?}: {id:?} was queued while it was still running, or a run queue could \
             not be inspected -- which would let this negative assertion pass without looking. \
             Another processor could pop it and resume a context that is still in use"
        );

        // And the wake must not have been *dropped* in exchange. Declining to
        // queue a running thread is only correct because the thread observes
        // the cleared park itself; a guard that skipped the queueing and the
        // recording alike would satisfy every assertion above and hang here.
        assert!(
            wait_until(|| FINISHED.load(Ordering::Acquire), WAIT_BUDGET),
            "{id:?} never resumed; the wake that arrived while it was running was lost"
        );
        assert!(wait_until_reaped(id), "{id:?} never exited");
    }

    #[test_case]
    fn a_wake_during_the_switch_away_does_not_queue_a_thread_mid_switch() {
        // The window `is_current_anywhere` cannot see, and the one that makes a
        // parked thread pass *through* the handoff rather than skip it.
        //
        // `schedule` stores the incoming thread as this processor's `current`
        // while still holding the scheduler lock, and only switches stacks
        // ~40 lines later. In between, the outgoing parked thread is in no run
        // queue, is parked, and is current nowhere -- so an `unpark` landing
        // there sees nothing stopping it. If it queues the thread, another
        // processor pops it and resumes a context whose stack pointer has not
        // been stored yet: two processors on one kernel stack, and the
        // `assert!(!to_ctx.is_null())` in `schedule` cannot catch it, because
        // nothing ever writes null back into a dispatched thread's slot.
        //
        // Forced the same way as the park-window test, and masked for the same
        // reason: this processor must dispatch nothing, so the sleeper is
        // stolen and holds its switch window somewhere this thread can watch.
        use core::sync::atomic::{AtomicBool, Ordering};
        static ARMED: AtomicBool = AtomicBool::new(false);
        static FINISHED: AtomicBool = AtomicBool::new(false);

        extern "C" fn sleeper(_: u64) -> ! {
            let me = crate::sched::current_id();
            crate::sched::widen_next_switch(me);
            ARMED.store(true, Ordering::Release);
            crate::sched::park();
            FINISHED.store(true, Ordering::Release);
            crate::sched::exit_current()
        }

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(crate::smp::wait_for_all(200_000_000), "application processors did not come online");
        ARMED.store(false, Ordering::Release);
        FINISHED.store(false, Ordering::Release);

        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        let id = crate::sched::spawn_kernel(sleeper, 0, qunix_sched::Priority::Normal);

        let widened_before = crate::sched::widened_count();
        let armed = wait_by_ticks(|| ARMED.load(Ordering::Acquire), RESCUE_TICKS);
        // Spin-only: yielding would hand this processor to the sleeper, and the
        // window would then be held where this thread cannot look.
        // Parked *and* in the window. Without the park requirement, a timer
        // tick that preempted the sleeper between arming and `park` consumes
        // the arming and opens the window for an ordinary `Ready` switch --
        // `unpark` then returns `Noted`, queues nothing, and every assertion
        // below passes with `HANDOFF_PARKED` and `claim_parked_handoff`
        // untouched. Rare, which is worse than common: the test would degrade
        // silently and intermittently rather than fail.
        let in_window = wait_by_ticks(
            || crate::sched::is_parked(id) && crate::sched::thread_in_switch_window() == Some(id),
            RESCUE_TICKS,
        );

        let queued_on = if in_window.is_ok() {
            // The wake under test, delivered while the thread is provably
            // between the lock release and the stack switch.
            crate::sched::unpark(id);
            queue_holding(id)
        } else {
            Ok(None)
        };

        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
        crate::sched::unpark(id);

        assert!(armed.is_ok(), "{id:?} never armed the switch window: {armed:?}");
        assert_eq!(
            crate::sched::widened_count(),
            widened_before + 1,
            "the switch window was never actually held open; whatever this test observed, it \
             was not the forced race it claims to be"
        );
        assert!(in_window.is_ok(), "never observed {id:?} inside its switch window: {in_window:?}");
        assert_eq!(
            queued_on,
            Ok(None),
            "{queued_on:?}: {id:?} was queued while its context was still being saved, or a \
             run queue could not be inspected. Another processor could resume a stack pointer \
             that has not been stored yet"
        );
        // And the wake must still take effect. Declining to queue is only
        // correct because the handoff queues it once the switch completes; a
        // guard that dropped the wake instead would satisfy the assertion above
        // and hang here.
        assert!(
            wait_until(|| FINISHED.load(Ordering::Acquire), WAIT_BUDGET),
            "{id:?} never resumed; the wake delivered during its switch was lost"
        );
        assert!(wait_until_reaped(id), "{id:?} never exited");
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
        use core::sync::atomic::{AtomicBool, Ordering};
        // The lost wakeup, end to end and on real threads. Task 1 proves the
        // state machine; this proves the scheduler consults it, which is the
        // half a unit test cannot reach.
        static DONE: AtomicBool = AtomicBool::new(false);

        extern "C" fn racer(_: u64) -> ! {
            // Wake ourselves first, then park. The park must decline.
            crate::sched::unpark(crate::sched::current_id());
            crate::sched::park();
            DONE.store(true, Ordering::Release);
            crate::sched::exit_current()
        }

        DONE.store(false, Ordering::Release);
        let id = crate::sched::spawn_kernel(racer, 0, qunix_sched::Priority::Normal);
        // Bounded: this hangs the whole suite if the wakeup was lost, and a
        // hang reports nothing. A budget turns it into a failure with a name.
        assert!(
            wait_until(|| DONE.load(Ordering::Acquire), WAIT_BUDGET),
            "{id:?} parked on a wakeup that had already arrived"
        );
        assert!(wait_until_reaped(id), "the racer never exited");
    }

    /// Which CPUs a batch of worker threads observed themselves running on.
    static RAN_ON: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    /// One bit per worker, so "all four ran" is distinguishable from "one ran
    /// four times".
    static RAN_WORKERS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Records where it ran and exits immediately.
    extern "C" fn cpu_worker(arg: u64) -> ! {
        use core::sync::atomic::Ordering;
        RAN_ON.fetch_or(1u64 << qunix_hal_x86_64::percpu::cpu_id(), Ordering::SeqCst);
        RAN_WORKERS.fetch_or(1u64 << arg, Ordering::SeqCst);
        crate::sched::exit_current();
    }

    /// Records where it ran, then refuses to finish until a *second* CPU has
    /// recorded itself.
    ///
    /// The barrier is what forces the outcome rather than hoping for it. A
    /// worker that merely exits leaves the spawning CPU free to run the whole
    /// batch before any other processor has finished waking from `hlt`, and
    /// "they all ran on one CPU" is then a statement about wake-up latency
    /// rather than about the scheduler. Holding each worker here keeps the run
    /// queue non-empty for as long as it takes another CPU to reach it, and one
    /// CPU alone can never satisfy the condition however it interleaves them.
    ///
    /// Bounded, so a kernel where no other CPU ever schedules fails the
    /// assertion instead of hanging the suite.
    extern "C" fn spread_worker(arg: u64) -> ! {
        use core::sync::atomic::Ordering;
        RAN_ON.fetch_or(1u64 << qunix_hal_x86_64::percpu::cpu_id(), Ordering::SeqCst);
        let mut budget = 50_000_000u64;
        while RAN_ON.load(Ordering::SeqCst).count_ones() < 2 && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        RAN_WORKERS.fetch_or(1u64 << arg, Ordering::SeqCst);
        crate::sched::exit_current();
    }

    #[test_case]
    fn work_queued_on_one_cpu_is_stolen_by_another_when_the_owner_never_dispatches() {
        use core::sync::atomic::Ordering;
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );

        // The contention is forced, not hoped for. `spawn_kernel` queues on the
        // calling CPU, and with preemption off this thread never yields, so
        // this CPU provably dispatches none of these threads. Anything that
        // runs them was stolen. A steal path left to the scheduler's discretion
        // is covered on a machine where a steal happens to occur and uncovered
        // on one where it does not, which CLAUDE.md is explicit is not a test.
        let was = crate::sched::set_preemption(false);
        RAN_ON.store(0, Ordering::SeqCst);
        RAN_WORKERS.store(0, Ordering::SeqCst);
        let steals_before = crate::sched::steal_count();
        let me = qunix_hal_x86_64::percpu::cpu_id();

        const WORKERS: u64 = 4;
        for i in 0..WORKERS {
            crate::sched::spawn_kernel(cpu_worker, i, Priority::Normal);
        }

        // Spin rather than yield. Yielding would dispatch them here and destroy
        // the property being tested.
        let all = u64::MAX >> (64 - WORKERS);
        let mut budget = 200_000_000u64;
        while RAN_WORKERS.load(Ordering::SeqCst) != all && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        crate::sched::set_preemption(was);

        assert_eq!(
            RAN_WORKERS.load(Ordering::SeqCst),
            all,
            "{} of {WORKERS} queued threads were never stolen; this cpu ran none of them",
            RAN_WORKERS.load(Ordering::SeqCst).count_ones()
        );
        // The direction the counter exists for: they must have arrived by
        // stealing, not by some other CPU having been handed them directly.
        assert!(
            crate::sched::steal_count() >= steals_before + WORKERS,
            "threads ran without the steal counter moving"
        );
        let cpus = RAN_ON.load(Ordering::SeqCst);
        assert_eq!(
            cpus & (1u64 << me),
            0,
            "cpu {me} ran a thread it never dispatched; preemption was not actually off"
        );
        assert!(cpus != 0, "no cpu was recorded");
    }

    #[test_case]
    fn more_threads_than_cpus_all_run_and_on_more_than_one_cpu() {
        use core::sync::atomic::Ordering;
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        let cpus = crate::smp::cpu_count() as u64;
        assert!(cpus >= 2, "qemu must be launched with -smp; only {cpus} cpu(s) reported");
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );

        RAN_ON.store(0, Ordering::SeqCst);
        RAN_WORKERS.store(0, Ordering::SeqCst);
        // Three times the CPU count, so no single CPU can be running them all
        // concurrently and every CPU has a reason to look for work.
        let workers = cpus * 3;
        assert!(workers <= 64, "the worker bitmap is a u64");
        for i in 0..workers {
            crate::sched::spawn_kernel(spread_worker, i, Priority::Normal);
        }

        let all = u64::MAX >> (64 - workers);
        wait_until(|| RAN_WORKERS.load(Ordering::SeqCst) == all, WAIT_BUDGET);

        assert_eq!(
            RAN_WORKERS.load(Ordering::SeqCst),
            all,
            "not every spawned thread ran, or one ran twice"
        );
        // The point of per-CPU scheduling. One CPU here would mean the
        // application processors are online and idle, which is exactly the
        // state this replaced -- and every other assertion in this test would
        // still hold.
        assert!(
            RAN_ON.load(Ordering::SeqCst).count_ones() >= 2,
            "every thread ran on one cpu ({:#b}); the other processors did not schedule",
            RAN_ON.load(Ordering::SeqCst)
        );
    }

    #[test_case]
    fn every_processor_runs_a_thread_of_its_own() {
        use qunix_hal_x86_64::percpu;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );

        // No CPU may claim to be running the thread another CPU is running.
        // This is the failure D3 named -- one shared `current` slot, two CPUs
        // saving into one context, two threads on one stack -- checked
        // directly rather than through its symptoms. Two CPUs reporting the
        // same id is that state.
        let mut seen: [u64; 64] = [percpu::NO_THREAD; 64];
        let mut count = 0usize;
        for cpu in 0..64u32 {
            let Some(id) = percpu::current_thread_of(cpu) else {
                continue;
            };
            if id == percpu::NO_THREAD {
                continue;
            }
            for other in &seen[..count] {
                assert_ne!(*other, id, "two cpus report running the same thread {id}");
            }
            seen[count] = id;
            count += 1;
        }
        assert_eq!(
            count,
            crate::smp::cpu_count() as usize,
            "only {count} of {} processors have a thread of their own",
            crate::smp::cpu_count()
        );
    }

    #[test_case]
    fn preemption_toggles_and_reports_the_previous_setting() {
        // Deliberately not asserting the *default*: `kmain` enables preemption
        // before `test_main` runs, so the initial value is unobservable from
        // here and an assertion about it would only be restating the `false`
        // this test just wrote.
        let previous = crate::sched::set_preemption(false);
        assert!(!crate::sched::preemption_enabled(), "disabling did not take effect");
        // Returns the *previous* setting, which is what makes it usable for
        // save-and-restore around a critical section.
        assert!(!crate::sched::set_preemption(true), "set_preemption returned the new value");
        assert!(crate::sched::preemption_enabled(), "enabling did not take effect");
        assert!(crate::sched::set_preemption(previous), "the previous setting was not reported");
    }

    #[test_case]
    fn all_application_processors_come_online() {
        crate::frames::init();
        crate::heap::init();

        let total = crate::smp::cpu_count();
        // A vacuous pass is the failure mode here: on a single-CPU guest every
        // assertion below holds for a kernel that cannot start any AP at all.
        assert!(
            total >= 2,
            "qemu must be launched with -smp; only {total} cpu(s) reported"
        );

        let started = crate::smp::start_all();
        assert_eq!(
            started,
            total - 1,
            "start_all skipped a processor; the BSP should be the only one not started"
        );

        assert!(
            crate::smp::wait_for_all(200_000_000),
            "only {}/{} application processors came online",
            crate::smp::online_count(),
            started
        );
    }

    #[test_case]
    fn each_processor_has_its_own_percpu_block() {
        use qunix_hal_x86_64::percpu;
        crate::frames::init();
        crate::heap::init();
        crate::smp::start_all();
        crate::smp::wait_for_all(200_000_000);

        // The whole point of Task 1: one block per CPU, not one shared. If the
        // APs had reused the BSP's block this count would still be 1, and two
        // CPUs would be faulting onto a single IST stack.
        assert_eq!(
            percpu::installed_count(),
            crate::smp::cpu_count(),
            "installed per-CPU blocks do not match the processor count"
        );
        // The BSP must still be reading its own block, not an AP's.
        assert_eq!(percpu::cpu_id(), 0, "the BSP's GS was repointed by AP bring-up");
    }

    #[test_case]
    fn a_shootdown_returns_only_after_every_other_cpu_has_invalidated() {
        use qunix_hal_x86_64::percpu::{self, MAX_CPUS};
        use qunix_hal_x86_64::tlb;

        crate::frames::init();
        crate::heap::init();
        crate::smp::start_all();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );

        let me = percpu::cpu_id();
        let remote = percpu::online_mask() & !(1u64 << me);
        // A vacuous pass is the failure mode here, exactly as it is for the AP
        // bring-up test: with nothing in the remote set, every assertion below
        // is equally true of a shootdown that does nothing at all.
        assert!(
            remote.count_ones() >= 1,
            "no remote cpu is in the shootdown mask; nothing below would be tested"
        );

        let mut before = [0u64; MAX_CPUS as usize];
        for (cpu, slot) in before.iter_mut().enumerate() {
            *slot = tlb::serviced(cpu as u32);
        }

        tlb::shootdown_all();

        // Sampled with nothing at all between it and the return, because this
        // is the assertion that separates a shootdown from a notification.
        // Taking the interrupt on another processor, dispatching the vector and
        // running the handler is hundreds of cycles at best, and more when that
        // processor is halted and has to be woken; an initiator that sent the
        // IPI and returned would be caught here with the counters still at
        // their old values. That is a very strong likelihood rather than a
        // proof — nothing bounds how fast a remote CPU may respond — which is
        // why `outstanding` is checked as well.
        let mut after = [0u64; MAX_CPUS as usize];
        for (cpu, slot) in after.iter_mut().enumerate() {
            *slot = tlb::serviced(cpu as u32);
        }
        let outstanding = tlb::pending();

        for cpu in 0..MAX_CPUS {
            if remote & (1u64 << cpu) == 0 {
                continue;
            }
            assert!(
                after[cpu as usize] > before[cpu as usize],
                "cpu {cpu} had not invalidated when the shootdown returned"
            );
        }
        assert_eq!(outstanding, 0, "the shootdown returned with acknowledgements outstanding");
        // The initiator must never be in its own outstanding set. It waits
        // before it is in any position to service anything, so a mask that
        // included it would hang rather than merely run slowly, and no test
        // after this one would ever report.
        assert_eq!(
            after[me as usize], before[me as usize],
            "the initiator serviced its own shootdown; it would have waited on itself"
        );
    }

    /// Kernel-half scratch address for the cross-CPU shootdown test.
    ///
    /// The kernel half deliberately: entries 256..512 are shared by reference
    /// into every address space, so a mapping made here is walked by every CPU
    /// through the same tables. A lower-half address would be private to
    /// whichever address space made it and no other CPU would resolve it at
    /// all, which would make the test pass for the wrong reason.
    const REMOTE_VA: u64 = 0xffff_9b00_0000_0000;
    const REMOTE_OLD: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    const REMOTE_NEW: u64 = 0x5555_5555_5555_5555;
    /// Neither of the two frame contents, so "not yet read" is distinguishable.
    const NOT_READ: u64 = u64::MAX;

    static REMOTE_CPU: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static REMOTE_FIRST: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(NOT_READ);
    static REMOTE_SECOND: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(NOT_READ);
    static REMOTE_REMAPPED: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);

    /// Caches a translation, waits for it to be shot down, then reads again.
    extern "C" fn stale_reader(_: u64) -> ! {
        use core::sync::atomic::Ordering;
        REMOTE_CPU.store(qunix_hal_x86_64::percpu::cpu_id() as u64, Ordering::SeqCst);
        // Everything this test proves rests on this read. It is what puts the
        // translation in *this* CPU's TLB; without it the second read below is
        // a fresh page-table walk, which finds the new frame whether or not
        // anything was ever invalidated.
        // SAFETY: the initiating CPU mapped this page before spawning us and
        // does not unmap it until after the second read is published.
        let first = unsafe { (REMOTE_VA as *const u64).read_volatile() };
        REMOTE_FIRST.store(first, Ordering::SeqCst);

        // Spins rather than yields: yielding could put this thread back on the
        // initiating CPU, and the whole point is that the second read happens
        // on a different one. Bounded, so a remap that never comes fails the
        // assertions rather than hanging the suite.
        let mut budget = 200_000_000u64;
        while !REMOTE_REMAPPED.load(Ordering::SeqCst) && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        // SAFETY: as above; the page is mapped again by now.
        let second = unsafe { (REMOTE_VA as *const u64).read_volatile() };
        REMOTE_SECOND.store(second, Ordering::SeqCst);
        crate::sched::exit_current();
    }

    #[test_case]
    fn a_remote_cpu_reads_the_new_frame_after_a_shootdown_not_the_old_one() {
        use core::sync::atomic::Ordering;
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};
        use qunix_sched::Priority;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();
        assert!(
            crate::smp::wait_for_all(200_000_000),
            "application processors did not come online"
        );

        // The failure a shootdown exists to prevent, constructed rather than
        // inferred: a CPU that still resolves an address through the frame the
        // initiator has already moved on from. Expressed as a *remap* rather
        // than an unmap because a stale read is observable and a stale fault is
        // not -- a #PF taken in ring 0 panics the kernel, so a test built on
        // one could never report its own result.
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE;
        let old = crate::frames::alloc(0).expect("frame allocation failed");
        let new = crate::frames::alloc(0).expect("frame allocation failed");
        assert_ne!(old, new, "the allocator handed out one frame twice");
        unsafe {
            ((hhdm + old) as *mut u64).write_volatile(REMOTE_OLD);
            ((hhdm + new) as *mut u64).write_volatile(REMOTE_NEW);
            space.map(REMOTE_VA, old, flags, &mut || crate::frames::alloc(0)).expect("map failed");
        }

        REMOTE_CPU.store(0, Ordering::SeqCst);
        REMOTE_FIRST.store(NOT_READ, Ordering::SeqCst);
        REMOTE_SECOND.store(NOT_READ, Ordering::SeqCst);
        REMOTE_REMAPPED.store(false, Ordering::SeqCst);

        // Preemption off and no yielding below, so this CPU provably never
        // dispatches the reader. Whatever runs it is another processor, which
        // is the only arrangement in which "remote" means anything.
        let was = crate::sched::set_preemption(false);
        let me = qunix_hal_x86_64::percpu::cpu_id() as u64;
        crate::sched::spawn_kernel(stale_reader, 0, Priority::Normal);

        let mut budget = 200_000_000u64;
        while REMOTE_FIRST.load(Ordering::SeqCst) == NOT_READ && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        assert_ne!(
            REMOTE_FIRST.load(Ordering::SeqCst),
            NOT_READ,
            "no other processor ran the reader; there is nothing remote to test"
        );
        assert_ne!(REMOTE_CPU.load(Ordering::SeqCst), me, "the reader ran on the initiating cpu");
        assert_eq!(
            REMOTE_FIRST.load(Ordering::SeqCst),
            REMOTE_OLD,
            "the reader did not see the original frame, so it cached nothing"
        );

        // The shootdown is inside `unmap`, and it does not return until the
        // reader's CPU has invalidated. The remap that follows is therefore
        // guaranteed to be what that CPU's next walk finds.
        unsafe {
            space.unmap(REMOTE_VA).expect("unmap failed");
            space.map(REMOTE_VA, new, flags, &mut || crate::frames::alloc(0)).expect("remap failed");
        }
        REMOTE_REMAPPED.store(true, Ordering::SeqCst);

        let mut budget = 200_000_000u64;
        while REMOTE_SECOND.load(Ordering::SeqCst) == NOT_READ && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        crate::sched::set_preemption(was);

        let observed = REMOTE_SECOND.load(Ordering::SeqCst);
        assert_ne!(observed, NOT_READ, "the reader never took its second read");
        assert_eq!(
            observed,
            REMOTE_NEW,
            "cpu {} resolved the old frame after the shootdown returned",
            REMOTE_CPU.load(Ordering::SeqCst)
        );

        wait_until(|| REMOTE_SECOND.load(Ordering::SeqCst) != NOT_READ, WAIT_BUDGET);
        unsafe {
            space.unmap(REMOTE_VA).expect("teardown unmap failed");
            crate::frames::free(old, 0);
            crate::frames::free(new, 0);
        }
    }

    #[test_case]
    fn a_remapped_page_is_not_read_through_the_old_translation() {
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };
        const VA: u64 = 0xffff_9a00_0000_0000;
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE;

        // The same failure as
        // `a_remote_cpu_reads_the_new_frame_after_a_shootdown_not_the_old_one`,
        // on the CPU that performs the remap rather than on another one. Kept
        // beside it because the two invalidations are separate code — `unmap`
        // issues a local `invlpg` and then a shootdown, and a change that broke
        // only the local half would leave the remote test green.
        let old = crate::frames::alloc(0).expect("frame allocation failed");
        let new = crate::frames::alloc(0).expect("frame allocation failed");
        assert_ne!(old, new, "the allocator handed out one frame twice");
        unsafe {
            core::ptr::write_bytes((hhdm + old) as *mut u8, 0xA5, 4096);
            core::ptr::write_bytes((hhdm + new) as *mut u8, 0x5A, 4096);
            space.map(VA, old, flags, &mut || crate::frames::alloc(0)).expect("map failed");
        }

        // Read before the remap, so the translation is actually cached in this
        // CPU's TLB. Without it the assertion below passes on a kernel that
        // never invalidates anything, because the walk would be fresh either
        // way -- which is the shape of test this project has already shipped.
        assert_eq!(unsafe { (VA as *const u8).read_volatile() }, 0xA5, "the old frame was not mapped");

        unsafe {
            space.unmap(VA).expect("unmap failed");
            space.map(VA, new, flags, &mut || crate::frames::alloc(0)).expect("remap failed");
        }
        assert_eq!(
            unsafe { (VA as *const u8).read_volatile() },
            0x5A,
            "the cpu resolved the old frame through a stale translation"
        );

        unsafe {
            space.unmap(VA).expect("teardown unmap failed");
            crate::frames::free(old, 0);
            crate::frames::free(new, 0);
        }
    }

    #[test_case]
    fn a_fresh_address_space_can_be_activated_and_left() {
        use qunix_hal_x86_64::paging::AddressSpace;
        crate::frames::init();
        crate::heap::init();

        let hhdm = crate::boot::hhdm_offset();
        let kernel_root = unsafe { AddressSpace::active(hhdm).root_frame() };

        let mut vm = crate::vmspace::VmSpace::new().expect("no frame for a PML4");
        assert_ne!(vm.root_frame(), kernel_root, "VmSpace reused the kernel's root");
        vm.map_new_page(0x4000_0000, true, false).expect("failed to map a user page");

        // The moment of truth: the instruction after `mov cr3` is fetched
        // through the new tables. Reaching the next line at all proves the
        // kernel half was copied -- a root without it triple-faults here.
        unsafe { vm.activate() };
        assert_eq!(
            unsafe { AddressSpace::active(hhdm).root_frame() },
            vm.root_frame(),
            "cr3 does not hold the address space that was just activated"
        );

        // Back to the kernel's own tables before dropping, since `Drop`
        // refuses to free the root that CR3 still points at.
        let kernel_space = unsafe { AddressSpace::from_root(hhdm, kernel_root) };
        unsafe { kernel_space.activate() };
        drop(vm);
    }

    #[test_case]
    fn user_pages_are_private_to_their_address_space() {
        use qunix_hal_x86_64::paging::AddressSpace;
        crate::frames::init();
        crate::heap::init();

        let hhdm = crate::boot::hhdm_offset();
        let kernel_root = unsafe { AddressSpace::active(hhdm).root_frame() };
        let kernel_space = unsafe { AddressSpace::from_root(hhdm, kernel_root) };

        let mut a = crate::vmspace::VmSpace::new().expect("no frame");
        let mut b = crate::vmspace::VmSpace::new().expect("no frame");
        const VA: u64 = 0x5000_0000;
        let pa_a = a.map_new_page(VA, true, false).expect("map a");
        let pa_b = b.map_new_page(VA, true, false).expect("map b");

        // Same virtual address, different physical frames. If these matched,
        // two processes would share memory at the same address -- which is the
        // whole thing an address space exists to prevent.
        assert_ne!(pa_a, pa_b, "two address spaces mapped one frame at the same VA");

        // Write through A's mapping, then read the same VA under B.
        unsafe { a.activate() };
        unsafe { (VA as *mut u64).write_volatile(0xA11CE) };
        unsafe { b.activate() };
        let seen = unsafe { (VA as *const u64).read_volatile() };
        unsafe { kernel_space.activate() };

        assert_eq!(seen, 0, "B saw A's write; the address spaces are not isolated");
        drop(a);
        drop(b);
    }

    #[test_case]
    fn dropping_an_address_space_returns_every_frame_it_allocated() {
        crate::frames::init();
        crate::heap::init();

        let before = crate::frames::free_bytes();
        let owned;
        {
            let mut vm = crate::vmspace::VmSpace::new().expect("no frame");
            for i in 0..4u64 {
                vm.map_new_page(0x6000_0000 + i * 4096, true, false).expect("map");
            }
            owned = vm.owned_frames();
            assert!(owned >= 5, "expected a root, tables and 4 pages, got {owned}");
        }
        // The negative direction: an address space that leaks its tables shows
        // up here and nowhere else -- the kernel keeps running perfectly well
        // while losing a few frames per process.
        assert_eq!(
            crate::frames::free_bytes(),
            before,
            "dropping a VmSpace leaked {} bytes",
            before - crate::frames::free_bytes()
        );
    }

    #[test_case]
    fn the_init_module_is_a_loadable_elf() {
        // The bootloader supplies this, so the test also covers `limine.conf`
        // and the ESP layout: a module that is missing, misnamed, or built as
        // a flat binary fails here rather than at boot.
        let image = crate::boot::module("init").expect("no init module was loaded");
        assert!(image.len() > 64, "init module is too short to be an ELF");

        let elf = qunix_elf::Elf64::parse(image).expect("init module is not a valid ELF64");
        assert_eq!(elf.entry(), crate::process::USER_TEXT, "init is linked at the wrong address");

        let segments: alloc::vec::Vec<_> = elf.segments().collect();
        assert!(!segments.is_empty(), "init has no loadable segments");
        // The direction that matters: nothing the loader will map may be both
        // writable and executable.
        for segment in &segments {
            assert!(
                !(segment.writable && segment.executable),
                "init segment at {:#x} is both writable and executable",
                segment.vaddr
            );
        }
    }

    #[test_case]
    fn loading_an_elf_produces_a_private_address_space() {
        crate::frames::init();
        crate::heap::init();

        let image = crate::boot::module("init").expect("no init module");
        let a = crate::process::Process::from_elf(image).expect("first load failed");
        let b = crate::process::Process::from_elf(image).expect("second load failed");
        // Two loads of one image must not share page tables, or two processes
        // would see each other's memory.
        assert_ne!(a.root_frame(), b.root_frame(), "two processes share a PML4");
    }

    /// Builds a minimal ELF64 with one PT_LOAD segment at `vaddr`.
    fn elf_with_segment(vaddr: u64, flags: u32, memsz: u64) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec![0xccu8; 64 + 56];
        out[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // little endian
        out[6] = 1; // EV_CURRENT
        out[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        out[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        out[24..32].copy_from_slice(&vaddr.to_le_bytes()); // e_entry
        out[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        out[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        out[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let h = 64;
        out[h..h + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        out[h + 4..h + 8].copy_from_slice(&flags.to_le_bytes());
        out[h + 8..h + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        out[h + 16..h + 24].copy_from_slice(&vaddr.to_le_bytes());
        // Non-zero on purpose. With `p_filesz == 0` the loader's copy loop
        // never executes, so a test built that way cannot observe a write that
        // happens before the address is validated -- which is exactly how an
        // arbitrary-kernel-write hole survived an earlier round.
        out[h + 32..h + 40].copy_from_slice(&8u64.to_le_bytes()); // p_filesz
        out[h + 40..h + 48].copy_from_slice(&memsz.to_le_bytes());
        out
    }

    #[test_case]
    fn a_faulting_user_program_dies_without_taking_the_kernel_with_it() {
        // Declared, not exempted: the harness asserts the kill count moves by
        // exactly this much, so a test that expects one kill and causes two --
        // or none -- still fails.
        crate::testing::expect_process_kills(1);
        use crate::process;

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();

        // The whole point of ring 3. Before the fault handlers distinguished
        // the ring they were entered from, every exception panicked
        // unconditionally -- so an unprivileged program could halt the machine
        // by dereferencing zero, and the kernel's response to a buggy user
        // program was to stop being a kernel.
        //
        // The image is executable and read-only, and its entry lands in the
        // zero fill past `p_filesz`. Zero bytes decode as `add [rax], al`, and
        // `enter_user` clears every GPR, so rax is 0 and the first instruction
        // writes to the null page -- a #PF from ring 3, deterministically.
        let image = elf_with_segment(crate::process::USER_TEXT, 1 | 4, 4096);
        let before = quiesce();
        let id = process::spawn_elf(&image).expect("the faulting image failed to load");

        // Bounded. If the fault panicked instead of killing the process, this
        // test never gets to fail -- the machine is already dead -- so the wait
        // is here to bound the *success* path, not to catch the failure.
        wait_until(|| crate::sched::thread_count() <= before, WAIT_BUDGET);

        assert!(
            crate::sched::thread_count() <= before,
            "{id:?} is still alive after faulting; it was neither killed nor reaped"
        );
    }

    /// Yields and spins until `condition` holds, or the budget runs out.
    ///
    /// Both, deliberately. Work runs on application processors now, so a
    /// condition can be satisfied by a CPU this thread never yields to — and
    /// with an empty local run queue `yield_now` returns immediately, so a loop
    /// that only yielded would burn its whole budget without giving any other
    /// CPU wall-clock time to finish.
    ///
    /// Bounded rather than unbounded because a condition that never holds must
    /// fail the calling test rather than hang the suite.
    pub(crate) fn wait_until(mut condition: impl FnMut() -> bool, budget: u32) -> bool {
        for _ in 0..budget {
            if condition() {
                return true;
            }
            crate::sched::yield_now();
            for _ in 0..256 {
                core::hint::spin_loop();
            }
        }
        condition()
    }

    /// Ticks a test waits for a race window to open, or for a rescuer to act.
    ///
    /// One name for what was six copies of `60`. Distinct from [`WAIT_BUDGET`],
    /// which counts spin iterations rather than ticks.
    pub(crate) const RESCUE_TICKS: u64 = 60;

    /// Ticks allowed for work stranded on a stalled processor to migrate.
    ///
    /// Twice [`PREEMPTION_TICKS`], because migration needs the stranded thread
    /// found and moved as well as a tick to notice.
    const MIGRATION_TICKS: u64 = 40;

    /// Ticks allowed for one preemption on any one processor -- a single timer
    /// interrupt away.
    const PREEMPTION_TICKS: u64 = 20;

    /// Budget for waiting on other CPUs. Generous: under TCG the guest runs
    /// orders of magnitude slower than under KVM, and a budget tuned to one is
    /// a spurious failure on the other.
    pub(crate) const WAIT_BUDGET: u32 = 20_000;

    /// Spins until `condition` holds or `tick_budget` timer ticks have passed.
    ///
    /// Distinct from [`wait_until`], which cannot be used with interrupts
    /// masked: it calls `yield_now`, and a thread that has masked interrupts
    /// specifically to stop being scheduled must not invite a switch.
    ///
    /// Bounded in *ticks* rather than in spin iterations because what these
    /// callers wait for is other processors making progress, and a count of
    /// instructions is a different amount of wall-clock on every host — the
    /// gap between KVM and TCG is orders of magnitude, and a budget tuned to
    /// one is a spurious failure on the other. Ticks still advance while this
    /// processor is masked: every CPU increments `TICKS` from its own timer.
    ///
    /// The instruction backstop is not redundant with that. If the tick source
    /// itself stops — every CPU halted, a broken LAPIC path — a purely
    /// tick-bounded loop never exits, and the harness reports a 120-second
    /// timeout naming no test, which is the worst diagnostic this project can
    /// produce. `Err` says which of the two ran out.
    fn wait_by_ticks(mut condition: impl FnMut() -> bool, tick_budget: u64) -> Result<(), TickWait> {
        let deadline = crate::TICKS.load(core::sync::atomic::Ordering::SeqCst) + tick_budget;
        // Deliberately enormous: it exists to catch a dead tick source, not to
        // bound the wait, so it must not be the limit that fires first on a
        // slow host.
        let mut backstop = 4_000_000_000u64;
        loop {
            if condition() {
                return Ok(());
            }
            if crate::TICKS.load(core::sync::atomic::Ordering::SeqCst) >= deadline {
                return Err(TickWait::Deadline);
            }
            if backstop == 0 {
                return Err(TickWait::TickSourceStalled);
            }
            backstop -= 1;
            core::hint::spin_loop();
        }
    }

    /// Why [`wait_by_ticks`] gave up.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TickWait {
        /// The tick budget elapsed: the condition genuinely did not hold in
        /// time, which is a real failure of whatever was being waited for.
        Deadline,
        /// The tick counter stopped advancing, so the deadline could never be
        /// reached. Nothing was learned about the condition.
        TickSourceStalled,
    }

    /// Which processor's run queue holds `id`, having inspected every one.
    ///
    /// Returns `Err(uninspected)` if any live queue could not be locked. That
    /// distinction is the whole point: the callers assert `Ok(None)`, a
    /// *negative*, and a queue that was skipped rather than examined is
    /// indistinguishable from a clean one. `take_next`'s steal loop holds these
    /// locks routinely -- `STEAL_CONTENDED` exists because that path is the
    /// routine one -- so silently skipping is a real way to miss the finding on
    /// the single occasion it mattered.
    pub(crate) fn queue_holding(id: qunix_sched::ThreadId) -> Result<Option<u32>, u32> {
        use qunix_hal_x86_64::percpu::{MAX_CPUS, run_queue_of};
        let mut found = None;
        let mut uninspected = 0;
        for cpu in 0..MAX_CPUS {
            let Some(queue) = run_queue_of(cpu) else { continue };
            let mut inspected = false;
            // Retried rather than skipped, and bounded rather than blocking: a
            // processor mid-dispatch holds its queue for a few instructions, so
            // waiting briefly is right, but waiting forever would turn a
            // scheduling delay into a hung suite.
            for _ in 0..10_000 {
                if let Some(queue) = queue.try_lock() {
                    if queue.contains(id) {
                        found = Some(cpu);
                    }
                    inspected = true;
                    break;
                }
                core::hint::spin_loop();
            }
            if !inspected {
                uninspected += 1;
            }
        }
        if uninspected > 0 { Err(uninspected) } else { Ok(found) }
    }

    fn wait_until_reaped(id: qunix_sched::ThreadId) -> bool {
        wait_until(|| !crate::sched::thread_id_is_live(id), WAIT_BUDGET)
    }

    /// Waits until the frame allocator is back to `target` bytes free.
    ///
    /// Distinct from [`wait_until_reaped`], and the two are not
    /// interchangeable: `sched::reap` removes a thread from the table *inside*
    /// the scheduler's critical section and drops it — returning its frames —
    /// only after that lock is released, because freeing an address space
    /// enters the frame allocator and holding the scheduler lock across that
    /// orders two locks in a way nothing else in the kernel does. So "gone
    /// from the table" is satisfied strictly earlier than "its frames are
    /// back", and a measurement taken in that window reports a leak that is
    /// really a few instructions of lag.
    ///
    /// That window is what failed CI while passing locally: the reaping
    /// processor was descheduled between the two, and the test sampled 12
    /// frames short. Waiting on the measurement itself rather than on a proxy
    /// for it is the fix, and it does not weaken the assertion — a genuine
    /// leak never satisfies this, so the budget expires and the caller's
    /// assertion reports the shortfall exactly as before.
    fn wait_until_frames_return(target: u64) -> bool {
        wait_until(|| crate::frames::free_bytes() == target, WAIT_BUDGET)
    }

    /// Waits for the scheduler to settle, and returns the resulting thread
    /// count.
    ///
    /// Every test shares one scheduler, and threads now finish and are reaped
    /// on processors this thread never yields to. A baseline sampled while an
    /// earlier test's thread is still being reaped is a baseline of something
    /// else, and the delta built on it fails at random rather than for a
    /// reason. Settled means nothing runnable anywhere and the count unchanged
    /// across two separated samples.
    fn quiesce() -> usize {
        let mut last = usize::MAX;
        for _ in 0..64 {
            wait_until(|| crate::sched::runnable_count() == 0, WAIT_BUDGET / 64);
            let now = crate::sched::thread_count();
            if now == last && crate::sched::runnable_count() == 0 {
                return now;
            }
            last = now;
        }
        last
    }

    #[test_case]
    fn a_process_that_exits_cleanly_gives_back_its_address_space() {
        crate::frames::init();
        crate::heap::init();
        crate::sched::init();

        // The real init image, so this covers the exit path a process actually
        // takes: `Sys::Exit`, which switches the CPU back to the kernel root
        // and stops the thread.
        let image = crate::boot::module("init").expect("no init module");

        // Settled first: a process from an earlier test still being reaped on
        // another processor would move the baseline under this measurement.
        quiesce();
        let before = crate::frames::free_bytes();
        let id = crate::process::spawn_elf(image).expect("init failed to load");
        assert!(
            crate::frames::free_bytes() < before,
            "loading a process consumed no frames; the measurement below proves nothing"
        );
        assert!(wait_until_reaped(id), "{id:?} never exited and was never reaped");

        // The negative direction, and the whole of Deviation D7: an address
        // space nobody owns is never dropped, and the kernel goes on working
        // perfectly while losing a PML4, three page tables and every user page
        // per process. Nothing else in the kernel observes that.
        //
        // Waited for rather than sampled: leaving the thread table and
        // returning the frames are two steps with a lock release between them.
        // See `wait_until_frames_return`.
        let recovered = wait_until_frames_return(before);
        let after = crate::frames::free_bytes();
        assert!(
            recovered,
            "a cleanly-exited process left {} bytes unreclaimed ({after} free, {before} before)",
            before.saturating_sub(after)
        );
    }

    #[test_case]
    fn a_process_killed_by_a_ring_three_fault_gives_back_its_address_space() {
        // Declared, not exempted: the harness asserts the kill count moves by
        // exactly this much, so a test that expects one kill and causes two --
        // or none -- still fails.
        crate::testing::expect_process_kills(1);
        crate::frames::init();
        crate::heap::init();
        crate::sched::init();

        // The other exit path, and the one no `exit` syscall runs through:
        // `syscall::user_fault` kills the process from inside an exception
        // handler. It has to reclaim the same frames as a clean exit, and it
        // reaches `exit_current` by a different route, so a fix applied to only
        // one of the two is exactly the shape of hole this kernel keeps
        // finding.
        //
        // Same construction as the fault test above: executable, read-only, and
        // the entry lands in zero fill, which decodes as `add [rax], al` with
        // rax cleared by `enter_user` — a deterministic #PF on the null page.
        let image = elf_with_segment(crate::process::USER_TEXT, 1 | 4, 4096);

        quiesce();
        let before = crate::frames::free_bytes();
        let id = crate::process::spawn_elf(&image).expect("the faulting image failed to load");
        assert!(
            crate::frames::free_bytes() < before,
            "loading a process consumed no frames; the measurement below proves nothing"
        );
        assert!(wait_until_reaped(id), "{id:?} survived its fault, or was never reaped");

        // Waited for, not sampled — see `wait_until_frames_return`. This is
        // the test that caught the difference: green locally, and 12 frames
        // short on a CI runner that descheduled the reaping processor between
        // the table removal and the drop.
        let recovered = wait_until_frames_return(before);
        let after = crate::frames::free_bytes();
        assert!(
            recovered,
            "a process killed by a ring-3 fault left {} bytes unreclaimed \
             ({after} free, {before} before)",
            before.saturating_sub(after)
        );
    }

    #[test_case]
    fn a_refused_kernel_half_segment_writes_nothing() {
        use crate::process::Process;
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        crate::heap::init();

        // Asserting the error is not enough, and the test that only did so
        // passed both with and against the fix. The hole was that the mapping
        // pass computed its own unchecked page range, so the file's bytes were
        // copied through the HHDM and *then* the load returned a clean
        // `NotUserAddress`. What distinguishes the two is the memory, not the
        // return value.
        //
        // A 4 KiB kernel-half mapping made here on purpose. The obvious canary
        // -- a heap buffer -- cannot observe the bug: the HHDM is mapped with
        // large pages, `translate` walks only to 4 KiB leaves, and the loader
        // bails with `BadAddress` before writing anything. A page this test
        // maps itself is the one shape that reaches the copy.
        const CANARY_VA: u64 = 0xffff_9900_0000_0000;
        let hhdm = crate::boot::hhdm_offset();
        let mut kernel = unsafe { AddressSpace::active(hhdm) };
        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        unsafe {
            kernel
                .map(CANARY_VA, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut || {
                    crate::frames::alloc(0)
                })
                .expect("mapping the canary failed");
            core::ptr::write_bytes((hhdm + pa) as *mut u8, 0xAA, 4096);
        }

        // Mapped before the load, so `VmSpace::new`'s copy of the kernel half
        // carries it and the loader's `is_mapped`/`translate` both resolve.
        let image = elf_with_segment(CANARY_VA, 4, 4096);
        assert!(Process::from_elf(&image).is_err(), "a kernel-half segment was loaded");

        let observed = unsafe { core::slice::from_raw_parts((hhdm + pa) as *const u8, 4096) };
        assert!(
            observed.iter().all(|&b| b == 0xAA),
            "the loader wrote {:#04x} into kernel memory before refusing the segment",
            observed.iter().find(|&&b| b != 0xAA).copied().unwrap_or(0)
        );

        unsafe {
            kernel.unmap(CANARY_VA).expect("unmapping the canary failed");
            crate::frames::free(pa, 0);
        }
    }

    #[test_case]
    fn a_segment_in_the_kernel_half_is_refused() {
        use crate::process::{LoadError, Process};
        crate::frames::init();
        crate::heap::init();

        // The kernel half is shared by reference into every address space, so a
        // "user" segment there would either be copied over live kernel memory
        // or install a user-accessible leaf into tables every CPU walks. Either
        // is an arbitrary kernel write from a file on the ESP.
        let image = elf_with_segment(0xffff_8000_0000_0000, 4, 4096);
        assert_eq!(
            Process::from_elf(&image).err(),
            Some(LoadError::NotUserAddress(0xffff_8000_0000_0000)),
            "a kernel-half segment was loaded"
        );

        // The lowest canonical kernel address. Note the segment carries real
        // file bytes, so if the loader copied before validating, this would
        // overwrite kernel text rather than return an error.
        let image = elf_with_segment(0xffff_ffff_8000_0000, 4, 4096);
        assert!(matches!(
            Process::from_elf(&image).err(),
            Some(LoadError::NotUserAddress(_))
        ));
    }

    #[test_case]
    fn a_write_execute_segment_is_refused() {
        use crate::process::{LoadError, Process};
        crate::frames::init();
        crate::heap::init();

        // PF_X | PF_W on one segment. Some linkers emit this; honouring it puts
        // a writable page in the instruction stream of the first process.
        let image = elf_with_segment(0x40_0000, 1 | 2, 4096);
        assert_eq!(
            Process::from_elf(&image).err(),
            Some(LoadError::WriteExecutePage(0x40_0000)),
            "a writable+executable segment was mapped"
        );

        // The positive direction, so the refusal is not just "everything fails".
        let image = elf_with_segment(0x40_0000, 1, 4096);
        assert!(Process::from_elf(&image).is_ok(), "a plain executable segment was refused");
    }

    #[test_case]
    fn a_non_elf_module_is_refused_rather_than_executed() {
        crate::frames::init();
        crate::heap::init();
        // The negative direction: garbage must fail to load, not produce a
        // process that jumps into whatever the bytes happen to encode.
        let garbage = [0xffu8; 128];
        assert!(crate::process::Process::from_elf(&garbage).is_err());
        assert!(crate::process::Process::from_elf(&[]).is_err());
    }

    #[test_case]
    fn percpu_block_is_reachable_and_reports_its_id() {
        unsafe { qunix_hal_x86_64::percpu::install_bsp() };
        assert_eq!(qunix_hal_x86_64::percpu::cpu_id(), 0);
        assert_eq!(qunix_hal_x86_64::percpu::current().cpu_id, 0);

        // Writing through the block must be visible on the next read, which is
        // what proves `gs:` is pointing at the block rather than at zero.
        // Saved and restored, not zeroed. `kernel_rsp` is the stack the syscall
        // stub lands on; leaving it null means a later trap from ring 3 faults
        // with no stack to report the fault on.
        let saved = qunix_hal_x86_64::percpu::current().kernel_rsp;
        unsafe { qunix_hal_x86_64::percpu::current_mut().kernel_rsp = 0xffff_ffff_dead_0000 };
        assert_eq!(qunix_hal_x86_64::percpu::current().kernel_rsp, 0xffff_ffff_dead_0000);
        unsafe { qunix_hal_x86_64::percpu::current_mut().kernel_rsp = saved };
    }

    #[test_case]
    fn percpu_reinstall_does_not_double_count_the_cpu() {
        use qunix_hal_x86_64::percpu;
        unsafe { percpu::install_bsp() };
        let before = percpu::installed_count();
        // The harness re-installs per test; a count that grew each time would
        // make SMP bring-up wait for CPUs that do not exist.
        unsafe { percpu::install_bsp() };
        unsafe { percpu::install_bsp() };
        assert_eq!(percpu::installed_count(), before, "reinstall counted a new CPU");
        assert!(before >= 1, "the BSP was never counted");
    }

    #[test_case]
    fn percpu_selectors_are_live_and_the_ist_canary_is_written() {
        use qunix_hal_x86_64::{gdt, percpu};
        unsafe { percpu::install_bsp() };
        // The selectors must be the ones actually loaded, not zero: CS is what
        // the CPU is executing under right now.
        use x86_64::instructions::segmentation::Segment;
        let cs = x86_64::instructions::segmentation::CS::get_reg();
        assert_eq!(cs, gdt::kernel_code_selector(), "CS is not this CPU's code selector");
        assert_ne!(gdt::tss_selector().0, 0, "TSS selector is null");
        // `None` would mean install never ran; `Some(false)` a real overflow.
        assert_eq!(gdt::ist_canary_intact(), Some(true));
    }

    #[test_case]
    fn backtrace_walks_at_least_one_kernel_frame() {
        #[inline(never)]
        fn depth_two() -> usize {
            let mut frames = 0;
            crate::panic::walk_frames(|addr| {
                // Kernel code is linked at -2 GiB; anything lower is bogus.
                assert!(addr >= 0xffff_ffff_8000_0000, "implausible return address {addr:#x}");
                frames += 1;
            });
            frames
        }
        #[inline(never)]
        fn depth_one() -> usize {
            depth_two()
        }
        assert!(depth_one() >= 2, "backtrace found fewer than two frames");
    }

    #[test_case]
    fn exception_while_console_is_held_does_not_deadlock() {
        unsafe { qunix_hal_x86_64::percpu::install_bsp() };
        // Hold the console, then take an exception whose handler also prints.
        // Interrupts are maskable and IrqSpinLock handles them; exceptions are
        // NOT, so this is the case that can still self-deadlock.
        let guard = qunix_hal_x86_64::serial::CONSOLE.lock();
        x86_64::instructions::interrupts::int3();
        // Observable, unlike merely reaching this line: the handler printed
        // from inside our critical section, so it must have taken `_print`'s
        // fresh-handle fallback rather than blocking on -- or stealing -- the
        // lock we still hold.
        assert!(
            qunix_hal_x86_64::serial::CONSOLE.try_lock().is_none(),
            "the breakpoint handler released a console lock it did not take"
        );
        drop(guard);
        assert!(
            qunix_hal_x86_64::serial::CONSOLE.try_lock().is_some(),
            "console lock was not released"
        );
    }


    #[test_case]
    fn gdt_installs_expected_kernel_code_selector() {
        use x86_64::instructions::segmentation::{CS, Segment};
        unsafe { qunix_hal_x86_64::percpu::install_bsp() };
        // Entry 0 is the null descriptor, so kernel code lands at index 1 => 0x08.
        assert_eq!(CS::get_reg().0, 0x08);
    }

    #[test_case]
    fn breakpoint_exception_returns_to_caller() {
        unsafe { qunix_hal_x86_64::percpu::install_bsp() };
        // Reaching the line after `int3` proves the IDT is not broken enough to
        // triple-fault, but not that the gate is well-formed. The interrupt
        // flag is: an interrupt gate clears IF on entry and `iret` restores it
        // from the saved RFLAGS, so a gate that returned via anything else, or
        // a handler that left IF where it found it, shows up here.
        let enabled_before = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::int3();
        assert_eq!(
            x86_64::instructions::interrupts::are_enabled(),
            enabled_before,
            "int3 did not restore the interrupt flag"
        );
    }

    #[test_case]
    fn hhdm_offset_is_in_the_higher_half() {
        let offset = crate::boot::hhdm_offset();
        assert!(offset >= 0xffff_8000_0000_0000, "hhdm offset {offset:#x} is not higher-half");
    }

    #[test_case]
    fn memory_map_reports_usable_memory() {
        let mut regions = 0usize;
        let mut total = 0u64;
        for region in crate::boot::usable_regions() {
            regions += 1;
            total += region.len;
            assert!(region.usable);
            assert!(region.len > 0);
        }
        assert!(regions > 0, "no usable memory regions reported");
        // QEMU is launched with 512 MiB; expect at least 256 MiB usable.
        assert!(total >= 256 * 1024 * 1024, "only {total} bytes usable");
    }

    #[test_case]
    fn frame_allocator_hands_out_usable_physical_memory() {
        crate::frames::init();
        let before = crate::frames::free_bytes();
        assert!(before > 64 * 1024 * 1024, "only {before} bytes of frames");

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        assert_eq!(pa % 4096, 0);

        // The frame must be readable and writable through the HHDM.
        let va = (crate::boot::hhdm_offset() + pa) as *mut u64;
        unsafe {
            va.write_volatile(0xdead_beef_cafe_f00d);
            assert_eq!(va.read_volatile(), 0xdead_beef_cafe_f00d);
        }

        unsafe { crate::frames::free(pa, 0) };
        assert_eq!(crate::frames::free_bytes(), before);
    }

    #[test_case]
    fn mapping_a_fresh_frame_makes_it_readable_and_writable() {
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        // A scratch virtual address in an unused part of the higher half.
        const TEST_VA: u64 = 0xffff_9000_0000_0000;

        assert!(space.translate(TEST_VA).is_none(), "test address already mapped");

        unsafe {
            space
                .map(TEST_VA, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut || {
                    crate::frames::alloc(0)
                })
                .expect("map failed");
        }

        assert_eq!(space.translate(TEST_VA), Some(pa));

        let ptr = TEST_VA as *mut u64;
        unsafe {
            ptr.write_volatile(0x1234_5678_9abc_def0);
            assert_eq!(ptr.read_volatile(), 0x1234_5678_9abc_def0);
        }

        let unmapped = unsafe { space.unmap(TEST_VA).expect("unmap failed") };
        assert_eq!(unmapped, pa);
        assert!(space.translate(TEST_VA).is_none());

        unsafe { crate::frames::free(pa, 0) };
    }

    #[test_case]
    fn apic_timer_fires_and_advances_the_tick_counter() {
        use core::sync::atomic::Ordering;

        crate::frames::init();
        crate::heap::init();
        unsafe { qunix_hal_x86_64::percpu::install_bsp() };
        crate::install_local_vectors();
        crate::map_lapic();
        // SAFETY: map_lapic() has just mapped the LAPIC page uncacheable.
        unsafe { qunix_hal_x86_64::apic::init(crate::boot::hhdm_offset()) };
        qunix_hal_x86_64::apic::start_timer(0b1011, 10_000_000);

        // Saved and restored rather than left masked. Every test shares one
        // kernel and one processor state, and this used to end with a bare
        // `disable()`: every later test on this processor then ran with
        // interrupts off, which silently turned off preemption for them.
        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::enable();
        let start = crate::TICKS.load(Ordering::Relaxed);
        // Spin until the timer proves it is firing, with a bounded budget so a
        // dead timer fails the test rather than hanging the suite forever.
        let mut budget = 50_000_000u64;
        while crate::TICKS.load(Ordering::Relaxed) == start && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        if !was_enabled {
            x86_64::instructions::interrupts::disable();
        }

        assert!(budget > 0, "apic timer never fired");
        assert!(crate::TICKS.load(Ordering::Relaxed) > start);
    }

    #[test_case]
    fn unmapping_reclaims_intermediate_page_tables() {
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };
        // A virgin 512 GiB slot, so the mapping has to create a fresh
        // PDPT + PD + PT that pruning must give back.
        //
        // Lower half deliberately. This test used a higher-half address, where
        // pruning is now refused outright -- the tables up there are shared by
        // every address space and are not this one's to free.
        const SCRATCH_VA: u64 = 0x0000_5000_0000_0000;

        let before = crate::frames::free_bytes();
        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        unsafe {
            space
                .map(SCRATCH_VA, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut || {
                    crate::frames::alloc(0)
                })
                .expect("map failed");
        }
        assert_eq!(space.translate(SCRATCH_VA), Some(pa));

        let (unmapped, pruned) = unsafe {
            space
                .unmap_and_prune(SCRATCH_VA, &mut |frame| crate::frames::free(frame, 0))
                .expect("unmap failed")
        };
        assert_eq!(unmapped, pa);
        // A virgin PML4 slot means the mapping built PDPT, PD and PT, so all
        // three must come back; 0 here is the silent-leak case the count exists
        // to expose.
        assert_eq!(pruned, 3, "unmap_and_prune reclaimed {pruned} of 3 tables");
        unsafe { crate::frames::free(pa, 0) };

        // Every frame the mapping consumed -- leaf and all three intermediate
        // tables -- must be back. Plain `unmap` leaks the intermediates, so
        // this would be short by 12 KiB.
        assert_eq!(
            crate::frames::free_bytes(),
            before,
            "page tables leaked: {} bytes unaccounted",
            before - crate::frames::free_bytes()
        );
        assert!(space.translate(SCRATCH_VA).is_none());
    }

    #[test_case]
    fn pruning_refuses_to_free_a_shared_higher_half_table() {
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };
        // A virgin higher-half slot. The tables beneath it are shared by every
        // address space, so emptying them is not this space's to do.
        const SHARED_VA: u64 = 0xffff_9800_0000_0000;

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        unsafe {
            space
                .map(SHARED_VA, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut || {
                    crate::frames::alloc(0)
                })
                .expect("mapping failed");
        }
        let after_map = crate::frames::free_bytes();

        let (got, pruned) = unsafe {
            space
                .unmap_and_prune(SHARED_VA, &mut |frame| crate::frames::free(frame, 0))
                .expect("unmap failed")
        };

        assert_eq!(got, pa);
        // The unmap is honoured; the pruning is not. A non-zero count here
        // means three tables every other address space still walks were handed
        // back to the frame allocator.
        assert_eq!(pruned, 0, "pruned {pruned} shared higher-half tables");
        assert_eq!(
            crate::frames::free_bytes(),
            after_map,
            "a shared higher-half table was freed"
        );
        assert!(space.translate(SHARED_VA).is_none(), "the leaf was not unmapped");

        unsafe { crate::frames::free(pa, 0) };
    }

    #[test_case]
    fn kernel_heap_supports_box_and_vec() {
        use alloc::boxed::Box;
        use alloc::vec::Vec;

        crate::frames::init();
        crate::heap::init();

        let boxed = Box::new(0xfeedu32);
        assert_eq!(*boxed, 0xfeed);

        // with_capacity, not new: a 16 KiB layout takes the large-block path
        // either way, but growth by doubling would carve three separate
        // extents (4+8+16 KiB) instead of one.
        let before = crate::heap::bump_remaining();
        let allocated_before = crate::heap::allocated_bytes();
        let mut v: Vec<u64> = Vec::with_capacity(2048);
        for i in 0..2048 {
            v.push(i);
        }
        assert_eq!(v.len(), 2048);
        assert_eq!(v[2047], 2047);
        assert_eq!(v.iter().sum::<u64>(), (0..2048u64).sum::<u64>());

        // 2048 u64s is 16 KiB. The heap must have charged at least that much,
        // whichever path served it.
        //
        // This deliberately does *not* assert that the bump region moved. It
        // used to, and that assertion held only because no earlier test had
        // freed a 16 KiB block: once M1's scheduler began allocating and
        // freeing 16 KiB thread stacks, the large-block free list satisfied
        // this allocation and the bump region correctly stayed put. The old
        // assertion was measuring which test ran first, not the allocator.
        let after = crate::heap::bump_remaining();
        let charged = crate::heap::allocated_bytes() - allocated_before;
        assert!(
            charged >= 16 * 1024,
            "a 16 KiB allocation was charged only {charged} bytes"
        );
        assert!(
            before - after >= 16 * 1024 || before == after,
            "the bump region moved by {} bytes -- neither a fresh carve nor a recycle",
            before - after
        );
    }

    #[test_case]
    fn stats_agree_with_the_individual_accessors() {
        crate::frames::init();
        // `stats` exists to take one lock instead of five; if it drifted from
        // the accessors it would be reporting a different allocator's numbers.
        assert_eq!(crate::frames::stats().free_bytes, crate::frames::free_bytes());
    }

    #[test_case]
    fn no_frame_falls_in_a_reserved_low_memory_window() {
        use alloc::vec::Vec;

        crate::frames::init();
        // The Vec below must come from the heap, not the frame allocator, or
        // collecting the results would perturb what is being measured.
        crate::heap::init();
        // The sub-1 MiB window is the only part of the map where a boundary
        // error hands out firmware-owned memory -- the IVT and BDA below
        // 0x1000, the EBDA and option ROMs above the conventional-memory
        // ceiling. Drain enough frames to reach it if the split is wrong.
        let mut taken: Vec<u64> = Vec::new();
        for _ in 0..512 {
            let Some(pa) = crate::frames::alloc(0) else { break };
            assert!(
                pa >= crate::frames::LOW_USABLE_START
                    && (pa < crate::frames::LOW_USABLE_END || pa >= 0x10_0000),
                "frame {pa:#x} lies in reserved low memory"
            );
            taken.push(pa);
        }
        assert!(!taken.is_empty(), "frame allocator handed out nothing");
        for pa in taken {
            unsafe { crate::frames::free(pa, 0) };
        }
    }
}
