use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::println;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

pub type HandlerFn = extern "x86-interrupt" fn(InterruptStackFrame);

/// Builds this CPU's IDT into `idt` and loads it.
///
/// Every CPU gets its own table. M0 shared one because there was only ever one
/// CPU; sharing it now would mean `set_handler` mutating a table other CPUs are
/// actively reading, with no way to sequence the write against their fetches.
///
/// Must be called *after* this CPU's GDT and TSS are loaded: the double-fault
/// gate names an IST index, which only means anything once a TSS with a
/// populated `interrupt_stack_table[0]` is live. Called first, a #DF would
/// switch to a zeroed stack pointer and triple-fault. `percpu::finish_install`
/// is what guarantees that order.
///
/// # Safety
/// `idt` must live for as long as this CPU runs — the CPU keeps reading it via
/// IDTR long after this returns — so it must not be moved or dropped. The
/// caller stores it in the per-CPU block for exactly that reason.
pub unsafe fn build_and_load(idt: &mut InterruptDescriptorTable) {
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.general_protection_fault.set_handler_fn(gp_fault_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    // SAFETY: the caller guarantees the table outlives this CPU.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        let idt_static: &'static InterruptDescriptorTable =
            &*(idt as *const InterruptDescriptorTable);
        idt_static.load();
    }
}

/// Registers a handler for a hardware-interrupt vector.
///
/// # Safety
/// This CPU's per-CPU block must be installed. The write targets *this* CPU's
/// table only, so it must be called on every CPU that needs the vector rather
/// than once globally. Interrupts are masked internally for the duration of the
/// descriptor write, so callers need not do so themselves.
pub unsafe fn set_handler(vector: u8, handler: HandlerFn) {
    assert!(vector >= 32, "vector {vector} is reserved for exceptions");
    // A gate is 16 bytes and is written non-atomically. An interrupt arriving
    // mid-write would dispatch through a half-updated descriptor, so mask for
    // the duration. No `lidt` reload is needed: mutating an entry in the table
    // the IDTR already points at is enough.
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let idt = &mut crate::percpu::current_mut().idt;
        idt[vector].set_handler_fn(handler);
    });
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    println!("qunix: breakpoint at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    panic!("invalid opcode at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn gp_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    panic!(
        "general protection fault (code {error_code:#x}) at {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let addr = x86_64::registers::control::Cr2::read();
    panic!(
        "page fault at {addr:?} (code {error_code:?}) from {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _error_code: u64) -> ! {
    // Checked before the panic machinery runs: if the IST stack itself has
    // overflowed, everything below is scribbling into adjacent `.bss`, and
    // saying so is more useful than the fault address. `None` means the canary
    // was never written, which says nothing about an overflow, so it is not
    // reported as one.
    if crate::gdt::ist_canary_intact() == Some(false) {
        panic!(
            "double fault at {:#x} AND the IST stack overflowed - diagnostics unreliable",
            frame.instruction_pointer.as_u64()
        );
    }
    panic!("double fault at {:#x}", frame.instruction_pointer.as_u64());
}
