#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod apic;
pub mod context;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod pci;
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

    /// Answers a TLB shootdown while spinning for a lock.
    ///
    /// The IPI cannot be delivered here -- taking the lock masked interrupts --
    /// and the initiator does not return until this processor acknowledges. So
    /// a CPU that masks and spins for a lock the initiator holds stops the
    /// machine: every processor halted with `IF` clear, no timer, and nothing
    /// left running that could report it.
    ///
    /// Polling the same idempotent function the IPI handler runs is what
    /// breaks that cycle.
    fn service_while_waiting() {
        crate::tlb::service_pending();
    }
}
