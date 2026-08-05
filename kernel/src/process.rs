//! Processes: an address space plus a thread entering ring 3.
//!
//! M1's process is deliberately thin — no file descriptors, no thread groups,
//! no fork. It is the smallest thing that can be *entered*: an address space
//! with code and a stack mapped user-accessible, and a kernel thread that
//! switches CR3 and executes `iretq` into ring 3.

use qunix_elf::{Elf64, ElfError};
use qunix_hal_x86_64::syscall;
use qunix_sched::{Priority, ThreadId};

use crate::vmspace::VmSpace;

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

        for segment in elf.segments() {
            // A segment need not start on a page boundary; the page it lands
            // in does. Mapping from the rounded-down address is what keeps two
            // segments sharing a page from unmapping each other.
            let start = segment.vaddr & !0xfff;
            let end = segment
                .vaddr
                .checked_add(segment.mem_size)
                .ok_or(LoadError::BadAddress)?;
            let pages = (end.next_multiple_of(4096) - start) / 4096;

            for page in 0..pages {
                let va = start + page * 4096;
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
            let start = segment.vaddr & !0xfff;
            let end = segment
                .vaddr
                .checked_add(segment.mem_size)
                .ok_or(LoadError::BadAddress)?;
            for va in (start..end.next_multiple_of(4096)).step_by(4096) {
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
pub enum LoadError {
    Elf(ElfError),
    /// No frame available for the address space or a segment.
    OutOfMemory,
    /// A segment names a virtual range that overflows.
    BadAddress,
    /// Two segments share a page and between them ask for write and execute.
    /// Refused rather than mapped: honouring it is a W^X hole.
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

    // The stack this thread is standing on is where the syscall stub lands when
    // the process traps. It has to be recorded before entering ring 3, because
    // there is no opportunity afterwards -- and a stub that lands on a null
    // stack faults with no stack to report the fault on.
    let mut here = 0u64;
    let rsp = (&raw mut here) as u64;
    // Backed off and 16-aligned so the stub's pushes land below this frame
    // rather than on top of the locals still in use.
    // SAFETY: this thread's stack outlives it -- the scheduler frees it only
    // after the thread exits, and this thread never returns.
    unsafe { syscall::set_kernel_stack((rsp - 512) & !0xf) };
    // SAFETY: this CPU's per-CPU block is installed and its kernel_rsp is set.
    unsafe { crate::syscall::init() };

    let Process { space, entry, stack_top } = process;
    // SAFETY: the address space carries the kernel half, so this code and this
    // stack stay mapped across the CR3 write.
    unsafe { space.activate() };
    // Leaked deliberately. `enter_user` does not return, so no destructor can
    // run here, and dropping the `VmSpace` would free the page tables the
    // process is about to execute on. The frames are reclaimed when the process
    // exits -- which M1 does not implement, and which is recorded as a known
    // limitation rather than pretended away.
    core::mem::forget(space);

    // SAFETY: entry and stack are mapped user-accessible in the space just
    // activated, and this CPU's kernel_rsp is set.
    unsafe { syscall::enter_user(entry, stack_top) }
}

