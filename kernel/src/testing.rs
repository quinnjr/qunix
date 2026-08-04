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

pub trait Testable {
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        print!("{} ... ", core::any::type_name::<T>());
        self();
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
