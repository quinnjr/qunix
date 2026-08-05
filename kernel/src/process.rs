//! Processes: an address space plus a thread entering ring 3.
//!
//! M1's process is deliberately thin — no file descriptors, no thread groups,
//! no fork. It is the smallest thing that can be *entered*: an address space
//! with code and a stack mapped user-accessible, and a kernel thread that
//! switches CR3 and executes `iretq` into ring 3.

use qunix_elf::{Elf64, ElfError};
use qunix_hal_x86_64::syscall;
use qunix_sched::{Priority, ThreadId};

use crate::vmspace::{USER_MAX, USER_MIN, VmSpace};

/// Page range a segment occupies, including the partial pages at each end.
///
/// One function because the mapping pass and the permission pass must agree on
/// it exactly: a page mapped by one and missed by the other keeps the
/// write-and-execute permissions the mapping pass uses to copy the image in.
///
/// The rounding is checked. `next_multiple_of` overflows for an end within
/// 4 KiB of the top of the address space, and the kernel builds without
/// overflow checks in release, so it would wrap to zero -- making the
/// permission pass iterate an empty range and silently leave the page RWX.
fn segment_pages(segment: &qunix_elf::Segment<'_>) -> Result<core::ops::Range<u64>, LoadError> {
    let start = segment.vaddr & !0xfff;
    let end = segment
        .vaddr
        .checked_add(segment.mem_size)
        .and_then(|end| end.checked_next_multiple_of(4096))
        .ok_or(LoadError::BadAddress)?;
    // Refused, not clamped. A segment reaching into the kernel half is not a
    // program this kernel can run, and mapping the part that fits would give it
    // a foothold at an address it chose.
    if end > USER_MAX {
        return Err(LoadError::NotUserAddress(segment.vaddr));
    }
    // The other end, which went unchecked while only `USER_MAX` was enforced. A
    // `PT_LOAD` at `p_vaddr = 0` maps the null page user-writable, which voids
    // the "a null dereference still faults" guarantee `USER_TEXT` documents and
    // hands a program a legal address it can plant a pointer target at.
    if start < USER_MIN {
        return Err(LoadError::NotUserAddress(segment.vaddr));
    }
    Ok(start..end)
}

/// Where a user program's text is placed.
///
/// Well above the null page, so a null dereference in a user program still
/// faults rather than reading its own code.
pub const USER_TEXT: u64 = 0x40_0000;
/// Top of the user stack. Grows down from here.
pub const USER_STACK_TOP: u64 = 0x7fff_0000_0000;
const USER_STACK_PAGES: u64 = 4;

/// A loaded, ready-to-run user program.
pub struct Process {
    space: VmSpace,
    entry: u64,
    stack_top: u64,
}

impl Process {
    /// Loads a static ELF64 executable into a fresh address space.
    ///
    /// Every segment is mapped writable first so the kernel can copy the file
    /// image and zero the `.bss` tail, then remapped to the permissions the
    /// program header asked for. Nothing can reach the address space in
    /// between, because it is not activated until the thread enters it.
    pub fn from_elf(bytes: &[u8]) -> Result<Self, LoadError> {
        let elf = Elf64::parse(bytes).map_err(LoadError::Elf)?;
        let mut space = VmSpace::new().ok_or(LoadError::OutOfMemory)?;
        let hhdm = crate::boot::hhdm_offset();

        // Every segment is validated before *any* memory is touched. An earlier
        // version checked inside the permission pass, which runs after the copy
        // -- so a segment naming the kernel half was written through the HHDM
        // and only then refused, which is an arbitrary kernel write dressed as
        // a clean error return. Validation must precede mutation.
        for segment in elf.segments() {
            segment_pages(&segment)?;
        }

        for segment in elf.segments() {
            let range = segment_pages(&segment)?;
            for va in range.step_by(4096) {
                // Skip a page an earlier segment already mapped, which happens
                // whenever two segments share one page. Mapping it twice would
                // leak the first frame and discard what was already copied.
                if space.is_mapped(va) {
                    continue;
                }
                space.map_new_page(va, true, true).map_err(LoadError::Map)?;
            }

            // Copied through the HHDM: the user mapping is only reachable once
            // CR3 points at this address space, and it must not, yet. The
            // frames were zeroed on allocation, so `.bss` needs no extra work.
            //
            // Translated once per page rather than once per byte. A four-level
            // walk for every byte made a 64 KiB segment ~262,000 dependent
            // loads instead of 16 walks and 16 block copies, and the length is
            // caller-controlled the moment anything but `init` is loaded.
            let mut copied = 0usize;
            while copied < segment.data.len() {
                let va = segment.vaddr + copied as u64;
                let pa = space.translate(va).ok_or(LoadError::BadAddress)?;
                // Stop at the page boundary: the next page is a different
                // frame and needs its own translation.
                let in_page = 4096 - (va & 0xfff) as usize;
                let run = in_page.min(segment.data.len() - copied);
                // SAFETY: `pa` backs `va` in an address space this owns, and
                // `run` stays inside the page `pa` names.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        segment.data[copied..copied + run].as_ptr(),
                        (hhdm + pa) as *mut u8,
                        run,
                    )
                };
                copied += run;
            }
        }

        // Permissions are applied per *page*, not per segment, and only after
        // every byte is in place.
        //
        // Two segments can share a page -- a linker routinely places the tail
        // of `.text` and the head of `.rodata` in one. Applying each segment's
        // permissions in turn lets the last one win, which either strips
        // execute from real instructions or leaves a writable page executable.
        // Neither faults here; the first shows up as a #PF on an instruction
        // that exists, the second is a W^X hole. So the permissions of every
        // segment touching a page are combined first, and the union is applied
        // once.
        let mut pages: alloc::collections::BTreeMap<u64, (bool, bool)> =
            alloc::collections::BTreeMap::new();
        for segment in elf.segments() {
            for va in segment_pages(&segment)?.step_by(4096) {
                let entry = pages.entry(va).or_insert((false, false));
                entry.0 |= segment.writable;
                entry.1 |= segment.executable;
            }
        }
        for (va, (writable, executable)) in pages {
            // A page that ends up both writable and executable is refused
            // rather than mapped. It can only arise from a program whose
            // segments genuinely overlap that way, and silently honouring it
            // would put a W^X hole in the first process the kernel runs.
            if writable && executable {
                return Err(LoadError::WriteExecutePage(va));
            }
            space.set_permissions(va, writable, executable).map_err(LoadError::Map)?;
        }

        for page in 0..USER_STACK_PAGES {
            let va = USER_STACK_TOP - (page + 1) * 4096;
            space.map_new_page(va, true, false).map_err(LoadError::Map)?;
        }

        Ok(Self { space, entry: elf.entry(), stack_top: USER_STACK_TOP })
    }

    pub fn root_frame(&self) -> u64 {
        self.space.root_frame()
    }
}

/// Why a program could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadError {
    Elf(ElfError),
    /// No frame available for the address space or a segment.
    OutOfMemory,
    /// A segment names a virtual range that overflows.
    BadAddress,
    /// A segment reaches at or above [`USER_MAX`], where the kernel half is
    /// shared by reference, or starts below
    /// [`USER_MIN`](crate::vmspace::USER_MIN), where mapping it would make the
    /// null page valid. The payload is the offending `p_vaddr`.
    NotUserAddress(u64),
    /// A page ends up both writable and executable — either from one segment
    /// carrying `PF_W | PF_X`, or from two segments that share the page and
    /// ask for write and execute between them. Refused rather than mapped:
    /// honouring it is a W^X hole. The payload is the page's virtual address.
    WriteExecutePage(u64),
    Map(qunix_hal_x86_64::paging::MapError),
}

/// Loads an ELF64 executable and spawns a thread that enters it.
pub fn spawn_elf(bytes: &[u8]) -> Result<ThreadId, LoadError> {
    Ok(spawn(Process::from_elf(bytes)?))
}

fn spawn(process: Process) -> ThreadId {
    let boxed = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(process));
    crate::sched::spawn_kernel(user_thread_entry, boxed as u64, Priority::Normal)
}

/// Kernel side of a user thread: arms the syscall path, then leaves ring 0.
extern "C" fn user_thread_entry(raw: u64) -> ! {
    // SAFETY: `raw` came from `Box::into_raw` in `spawn_user` and is delivered
    // exactly once, to this thread.
    let process = *unsafe { alloc::boxed::Box::from_raw(raw as *mut Process) };

    // The kernel stack the syscall stub lands on is programmed by the scheduler
    // on every switch, from the incoming thread's own `kernel_stack_top`. It is
    // deliberately *not* set here: this function ran once per process and set
    // it from a local frame, so the second user thread to start overwrote the
    // first thread's slot and both trapped onto one stack. Per-switch is the
    // only placement that survives more than one user thread.
    //
    // A real `assert_ne!`, not `debug_assert_ne!`: CI runs the in-QEMU tests in
    // release too, where a debug assertion compiles out and leaves nothing at
    // all checking this. The failure it catches is a ring-3 trap onto RSP 0,
    // which is unrecoverable and does not name its cause. It runs once per
    // process creation, not on any hot path.
    assert_ne!(
        qunix_hal_x86_64::percpu::current().kernel_rsp,
        0,
        "entered a user thread before the scheduler programmed its kernel stack"
    );
    // SAFETY: this CPU's per-CPU block is installed and its kernel_rsp is set.
    unsafe { crate::syscall::init() };

    let Process { space, entry, stack_top } = process;
    // SAFETY: the address space carries the kernel half, so this code and this
    // stack stay mapped across the CR3 write.
    unsafe { space.activate() };
    // Handed to the scheduler's table, not forgotten. `enter_user` does not
    // return, so this frame cannot own the space to its end, and dropping it
    // here would free the page tables the process is about to execute on. The
    // `Thread` is the one owner that outlives the switch to ring 3: it is
    // already reaped by a *different* thread, after the exiting thread has
    // switched the CPU back to the kernel root. Forgetting it instead leaked
    // the PML4, every intermediate table and every user page for the rest of
    // the boot, once per process death — Execution Deviation D7 in
    // `docs/superpowers/plans/2026-08-04-m1-processes.md`.
    crate::sched::adopt_address_space(space);

    // SAFETY: entry and stack are mapped user-accessible in the space just
    // activated, and this CPU's kernel_rsp is set.
    unsafe { syscall::enter_user(entry, stack_top) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment_at(vaddr: u64, mem_size: u64) -> qunix_elf::Segment<'static> {
        qunix_elf::Segment { vaddr, mem_size, data: &[], writable: false, executable: true }
    }

    #[test_case]
    fn a_segment_below_the_user_floor_is_refused() {
        // The end that went unchecked. A `PT_LOAD` at 0 maps the null page and
        // silently voids the guarantee `USER_TEXT`'s placement documents, so
        // this asserts the *refusal* -- the direction that was missing, not the
        // acceptance that always worked.
        assert_eq!(
            segment_pages(&segment_at(0, 4096)),
            Err(LoadError::NotUserAddress(0)),
            "a segment at the null page was accepted"
        );
        // Rounds *down* to page zero, so checking `p_vaddr` rather than the
        // page it lands in would let this through.
        assert_eq!(
            segment_pages(&segment_at(0x800, 16)),
            Err(LoadError::NotUserAddress(0x800)),
            "a segment inside the null page was accepted"
        );
        assert!(segment_pages(&segment_at(USER_MIN, 16)).is_ok(), "the first legal page was refused");
    }

    #[test_case]
    fn a_segment_reaching_the_kernel_half_is_refused() {
        assert_eq!(
            segment_pages(&segment_at(USER_MAX - 4096, 8192)),
            Err(LoadError::NotUserAddress(USER_MAX - 4096)),
            "a segment crossing into the kernel half was accepted"
        );
    }
}

