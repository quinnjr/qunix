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

/// One past the highest address a user pointer may name.
///
/// The *canonical* lower half, not the start of the kernel's higher half.
/// Everything from here to `0xffff_7fff_ffff_ffff` is the non-canonical hole,
/// and touching it raises #GP -- which this kernel has no handler that can
/// recover from, so a process could halt the machine with a pointer that is
/// neither kernel memory nor its own.
const USER_MAX: u64 = 0x0000_8000_0000_0000;

/// Whether `[ptr, ptr + len)` lies wholly inside the canonical lower half.
///
/// Split out from [`copy_user_slice`] so the bound can be tested without
/// dereferencing anything: `copy_user_slice` validates *and copies*, so a test
/// asserting that a legal address is accepted would fault on it, the address
/// being legal but unmapped. That is the difference this function makes
/// testable, and it is the one the doc below is careful about.
fn user_range_ok(ptr: u64, len: u64) -> bool {
    let Some(end) = ptr.checked_add(len) else { return false };
    ptr < USER_MAX && end <= USER_MAX
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
    if len == 0 {
        return Some(alloc::vec::Vec::new());
    }
    if !user_range_ok(ptr, len) {
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
    fn the_user_range_bound_is_the_canonical_half_not_the_kernel_half() {
        // Tested through `user_range_ok` rather than `copy_user_slice`, because
        // the latter copies: asserting that a legal address is accepted would
        // dereference it, and a legal-but-unmapped address faults.
        //
        // The gap the old higher-half bound left: addresses between the two
        // canonical halves are neither kernel nor user memory, and touching one
        // raises #GP with nothing to recover it. A process could have halted
        // the machine with `write(0x0000_8000_0000_0000, 1)`.
        assert!(!user_range_ok(0x0000_8000_0000_0000, 8), "non-canonical accepted");
        assert!(!user_range_ok(0x0000_9000_0000_0000, 1), "non-canonical accepted");
        assert!(!user_range_ok(0x0000_7fff_ffff_fff0, 64), "range ending in the hole accepted");
        assert!(!user_range_ok(0xffff_8000_0000_0000, 8), "kernel address accepted");
        assert!(!user_range_ok(u64::MAX - 4, 64), "wrapping range accepted");
        // Pinned from below too, so the bound cannot drift downward unnoticed.
        assert!(user_range_ok(0x0000_7fff_ffff_fff8, 8), "the last legal byte was refused");
        assert!(user_range_ok(0x40_0000, 4096), "an ordinary user buffer was refused");
    }

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
