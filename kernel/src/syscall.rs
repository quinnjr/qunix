//! Native syscall dispatch.
//!
//! Everything here runs on a kernel stack, in ring 0, on behalf of a process
//! that is free to have passed anything at all. So every argument that names
//! memory is validated before it is read — see [`copy_user_slice`] — and an
//! unrecognised number is an error return rather than a panic. A panic here
//! would let any process halt the machine.

use qunix_abi::{Errno, Sys};

/// Largest buffer a single `write` will accept.
///
/// Bounded because the length comes from userspace and the copy happens with
/// interrupts enabled on a kernel stack; an unbounded length is a way for a
/// process to keep a CPU in the kernel indefinitely.
const MAX_WRITE: u64 = 4096;

/// Installs the dispatcher on this CPU.
///
/// # Safety
/// This CPU's per-CPU block must be installed and its `kernel_rsp` set.
pub unsafe fn init() {
    unsafe { qunix_hal_x86_64::syscall::init(handle) };
}

/// The dispatcher proper.
///
/// `extern "C"` and taking six values because that is the shape the entry stub
/// calls with. The stub shifts every argument by one register on the way in:
/// the syscall convention and System V's do not line up, and getting that wrong
/// is silent -- the kernel dispatches on whatever happened to be in `rdi`.
extern "C" fn handle(nr: u64, a0: u64, a1: u64, _a2: u64, _a3: u64, _a4: u64) -> i64 {
    let Some(sys) = Sys::from_raw(nr) else {
        // Not a panic. Userspace chooses this number, and a process that asks
        // for syscall 9999 gets an error, not a dead machine.
        return Errno::BadSyscall as i64;
    };

    match sys {
        Sys::Exit => {
            qunix_hal_x86_64::println!("qunix: process exited with code {a0}");
            crate::sched::exit_current();
        }
        Sys::Write => sys_write(a0, a1),
        Sys::Yield => {
            crate::sched::yield_now();
            Errno::Ok as i64
        }
        Sys::GetPid => crate::sched::current_id().0 as i64,
    }
}

/// Writes a user buffer to the console.
///
/// Returns the number of bytes written, or a negative errno.
fn sys_write(ptr: u64, len: u64) -> i64 {
    if len > MAX_WRITE {
        return Errno::BadArgument as i64;
    }
    let Some(bytes) = (unsafe { copy_user_slice(ptr, len) }) else {
        return Errno::BadAddress as i64;
    };
    for byte in bytes.iter() {
        qunix_hal_x86_64::print!("{}", *byte as char);
    }
    len as i64
}

/// Copies `len` bytes from a user address, or `None` if the range is not a
/// plausible user buffer.
///
/// # What this checks, and what it does not
///
/// It rejects the higher half outright: a process passing a kernel address is
/// either confused or attacking, and reading it here would copy kernel memory
/// into a buffer the process then prints. It also rejects a range that wraps.
///
/// It does **not** verify the pages are mapped. A user address that is
/// well-formed but unmapped still faults, and this kernel has no fault handler
/// that can recover — that is the honest limit today, recorded rather than
/// papered over. Making it safe needs a per-thread "expected fault" hook, which
/// is a fault-handling change and not a syscall change.
///
/// # Safety
/// The active address space must be the calling process's.
unsafe fn copy_user_slice(ptr: u64, len: u64) -> Option<alloc::vec::Vec<u8>> {
    /// Lowest address the kernel occupies. Everything at or above it is off
    /// limits to a user pointer.
    const HIGHER_HALF: u64 = 0xffff_8000_0000_0000;

    if len == 0 {
        return Some(alloc::vec::Vec::new());
    }
    let end = ptr.checked_add(len)?;
    if ptr >= HIGHER_HALF || end > HIGHER_HALF {
        return None;
    }

    let mut out = alloc::vec![0u8; len as usize];
    // SAFETY: the range is entirely in the lower half and the caller guarantees
    // the process's address space is active. Still faults if unmapped -- see
    // the note above.
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), len as usize) };
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn a_kernel_pointer_is_refused() {
        // The direction that matters: a process passing a higher-half address
        // must be refused, not served. Serving it copies kernel memory into
        // something the process can print.
        assert!(unsafe { copy_user_slice(0xffff_8000_0000_0000, 8) }.is_none());
        assert!(unsafe { copy_user_slice(0xffff_ffff_8000_0000, 1) }.is_none());
    }

    #[test_case]
    fn a_range_that_wraps_is_refused() {
        assert!(unsafe { copy_user_slice(u64::MAX - 4, 64) }.is_none());
    }

    #[test_case]
    fn a_range_ending_in_the_higher_half_is_refused() {
        // Starts legal, ends kernel-side. Checking only the start would let a
        // process read across the boundary.
        assert!(unsafe { copy_user_slice(0xffff_7fff_ffff_fff0, 32) }.is_none());
    }

    #[test_case]
    fn an_unknown_syscall_number_returns_an_error_rather_than_panicking() {
        assert_eq!(handle(9999, 0, 0, 0, 0, 0), Errno::BadSyscall as i64);
    }

    #[test_case]
    fn an_oversized_write_is_refused() {
        assert_eq!(sys_write(0x1000, MAX_WRITE + 1), Errno::BadArgument as i64);
    }
}
