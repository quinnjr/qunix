#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(crate::testing::runner)]
#![reexport_test_harness_main = "test_main"]

mod boot;
mod testing;

use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    qunix_hal_x86_64::serial::init();
    assert!(boot::base_revision_supported(), "limine base revision unsupported");
    println!("qunix: booted");

    qunix_hal_x86_64::gdt::init();
    println!("qunix: gdt installed");

    #[cfg(test)]
    test_main();

    halt_forever();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("FAILED\nqunix: PANIC: {info}");
    testing::exit_qemu(testing::ExitCode::Failure);
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

#[cfg(test)]
mod tests {
    #[test_case]
    fn harness_runs_at_all() {
        assert_eq!(1 + 1, 2);
    }

    #[test_case]
    fn gdt_installs_expected_kernel_code_selector() {
        use x86_64::instructions::segmentation::{CS, Segment};
        qunix_hal_x86_64::gdt::init();
        // Entry 0 is the null descriptor, so kernel code lands at index 1 => 0x08.
        assert_eq!(CS::get_reg().0, 0x08);
    }
}
