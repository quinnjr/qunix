//! Owned user address spaces.
//!
//! [`AddressSpace`] in the HAL is a *borrowed* view of a page table — it does
//! not own the frames it walks and will happily wrap the bootloader's tables.
//! `VmSpace` is the owning counterpart: it allocates a PML4, shares the
//! kernel's higher half by reference, and gives back everything it allocated
//! when it is dropped.
//!
//! # What "shares the kernel half" costs
//!
//! The top-level entries 256..512 are copied from the kernel's own root, so
//! every address space sees the same kernel. That sharing is by *reference* —
//! the tables beneath those entries are not copied and are not owned here. So
//! teardown must free only what lies under entries 0..256. Freeing a shared
//! kernel table would unmap the kernel out from under every other address
//! space, including the one doing the freeing.

use alloc::vec::Vec;
use qunix_hal_x86_64::paging::{AddressSpace, MapError, PageFlags};
use qunix_mm::PAGE_SIZE;

/// One past the highest address a user mapping may name.
///
/// Enforced here rather than at each caller because this is the layer that
/// knows the kernel half is shared *by reference*: entries 256..512 are copied
/// from the kernel's own root, so a "user" mapping above this bound either
/// writes through the kernel's existing tables or installs a user-accessible
/// leaf into tables every other address space and every CPU also walks. The
/// second case is worse than it looks -- `Drop` then frees the frame while that
/// global mapping is still live.
/// Everything from here to `0xffff_7fff_ffff_ffff` is also the non-canonical
/// hole, and touching it raises #GP -- a fault this kernel has no handler that
/// can recover from, so the same bound keeps a process from halting the machine
/// with a pointer that is neither kernel memory nor its own. That is why
/// [`crate::syscall`] validates user pointers against this constant rather than
/// against a bound of its own.
pub const USER_MAX: u64 = 0x0000_8000_0000_0000;

/// Lowest address a user mapping may name.
///
/// The null page stays unmapped so that a null dereference in a user program
/// faults. `USER_TEXT`'s placement documents that guarantee, but placement
/// alone does not enforce it: an ELF with `p_vaddr = 0` would map page zero
/// user-writable and quietly void it.
///
/// Deliberately beside [`USER_MAX`]. The bug this pair closes is that only the
/// upper end was ever checked -- a guard applied to one of two ends is the
/// shape of hole this kernel has already shipped, so both ends live in one
/// place and every caller checks both.
pub const USER_MIN: u64 = 0x1000;

/// The page-table root the kernel booted on.
///
/// Recorded once, because CR3 is not a reliable source for it: a user thread
/// activates its own space and never switches back, so by the time a *second*
/// process is created the "current" root is the first process's. Copying the
/// kernel half from that still works by accident today -- the halves are
/// identical -- but it makes every new address space depend on a page table
/// owned by an unrelated process, and it silently stops working the moment the
/// kernel half is ever modified after boot.
static KERNEL_ROOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Records the root CR3 held at boot, and returns it.
///
/// # Safety
/// Must be called on the kernel's own tables, before anything activates a
/// [`VmSpace`].
pub unsafe fn record_kernel_root() -> u64 {
    let hhdm = crate::boot::hhdm_offset();
    // SAFETY: the caller guarantees CR3 holds the kernel's tables.
    let root = unsafe { AddressSpace::active(hhdm).root_frame() };
    KERNEL_ROOT.store(root, core::sync::atomic::Ordering::Release);
    root
}

/// Switches this CPU back to the kernel's own page tables.
///
/// Called when a process's thread stops running on its address space. Without
/// it the kernel keeps executing on a dead process's tables, which is only
/// survivable while the kernel half is shared and identical.
///
/// # Safety
/// The kernel half must map this code and the current stack, which is what
/// makes the CR3 write survivable. It always does — that is the invariant
/// `copy_kernel_half` exists to maintain.
pub unsafe fn activate_kernel_root() {
    let root = KERNEL_ROOT.load(core::sync::atomic::Ordering::Acquire);
    // Not a silent return. Both callers -- `exit` and the user-fault path --
    // continue as if the CPU had been switched off the dying process's tables,
    // so returning here reinstates exactly the bug this function was added to
    // prevent: the kernel running on page tables that are about to be freed.
    assert_ne!(root, 0, "activate_kernel_root before record_kernel_root");
    let hhdm = crate::boot::hhdm_offset();
    // SAFETY: `root` was the live kernel root when it was recorded, and the
    // kernel never frees it.
    unsafe { AddressSpace::from_root(hhdm, root).activate() };
}

/// Loads `root` into CR3, or the kernel's own root when the incoming thread has
/// no address space of its own.
///
/// Called on every dispatch. That is not belt-and-braces: a CPU that ran a user
/// thread and then switched to a kernel thread kept the process's root in CR3,
/// and nothing noticed, because the kernel half is identical in every root.
/// With one CPU the exit path could guarantee the switch away happened before
/// the tables were freed; with threads moving between CPUs it cannot, so each
/// dispatch states which root it wants and the guarantee becomes structural.
///
/// # Safety
/// The kernel half must map the calling code and stack, which every root this
/// kernel builds does — that is the invariant `copy_kernel_half` maintains.
pub unsafe fn activate_root(root: Option<u64>) {
    let kernel = KERNEL_ROOT.load(core::sync::atomic::Ordering::Acquire);
    assert_ne!(kernel, 0, "activate_root before record_kernel_root");
    let wanted = root.unwrap_or(kernel);
    let hhdm = crate::boot::hhdm_offset();
    // Read before write. Writing CR3 flushes every non-global TLB entry, so
    // rewriting the value already there would cost a full flush on every switch
    // between two kernel threads — which is most of them.
    // SAFETY: reads CR3 only.
    let active = unsafe { AddressSpace::active(hhdm).root_frame() };
    if active == wanted {
        return;
    }
    // SAFETY: `wanted` is either the recorded kernel root, which is never
    // freed, or a root owned by the thread about to run, which the scheduler's
    // table keeps alive for as long as that thread exists.
    unsafe { AddressSpace::from_root(hhdm, wanted).activate() };
}

/// An address space this kernel allocated and is responsible for.
pub struct VmSpace {
    space: AddressSpace,
    root_pa: u64,
    /// Every frame allocated for this address space: the root, the intermediate
    /// tables, and the user pages. Freed in reverse on drop.
    ///
    /// A list rather than a page-table walk at teardown, because a walk cannot
    /// distinguish a frame this address space allocated from one it merely
    /// mapped — and unmapping a shared frame twice is the double-free this
    /// project has already paid for once.
    owned: Vec<u64>,
}

impl VmSpace {
    /// Allocates a fresh address space sharing the current kernel half.
    ///
    /// Returns `None` when no frame is available, rather than panicking:
    /// running out of memory while creating a process is an ordinary failure
    /// the caller must handle, not a kernel bug.
    pub fn new() -> Option<Self> {
        let root_pa = crate::frames::alloc(0)?;
        let hhdm = crate::boot::hhdm_offset();

        // Zeroed first. `frames::alloc` hands back whatever the frame last
        // held, and a PML4 full of stale entries is a page table pointing at
        // memory this address space does not own.
        // SAFETY: the frame was just allocated, is 4 KiB, and is mapped through
        // the HHDM like all RAM.
        unsafe { core::ptr::write_bytes((hhdm + root_pa) as *mut u8, 0, PAGE_SIZE as usize) };

        // SAFETY: `root_pa` is an exclusively-owned, zeroed 4 KiB frame.
        let mut space = unsafe { AddressSpace::from_root(hhdm, root_pa) };
        // The recorded boot root, not CR3 -- see `KERNEL_ROOT`. There is no
        // fallback to the active root: once a user thread is running, CR3 is
        // that process's, and copying a kernel half out of it is the dependency
        // `KERNEL_ROOT` exists to break. This crate is a `no_std` binary with no
        // host tests, so every `#[test_case]` runs in QEMU after `boot` has
        // called `record_kernel_root`; nothing can legitimately reach here
        // first.
        let recorded = KERNEL_ROOT.load(core::sync::atomic::Ordering::Acquire);
        assert_ne!(recorded, 0, "VmSpace::new before record_kernel_root");
        // SAFETY: `recorded` is the kernel's own root, which is never freed.
        let kernel = unsafe { AddressSpace::from_root(hhdm, recorded) };
        // SAFETY: `kernel` is the running kernel's address space by construction.
        unsafe { space.copy_kernel_half(&kernel) };

        Some(Self { space, root_pa, owned: alloc::vec![root_pa] })
    }

    pub fn root_frame(&self) -> u64 {
        self.root_pa
    }

    /// Maps one user page.
    ///
    /// `PageFlags::USER` is set unconditionally — that is what makes this a
    /// *user* mapping. A page reachable from ring 3 without it faults in a way
    /// that reads as a missing mapping rather than as a permission bug.
    pub fn map_user(
        &mut self,
        va: u64,
        pa: u64,
        writable: bool,
        executable: bool,
    ) -> Result<(), MapError> {
        // Both ends. The upper one keeps a "user" mapping out of the shared
        // kernel half; the lower one keeps the null page unmapped. See
        // `USER_MAX` and `USER_MIN`.
        //
        // `NotUserAddress`, not `Misaligned`: the latter is documented in
        // `paging` as meaning page alignment, so reporting it here left a
        // caller unable to tell `0x401` from a kernel address.
        if va >= USER_MAX || va < USER_MIN {
            return Err(MapError::NotUserAddress);
        }
        let mut flags = PageFlags::PRESENT | PageFlags::USER;
        if writable {
            flags = flags | PageFlags::WRITABLE;
        }
        if !executable {
            flags = flags | PageFlags::NO_EXECUTE;
        }

        let owned = &mut self.owned;
        // SAFETY: `va` is a user address in an address space this owns, and
        // every intermediate table the mapper allocates is recorded so drop can
        // free it.
        unsafe {
            self.space.map(va, pa, flags, &mut || {
                let frame = crate::frames::alloc(0)?;
                owned.push(frame);
                Some(frame)
            })
        }
    }

    /// Allocates a frame, records it, and maps it at `va`.
    ///
    /// The common case: user memory that is not backed by anything the caller
    /// already has. Zeroed, because handing a process the previous owner's
    /// memory is an information leak across the ring boundary.
    pub fn map_new_page(
        &mut self,
        va: u64,
        writable: bool,
        executable: bool,
    ) -> Result<u64, MapError> {
        let pa = crate::frames::alloc(0).ok_or(MapError::OutOfFrames)?;
        let hhdm = crate::boot::hhdm_offset();
        // SAFETY: freshly allocated 4 KiB frame, mapped through the HHDM.
        unsafe { core::ptr::write_bytes((hhdm + pa) as *mut u8, 0, PAGE_SIZE as usize) };
        self.owned.push(pa);
        self.map_user(va, pa, writable, executable)?;
        Ok(pa)
    }

    /// Whether `va` already has a mapping in this address space.
    ///
    /// The ELF loader needs this because two segments can share a page. Mapping
    /// such a page twice leaks the first frame and discards whatever was
    /// already copied into it.
    pub fn is_mapped(&mut self, va: u64) -> bool {
        self.space.translate(va).is_some()
    }

    /// Physical frame backing `va`, if any.
    pub fn translate(&mut self, va: u64) -> Option<u64> {
        self.space.translate(va)
    }

    /// Re-applies permissions to an already-mapped user page.
    ///
    /// A program's text has to be writable while the kernel copies it in and
    /// read-only once the process can run. Remapping is cheaper than a second
    /// temporary mapping of the same frame, and nothing else can reach the
    /// address space in between.
    pub fn set_permissions(
        &mut self,
        va: u64,
        writable: bool,
        executable: bool,
    ) -> Result<(), MapError> {
        // SAFETY: `va` was mapped by this address space, so unmapping returns
        // a frame this owns rather than one shared with the kernel half.
        let pa = unsafe { self.space.unmap(va)? };
        // The frame stays in `owned`: it was detached from a virtual address,
        // not returned to the frame allocator.
        self.map_user(va, pa, writable, executable)
    }

    /// Loads this address space into CR3.
    ///
    /// # Safety
    /// The caller must be executing on a stack and code that this address space
    /// maps — which the shared kernel half guarantees for kernel code, and
    /// which is why `new` copies it before anything can activate.
    pub unsafe fn activate(&self) {
        unsafe { self.space.activate() };
    }

    /// Frames this address space allocated, for tests and accounting.
    pub fn owned_frames(&self) -> usize {
        self.owned.len()
    }
}

impl Drop for VmSpace {
    fn drop(&mut self) {
        // Guard against tearing down the address space the CPU is running on.
        // Freeing the root while CR3 still points at it hands the live page
        // table to the next allocator caller, and the fault that follows has no
        // tables left to report itself through.
        let hhdm = crate::boot::hhdm_offset();
        // SAFETY: reads CR3 only.
        let active_root = unsafe { AddressSpace::active(hhdm).root_frame() };
        assert_ne!(
            active_root, self.root_pa,
            "dropped the address space that is currently loaded in CR3"
        );

        // Reverse order so user pages go back before the tables that mapped
        // them; the allocator does not care, but a leak shows up as the root
        // surviving rather than as an arbitrary frame surviving.
        for pa in self.owned.drain(..).rev() {
            // SAFETY: every address in `owned` was returned by `frames::alloc`
            // at order 0 and is not referenced by any other address space --
            // shared kernel tables are never recorded here.
            unsafe { crate::frames::free(pa, 0) };
        }
    }
}
