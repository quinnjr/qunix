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
/// The three fields are deliberately not "everything observable". They are the
/// state that is *global, sticky, and invisible*: changing it has no immediate
/// effect the changing test would notice, and no later test announces that it
/// is running under the wrong conditions.
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
        }
    }

    /// Fails the run if `self` differs from `before`, naming the field.
    ///
    /// A panic rather than a warning, deliberately. A warning here would be a
    /// line of output nobody reads in a suite that already prints one line per
    /// test, and CLAUDE.md is explicit that a required change should be an
    /// error rather than a note. The test that leaked state is named, because
    /// the test that *fails* from it can be any later one.
    fn assert_restored(&self, before: &Self, name: &str) {
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
    }
}

pub trait Testable {
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        let name = core::any::type_name::<T>();
        print!("{name} ... ");
        let before = MachineState::capture();
        self();
        // After the test body and before "ok" is printed, so a leak is
        // attributed to the test that caused it rather than to whichever test
        // later trips over it.
        MachineState::capture().assert_restored(&before, name);
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
