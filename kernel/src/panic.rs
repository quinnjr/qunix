use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};
use qunix_hal_x86_64::println;

/// Kernel *code* lives at -2 GiB, so return addresses must be at least this.
const KERNEL_TEXT_BASE: u64 = 0xffff_ffff_8000_0000;
/// Kernel *stacks* live in HHDM-mapped RAM, far below the text base, so frame
/// pointers only have to be canonical higher-half addresses.
const HIGHER_HALF: u64 = 0xffff_8000_0000_0000;
const MAX_FRAMES: usize = 32;
/// Widest span a frame pointer may sit above the current stack pointer. The
/// higher half is 128 TiB of mostly-unmapped address space, so a range check
/// alone admits almost any garbage value; anchoring to RSP narrows it to a
/// window that a real stack could plausibly occupy.
const MAX_STACK_SPAN: u64 = 1 << 20;

/// How many panics are currently in flight. A fault taken *inside* the panic
/// path (a corrupt frame chain dereferenced by `walk_frames`, or a print while
/// the console lock is held) would otherwise recurse until the stack is gone —
/// and Limine's stack has no guard page, so that scribbles through RAM instead
/// of trapping.
///
/// A one-shot latch would not actually bound the recursion: every nested panic
/// would keep running the same reporting code, which is precisely the code that
/// just faulted. Counting depth lets each level do strictly less work than the
/// last, ending at doing nothing at all.
static PANIC_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// COM1 base, and the transmit-holding-register-empty bit in its line status
/// register at `base + 5`.
const COM1: u16 = 0x3F8;
const LSR_THRE: u8 = 1 << 5;
/// Bound on the THRE poll. A wedged or absent UART must not turn the nested
/// panic path into a hang.
const THRE_SPIN_BUDGET: u32 = 100_000;

/// Emits `bytes` to COM1 with a bounded poll, touching no lock and no
/// formatting machinery.
///
/// Deliberately duplicates the UART poll that `serial::Uart` already implements:
/// this runs only when the normal console path is what just faulted, so sharing
/// any of its code — the lock, the `Uart` struct, or `format_args!` — would
/// reenter the fault. Everything it needs is inlined here on purpose.
fn emit_raw(bytes: &[u8]) {
    for &byte in bytes {
        let mut budget = THRE_SPIN_BUDGET;
        while budget > 0 && unsafe { qunix_hal_x86_64::port::inb(COM1 + 5) } & LSR_THRE == 0 {
            budget -= 1;
            core::hint::spin_loop();
        }
        unsafe { qunix_hal_x86_64::port::outb(COM1, byte) };
    }
}

/// Walks the frame-pointer chain, calling `visit` with each return address.
///
/// Requires `-C force-frame-pointers=yes`; without frame pointers RBP is a
/// general-purpose register and the chain is meaningless.
///
/// Runs from the panic path, where machine state is by definition untrusted, so
/// every candidate is bounds-checked against the live stack pointer before it
/// is dereferenced.
pub fn walk_frames(mut visit: impl FnMut(u64)) {
    let (mut rbp, rsp): (u64, u64);
    unsafe {
        core::arch::asm!(
            "mov {}, rbp",
            "mov {}, rsp",
            out(reg) rbp,
            out(reg) rsp,
            options(nomem, nostack)
        )
    };

    for _ in 0..MAX_FRAMES {
        // A frame must be 8-byte aligned, higher-half, at or above the current
        // stack pointer, and within a plausible distance of it. Both words of
        // the frame are read, so the upper bound covers `rbp + 16`.
        if rbp % 8 != 0
            || rbp < HIGHER_HALF
            || rbp < rsp
            || rbp.saturating_sub(rsp) > MAX_STACK_SPAN
            || rbp.checked_add(16).is_none()
        {
            break;
        }
        let frame = rbp as *const u64;
        let next_rbp = unsafe { frame.read_volatile() };
        let return_addr = unsafe { frame.add(1).read_volatile() };

        if return_addr < KERNEL_TEXT_BASE {
            break;
        }
        visit(return_addr);

        // The chain must strictly ascend; anything else means it is corrupt.
        if next_rbp <= rbp {
            break;
        }
        rbp = next_rbp;
    }
}

pub fn print_backtrace() {
    println!("backtrace:");
    let mut index = 0;
    walk_frames(|addr| {
        println!("  {index:>2}: {addr:#018x}");
        index += 1;
    });
    if index == 0 {
        println!("  <no frames recovered>");
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    match PANIC_DEPTH.fetch_add(1, Ordering::AcqRel) {
        0 => {}
        1 => {
            // The reporting path below is what faulted. Say so using a fixed
            // byte string and the self-contained emitter, so the harness sees a
            // verdict rather than a timeout.
            emit_raw(b"\nqunix: PANIC WHILE PANICKING\n");
            crate::testing::exit_qemu(crate::testing::ExitCode::Failure);
        }
        // Even `emit_raw` faulted. There is nothing left that is safe to run.
        _ => crate::testing::exit_qemu(crate::testing::ExitCode::Failure),
    }

    // The console lock may be held by whatever we interrupted, and that context
    // will never resume — reclaim it so the message can actually be emitted.
    unsafe { qunix_hal_x86_64::serial::force_unlock() };

    println!("\nqunix: PANIC: {info}");
    print_backtrace();
    crate::testing::exit_qemu(crate::testing::ExitCode::Failure);
}
