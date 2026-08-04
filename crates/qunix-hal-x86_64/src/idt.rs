use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::println;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

pub type HandlerFn = extern "x86-interrupt" fn(InterruptStackFrame);

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();
static mut INITIALISED: bool = false;

/// Installs the IDT on the current CPU. Idempotent per CPU — the table is built
/// once and reloaded on every CPU that calls this.
///
/// Must be called after [`crate::gdt::init`]: the double-fault gate sets an IST
/// index, which only means anything once a TSS with a populated
/// `interrupt_stack_table[0]` is loaded. Called first, a #DF would switch to a
/// zeroed stack pointer and triple-fault. `gdt::init` is idempotent in the same
/// per-CPU sense — it loads the GDT, the segment registers and the TSS on every
/// call, not just the first — so this simply calls it rather than relying on
/// the caller's ordering, and a CPU that reaches only `idt::init` still gets
/// the TSS its IST index depends on.
pub fn init() {
    crate::gdt::init();
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
/// [`init`] must already have run on this CPU, and no other context may be
/// concurrently in `init` or `set_handler` — this mutates a shared IDT the CPU
/// is actively reading. Interrupts are masked internally for the duration of
/// the descriptor write, so callers need not do so themselves.
pub unsafe fn set_handler(vector: u8, handler: HandlerFn) {
    assert!(vector >= 32, "vector {vector} is reserved for exceptions");
    // A gate is 16 bytes and is written non-atomically. An interrupt arriving
    // mid-write would dispatch through a half-updated descriptor, so mask for
    // the duration. No `lidt` reload is needed: mutating an entry in the table
    // the IDTR already points at is enough.
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let idt = &mut *(&raw mut IDT);
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
