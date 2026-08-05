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
    // Installed here rather than by the kernel, so that every CPU has it by
    // construction. A CPU that came online without it would either triple-fault
    // on the first shootdown or never acknowledge one, and the initiator's wait
    // is unbounded — "install it on each CPU" is exactly the obligation
    // `set_handler` leaves to its callers, and this one cannot be missed.
    idt[crate::tlb::SHOOTDOWN_VECTOR].set_handler_fn(crate::tlb::shootdown_handler);
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

/// Which fault a ring-3 thread took.
///
/// A `repr(u8)` enum rather than a `&str` because the handler crosses an
/// `extern "C"` boundary, and a Rust fat pointer has no C representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UserFault {
    InvalidOpcode,
    GeneralProtection,
    PageFault,
}

impl UserFault {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidOpcode => "invalid opcode",
            Self::GeneralProtection => "general protection fault",
            Self::PageFault => "page fault",
        }
    }
}

/// What the kernel does with a fault raised by ring 3.
///
/// Takes the faulting instruction pointer and which fault it was. It does not
/// return: the faulting thread is finished either way, and there is nothing to
/// resume it into.
pub type UserFaultHandler = extern "C" fn(rip: u64, what: UserFault) -> !;

/// Installed by the kernel. Zero until then.
static USER_FAULT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Installs the policy for faults raised in ring 3.
///
/// Without one, a userspace fault panics — which halts the machine because a
/// user program divided by zero. This is what makes a fault a *process*
/// failure rather than a kernel one.
pub fn set_user_fault_handler(handler: UserFaultHandler) {
    USER_FAULT.store(handler as *const () as usize, core::sync::atomic::Ordering::Release);
}

/// Whether `frame` was pushed by a fault taken in ring 3.
///
/// The saved CS is the faulting code's selector, and its low two bits are the
/// privilege level the CPU was running at. Reading CS *now* would give the
/// kernel's, which is why the check is against the frame.
fn from_user(frame: &InterruptStackFrame) -> bool {
    is_ring_three(frame.code_segment.0)
}

/// Whether a saved CS selector belongs to ring 3.
///
/// Split out from [`from_user`] so the decision is testable: an
/// `InterruptStackFrame` cannot be constructed off the machine, so the check
/// that decides *panic the kernel* versus *kill the process* was reachable only
/// by faulting for real. That left it asserted in one direction only -- there
/// is a test that a ring-3 fault kills the process, and none that a ring-0
/// fault still panics. `fn from_user(_) -> bool { true }` would have kept every
/// test in this repo green while silently reaping kernel threads on kernel
/// faults, which is the "degrades without halting, still passes" failure
/// CLAUDE.md names.
pub(crate) const fn is_ring_three(cs: u16) -> bool {
    // The low two bits of the selector are the CPL the faulting code ran at.
    // Reading CS *now* would give the kernel's, which is why this is asked of
    // the saved frame rather than the register.
    cs & 3 == 3
}

/// Hands a ring-3 fault to the kernel's policy, or returns if none is
/// installed.
///
/// Returning is the caller's cue to fall through to its panic: a fault from
/// ring 3 with no handler is still better reported than swallowed.
fn deliver_user_fault(frame: &InterruptStackFrame, what: UserFault) {
    if !from_user(frame) {
        return;
    }
    // Before anything reads `gs:`. Ring 3 can zero the hidden `GS.base` with
    // `mov gs, ax`, and this path leads to `exit_current` -> `schedule` ->
    // `percpu::set_kernel_stack`, which dereferences `gs:[0x20]`. With a
    // cleared base that read lands at linear address 0x20 *under the faulting
    // process's own page tables* -- an unmapped read that panics the kernel in
    // the handler meant to prevent exactly that, or, if the process arranged a
    // mapping there, a write through a `PerCpu` pointer it chose.
    //
    // The syscall stub has always reloaded the base via `swapgs`; the exception
    // path never did. One of two entry paths, which is the shape of hole this
    // kernel keeps finding.
    // SAFETY: this CPU's per-CPU block was installed during boot, so
    // `KERNEL_GS_BASE` holds its address.
    unsafe { crate::percpu::restore_gs_base() };
    let raw = USER_FAULT.load(core::sync::atomic::Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: `USER_FAULT` only ever holds a `UserFaultHandler` stored by
    // `set_user_fault_handler`.
    let handler: UserFaultHandler = unsafe { core::mem::transmute::<usize, UserFaultHandler>(raw) };
    handler(frame.instruction_pointer.as_u64(), what)
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    println!("qunix: breakpoint at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    deliver_user_fault(&frame, UserFault::InvalidOpcode);
    panic!("invalid opcode at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn gp_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    deliver_user_fault(&frame, UserFault::GeneralProtection);
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
    deliver_user_fault(&frame, UserFault::PageFault);
    panic!(
        "page fault at {addr:?} (code {error_code:?}) from {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _error_code: u64) -> ! {
    // A #DF is deliberately *not* delivered to the user-fault policy: it means
    // the CPU failed to deliver an earlier exception, so the kernel's own fault
    // machinery is already broken and downgrading it to "kill the process"
    // would resume a machine in an unknown state. Panicking is correct here.
    //
    // The GS repair is still required. `ist_canary_intact` reads `gs:`, and a
    // #DF escalated from a ring-3 fault arrives with whatever base ring 3 left
    // -- so without this the handler dereferences an attacker-influenced
    // pointer while diagnosing a fault.
    // SAFETY: this CPU's per-CPU block was installed during boot.
    unsafe { crate::percpu::restore_gs_base() };

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

#[cfg(test)]
mod tests {
    use super::UserFault;

    #[test]
    fn only_ring_three_selectors_are_treated_as_user() {
        use super::is_ring_three;
        // Ring 0: the kernel's own code and data. A fault from these must
        // reach the panic, not the process-kill path.
        assert!(!is_ring_three(0x08), "kernel CS treated as ring 3");
        assert!(!is_ring_three(0x10), "kernel SS treated as ring 3");
        assert!(!is_ring_three(0x00), "the null selector treated as ring 3");
        // Ring 3: the user selectors as `enter_user` and `sysretq` set them,
        // with RPL 3 in the low bits.
        assert!(is_ring_three(0x2b), "user CS not treated as ring 3");
        assert!(is_ring_three(0x33), "user SS not treated as ring 3");
        // RPL 1 and 2 are unused here but are not ring 3, and a `!= 0` test
        // instead of `== 3` would wrongly accept them.
        assert!(!is_ring_three(0x09), "RPL 1 treated as ring 3");
        assert!(!is_ring_three(0x0a), "RPL 2 treated as ring 3");
    }

    #[test]
    fn every_fault_kind_has_a_distinct_name() {
        let all = [UserFault::InvalidOpcode, UserFault::GeneralProtection, UserFault::PageFault];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.as_str(), b.as_str(), "{a:?} and {b:?} report the same name");
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn a_fault_kind_crosses_the_extern_c_boundary_by_discriminant() {
        // `size_of == 1` was the original assertion and it cannot fail: any
        // three-variant fieldless enum is one byte with or without
        // `#[repr(u8)]`, so it asserted nothing about the repr it claimed to
        // pin. The discriminants do depend on it.
        assert_eq!(UserFault::InvalidOpcode as u8, 0);
        assert_eq!(UserFault::GeneralProtection as u8, 1);
        assert_eq!(UserFault::PageFault as u8, 2);
    }

    // A payload would make this a fat value with no C representation, which is
    // the whole reason the handler takes an enum rather than a `&str`.
    const _: () = assert!(core::mem::size_of::<UserFault>() == core::mem::size_of::<u8>());
}
