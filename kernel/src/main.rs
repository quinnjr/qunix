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
    TICKS.fetch_add(1, Ordering::Relaxed);
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

/// Registers the APIC timer handler. Separate from apic::init so tests can
/// install the handler before enabling interrupts.
pub fn install_timer() {
    unsafe {
        qunix_hal_x86_64::idt::set_handler(qunix_hal_x86_64::apic::TIMER_VECTOR, timer_handler)
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

    install_timer();
    map_lapic();
    // SAFETY: map_lapic() has just mapped the LAPIC page uncacheable at this
    // exact HHDM offset.
    unsafe { qunix_hal_x86_64::apic::init(boot::hhdm_offset()) };
    qunix_hal_x86_64::apic::start_timer(0b1011, 10_000_000);
    x86_64::instructions::interrupts::enable();
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
    // Hand the CPU over so init actually runs before the boot thread parks.
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

    halt_forever();
}

/// The first thread the kernel ever schedules.
extern "C" fn greet(_: u64) -> ! {
    println!("qunix: hello from {:?} on its own stack", sched::current_id());
    sched::exit_current();
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
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
        // Each yield runs one worker to completion and comes back here.
        for _ in 0..8 {
            crate::sched::yield_now();
        }

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
        for _ in 0..8 {
            crate::sched::yield_now();
        }

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

        crate::frames::init();
        crate::heap::init();
        crate::sched::init();

        let before = crate::sched::thread_count();
        for i in 0..4u64 {
            crate::sched::spawn_kernel(sched_worker, i, Priority::Normal);
        }
        assert_eq!(crate::sched::thread_count(), before + 4);

        for _ in 0..12 {
            crate::sched::yield_now();
        }

        // The negative direction: exited threads must actually leave the table.
        // A scheduler that only marked them would grow without bound and leak a
        // 16 KiB stack per thread.
        assert_eq!(
            crate::sched::thread_count(),
            before,
            "exited threads were not reaped; their stacks are still allocated"
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
        for _ in 0..16 {
            if crate::sched::runnable_count() == 0 {
                break;
            }
            crate::sched::yield_now();
        }
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
    static SPIN_STOP: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);

    /// Never calls `yield_now`, so the only thing that can take the CPU from it
    /// is a timer tick.
    ///
    /// It does watch a stop flag, which is not a weakening of the test: the
    /// in-QEMU harness runs every test against one kernel and one scheduler, so
    /// a thread that truly never terminates is inherited by every later test.
    /// An earlier version of this omitted the flag and hung the next test.
    extern "C" fn spinner(_: u64) -> ! {
        use core::sync::atomic::Ordering;
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
        SPIN_RAN.store(0, Ordering::SeqCst);
        SPIN_STOP.store(false, Ordering::SeqCst);
        let before = crate::sched::thread_count();

        let was = crate::sched::set_preemption(true);
        crate::sched::spawn_kernel(spinner, 0, Priority::Normal);

        // Hand the CPU over once. From here the spinner never yields, so only
        // a timer tick can bring control back to this thread.
        let ticks_before = crate::TICKS.load(Ordering::SeqCst);
        crate::sched::yield_now();

        assert!(
            SPIN_RAN.load(Ordering::SeqCst) > 0,
            "the spinner never ran"
        );
        assert!(
            crate::TICKS.load(Ordering::SeqCst) > ticks_before,
            "no timer tick was taken; preemption cannot be what returned control"
        );
        // The spinner is still runnable and must not have been reaped.
        assert!(
            crate::sched::thread_count() > before,
            "the preempted thread disappeared instead of staying runnable"
        );

        // Wind it down, or every later test inherits a thread that never ends.
        SPIN_STOP.store(true, Ordering::SeqCst);
        for _ in 0..16 {
            if crate::sched::thread_count() == before {
                break;
            }
            crate::sched::yield_now();
        }
        crate::sched::set_preemption(was);
        assert_eq!(
            crate::sched::thread_count(),
            before,
            "the spinner did not exit; later tests would inherit it"
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
        let before = crate::sched::thread_count();
        let id = process::spawn_elf(&image).expect("the faulting image failed to load");

        // Bounded. If the fault panicked instead of killing the process, this
        // test never gets to fail -- the machine is already dead -- so the loop
        // is here to bound the *success* path, not to catch the failure.
        for _ in 0..1000 {
            if crate::sched::thread_count() <= before {
                break;
            }
            crate::sched::yield_now();
        }

        assert!(
            crate::sched::thread_count() <= before,
            "{id:?} is still alive after faulting; it was neither killed nor reaped"
        );
    }

    /// Yields until `id` has left the thread table, or the budget runs out.
    ///
    /// Returns whether it did. Bounded rather than unbounded because a process
    /// that never exits must fail the calling test rather than hang the suite,
    /// and because reaping happens on whichever thread runs *next* — so the
    /// loop has to keep giving the CPU away after the process is gone.
    fn wait_until_reaped(id: qunix_sched::ThreadId) -> bool {
        for _ in 0..2000 {
            if crate::sched::thread_id_is_live(id) {
                crate::sched::yield_now();
            } else {
                return true;
            }
        }
        false
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
        let after = crate::frames::free_bytes();
        assert_eq!(
            after, before,
            "a cleanly-exited process leaked {} bytes of address space",
            before - after
        );
    }

    #[test_case]
    fn a_process_killed_by_a_ring_three_fault_gives_back_its_address_space() {
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

        let before = crate::frames::free_bytes();
        let id = crate::process::spawn_elf(&image).expect("the faulting image failed to load");
        assert!(
            crate::frames::free_bytes() < before,
            "loading a process consumed no frames; the measurement below proves nothing"
        );
        assert!(wait_until_reaped(id), "{id:?} survived its fault, or was never reaped");

        let after = crate::frames::free_bytes();
        assert_eq!(
            after, before,
            "a process killed by a ring-3 fault leaked {} bytes of address space",
            before - after
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
        crate::install_timer();
        crate::map_lapic();
        // SAFETY: map_lapic() has just mapped the LAPIC page uncacheable.
        unsafe { qunix_hal_x86_64::apic::init(crate::boot::hhdm_offset()) };
        qunix_hal_x86_64::apic::start_timer(0b1011, 10_000_000);

        x86_64::instructions::interrupts::enable();
        let start = crate::TICKS.load(Ordering::Relaxed);
        // Spin until the timer proves it is firing, with a bounded budget so a
        // dead timer fails the test rather than hanging the suite forever.
        let mut budget = 50_000_000u64;
        while crate::TICKS.load(Ordering::Relaxed) == start && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        x86_64::instructions::interrupts::disable();

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
