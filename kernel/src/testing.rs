use qunix_hal_x86_64::{print, println};

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ExitCode {
    Success = 0x10,
    Failure = 0x11,
}

/// QEMU's isa-debug-exit device exits the process with `(value << 1) | 1`.
/// Success therefore surfaces on the host as exit status 33, failure as 35.
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
