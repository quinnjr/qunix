#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    halt_forever();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    halt_forever();
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
