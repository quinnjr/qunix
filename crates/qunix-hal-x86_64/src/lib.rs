#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod apic;
pub mod context;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod percpu;
pub mod port;
pub mod serial;
pub mod syscall;
pub mod tlb;

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
