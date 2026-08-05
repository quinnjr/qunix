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
        // SAFETY: CR3 currently holds the kernel's tables.
        let kernel = unsafe { AddressSpace::active(hhdm) };
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

    /// Changes the writability of an already-mapped user page.
    ///
    /// Exists for one reason: a program's text must be writable while the
    /// kernel copies it in and read-only once the process can run. Remapping
    /// is cheaper than a second temporary mapping of the same frame, and
    /// nothing else can reach the address space in between.
    pub fn set_writable(&mut self, va: u64, writable: bool) -> Result<(), MapError> {
        // SAFETY: `va` was mapped by this address space, so unmapping returns
        // the frame this owns rather than one shared with the kernel half.
        let pa = unsafe { self.space.unmap(va)? };
        // The frame is still in `owned` -- it was never given back to the frame
        // allocator, only detached from this virtual address.
        let executable = !writable;
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
