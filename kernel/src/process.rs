//! Processes: an address space plus a thread entering ring 3.
//!
//! M1's process is deliberately thin — no file descriptors, no thread groups,
//! no fork. It is the smallest thing that can be *entered*: an address space
//! with code and a stack mapped user-accessible, and a kernel thread that
//! switches CR3 and executes `iretq` into ring 3.

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
    /// Maps `code` at [`USER_TEXT`] and a stack below [`USER_STACK_TOP`].
    ///
    /// `code` is raw machine code, not an ELF. The ELF loader parses and then
    /// calls the same mapping primitives; keeping the two apart means the
    /// ring-3 transition can be tested without also testing a parser.
    pub fn from_flat_binary(code: &[u8]) -> Option<Self> {
        let mut space = VmSpace::new()?;
        let hhdm = crate::boot::hhdm_offset();
        let pages = (code.len() as u64).div_ceil(4096).max(1);

        for page in 0..pages {
            let va = USER_TEXT + page * 4096;
            // Mapped writable only long enough to copy the bytes in. The
            // alternative -- a second temporary mapping of the same frame --
            // costs a page table walk and buys nothing, because nothing else
            // can reach this address space until it is activated.
            let pa = space.map_new_page(va, true, true).ok()?;

            let start = (page * 4096) as usize;
            let end = (start + 4096).min(code.len());
            if start < code.len() {
                // Written through the HHDM, not through the user mapping: the
                // user mapping is only reachable once CR3 points here, and it
                // must not, yet.
                // SAFETY: `pa` is a frame this address space owns, mapped in
                // the HHDM like all RAM, and the slice fits in a page.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        code[start..end].as_ptr(),
                        (hhdm + pa) as *mut u8,
                        end - start,
                    )
                };
            }
            // Text is executable and *not* writable from ring 3. A process that
            // can rewrite its own text makes W^X meaningless from the very
            // first program this kernel runs.
            space.set_writable(va, false).ok()?;
        }

        for page in 0..USER_STACK_PAGES {
            let va = USER_STACK_TOP - (page + 1) * 4096;
            // Writable, never executable: an executable stack is the oldest
            // exploit primitive there is.
            space.map_new_page(va, true, false).ok()?;
        }

        Some(Self { space, entry: USER_TEXT, stack_top: USER_STACK_TOP })
    }

    pub fn root_frame(&self) -> u64 {
        self.space.root_frame()
    }
}

/// Spawns a kernel thread that enters `code` in ring 3.
pub fn spawn_user(code: &'static [u8]) -> Option<ThreadId> {
    let process = Process::from_flat_binary(code)?;
    let boxed = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(process));
    Some(crate::sched::spawn_kernel(user_thread_entry, boxed as u64, Priority::Normal))
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

/// The flat binary assembled from `kernel/user/init.s` at build time.
///
/// Not a committed blob: `build.rs` assembles it, so the `.s` is the only
/// source of truth and the two cannot drift apart.
pub static INIT_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/init.bin"));
