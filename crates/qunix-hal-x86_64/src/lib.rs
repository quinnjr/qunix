#![no_std]
#![feature(abi_x86_interrupt)]

pub mod gdt;
pub mod idt;
pub mod paging;
pub mod port;
pub mod serial;

pub struct Irq;

impl qunix_sync::IrqControl for Irq {
    fn disable_and_save() -> bool {
        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        was_enabled
    }

    fn restore(was_enabled: bool) {
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
    }
}
