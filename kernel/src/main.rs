#![no_std]
#![no_main]

mod boot;

use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    qunix_hal_x86_64::serial::init();
    assert!(boot::base_revision_supported(), "limine base revision unsupported");
    println!("qunix: booted");
    halt_forever();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("qunix: PANIC: {info}");
    halt_forever();
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
