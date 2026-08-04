use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

/// Kernel *code* lives at -2 GiB, so return addresses must be at least this.
const KERNEL_TEXT_BASE: u64 = 0xffff_ffff_8000_0000;
/// Kernel *stacks* live in HHDM-mapped RAM, far below the text base, so frame
/// pointers only have to be canonical higher-half addresses.
const HIGHER_HALF: u64 = 0xffff_8000_0000_0000;
const MAX_FRAMES: usize = 32;

/// Walks the frame-pointer chain, calling `visit` with each return address.
///
/// Requires `-C force-frame-pointers=yes`; without frame pointers RBP is a
/// general-purpose register and the chain is meaningless.
pub fn walk_frames(mut visit: impl FnMut(u64)) {
    let mut rbp: u64;
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack)) };

    for _ in 0..MAX_FRAMES {
        // A valid frame pointer is higher-half and 8-byte aligned.
        if rbp < HIGHER_HALF || rbp % 8 != 0 {
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
    println!("\nqunix: PANIC: {info}");
    print_backtrace();
    crate::testing::exit_qemu(crate::testing::ExitCode::Failure);
}
