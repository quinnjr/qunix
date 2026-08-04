use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::println;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

pub type HandlerFn = extern "x86-interrupt" fn(InterruptStackFrame);

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();
static mut INITIALISED: bool = false;

/// Installs the IDT on the current CPU. Idempotent.
pub fn init() {
    unsafe {
        if *(&raw const INITIALISED) {
            // Already built; just reload it on this CPU.
            (*(&raw const IDT)).load();
            return;
        }
        let idt = &mut *(&raw mut IDT);
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.general_protection_fault.set_handler_fn(gp_fault_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        INITIALISED = true;
        (*(&raw const IDT)).load();
    }
}

/// Registers a handler for a hardware-interrupt vector.
///
/// # Safety
/// The handler must remain valid for the life of the system.
pub unsafe fn set_handler(vector: u8, handler: HandlerFn) {
    assert!(vector >= 32, "vector {vector} is reserved for exceptions");
    unsafe {
        let idt = &mut *(&raw mut IDT);
        idt[vector].set_handler_fn(handler);
        (*(&raw const IDT)).load();
    }
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
    panic!("double fault at {:#x}", frame.instruction_pointer.as_u64());
}
