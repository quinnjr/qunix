//! Native syscall dispatch.
//!
//! Everything here runs on a kernel stack, in ring 0, on behalf of a process
//! that is free to have passed anything at all. So every argument that names
//! memory is validated before it is read — see [`copy_user_slice`] — and an
//! unrecognised number is an error return rather than a panic. A panic here
//! would let any process halt the machine.

use core::sync::atomic::{AtomicU64, Ordering};
use qunix_abi::{Errno, Sys};

// One definition, shared with the mapper. A second copy here drifted from the
// bound `VmSpace::map_user` enforces, and a pointer check that disagrees with
// the mapper is a check that can be walked around.
use crate::vmspace::USER_MAX;

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
    // Faults are the other half of the ring-3 contract. Installing the syscall
    // path without this leaves a process able to halt the machine by faulting
    // instead of by asking.
    qunix_hal_x86_64::idt::set_user_fault_handler(user_fault);
}

/// What a fault raised in ring 3 costs: the process, not the machine.
///
/// Installed by [`init`], which every user thread runs as it starts, alongside
/// the syscall MSRs. Not once: the store is idempotent and every thread writes
/// the same handler, so repeated installation is harmless. Before it existed, a
/// userspace null dereference panicked the kernel — an unprivileged program
/// could halt the machine by dereferencing zero, which is the whole reason
/// ring 3 exists.
/// Processes killed by a fault since boot.
///
/// Counted because the kill is otherwise invisible to anything but a human
/// reading the console. Two things need it. A test that asserts a faulting
/// process died can then assert it died *once*, rather than that the thread
/// count happened to fall -- which is also true of an unrelated thread being
/// reaped. And the test harness can assert that no *other* test killed
/// anything: a fault taken in ring 0 must panic, so if the ring check ever
/// answered "user" unconditionally, kernel threads would start disappearing
/// silently and every existing assertion would still hold.
static PROCESS_KILLS: AtomicU64 = AtomicU64::new(0);

/// Processes killed by a ring-3 fault since boot.
pub fn process_kills() -> u64 {
    PROCESS_KILLS.load(Ordering::Acquire)
}

extern "C" fn user_fault(rip: u64, what: qunix_hal_x86_64::idt::UserFault) -> ! {
    // Before the print and before anything that can fail: `exit_current` never
    // returns, so a bump placed later would be skipped on any path that stops
    // short, and the harness would read the miss as "no kill happened".
    PROCESS_KILLS.fetch_add(1, Ordering::AcqRel);
    qunix_hal_x86_64::println!("qunix: process killed by {} at {rip:#x}", what.as_str());
    // Off the faulting process's page tables first, for the same reason `exit`
    // does it: the thread is about to stop and its tables must not be what the
    // kernel keeps running on.
    // SAFETY: the kernel half maps this code and this stack.
    unsafe { crate::vmspace::activate_kernel_root() };
    crate::sched::exit_current()
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
            // Off the dying process's page tables before the thread stops
            // running. The kernel half is shared and identical, so continuing
            // on them appears to work -- right up to the point that the
            // process's tables are reclaimed, at which point this CPU is
            // executing on freed memory.
            // SAFETY: the kernel half maps this code and this stack, which is
            // exactly what makes the CR3 write survivable mid-syscall.
            unsafe { crate::vmspace::activate_kernel_root() };
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
/// It also verifies that every page in the range is *mapped*, by walking the
/// active address space before it reads anything. Range-checking alone was not
/// enough: `write(0x1000, 1)` names a perfectly legal user address that no
/// process has mapped, and the resulting #PF is taken in **ring 0**, where the
/// ring-3 fault handler does not apply because CS is the kernel's — so the
/// kernel panicked and the machine halted, at the request of an unprivileged
/// program. That is precisely the failure the fault path exists to close, and
/// it survived in the one syscall that takes a pointer.
///
/// There is no TOCTOU window between the walk and the copy today: nothing
/// unmaps a live user address space concurrently, there is no `munmap`, and the
/// only address space teardown happens after its last thread has stopped. When
/// that stops being true this needs a fault-tolerant accessor, not a longer
/// walk.
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

    // SAFETY: the caller guarantees CR3 holds the calling process's tables.
    let mut active =
        unsafe { qunix_hal_x86_64::paging::AddressSpace::active(crate::boot::hhdm_offset()) };
    // Every page the copy will touch, not just the first: a buffer that starts
    // on a mapped page and runs into an unmapped one faults just as fatally.
    // `user_range_ok` already ruled out the overflow that would make this loop
    // wrap.
    let mut page = ptr & !0xfff;
    while page < ptr + len {
        active.translate(page)?;
        page += 4096;
    }

    let mut out = alloc::vec![0u8; len as usize];
    // SAFETY: the range is entirely in the lower half, every page in it is
    // mapped in the active address space, and the caller guarantees that space
    // is the calling process's.
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
    fn a_well_formed_but_unmapped_user_pointer_is_refused_rather_than_faulting() {
        // The distinction the range check alone could not make. This address is
        // canonical, in the lower half, and below `USER_MAX` -- so it passes
        // `user_range_ok` -- but nothing maps it in any address space this
        // kernel builds, including the one these tests run on. Before the walk
        // existed the copy took a #PF in ring 0 and panicked, which is a process
        // halting the machine.
        //
        // Far above any physical address so that a lower-half identity mapping,
        // if the boot tables have one, cannot accidentally satisfy it.
        assert!(unsafe { copy_user_slice(0x7f00_0000_0000, 8) }.is_none());
        assert_eq!(sys_write(0x7f00_0000_0000, 8), Errno::BadAddress as i64);
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
