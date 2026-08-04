use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::{MapToError, TranslateResult, UnmapError};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageFlags(u64);

impl PageFlags {
    pub const PRESENT: Self = Self(1 << 0);
    pub const WRITABLE: Self = Self(1 << 1);
    pub const USER: Self = Self(1 << 2);
    pub const NO_CACHE: Self = Self(1 << 4);
    pub const NO_EXECUTE: Self = Self(1 << 63);

    fn to_x86(self) -> PageTableFlags {
        let mut flags = PageTableFlags::empty();
        if self.0 & Self::PRESENT.0 != 0 {
            flags |= PageTableFlags::PRESENT;
        }
        if self.0 & Self::WRITABLE.0 != 0 {
            flags |= PageTableFlags::WRITABLE;
        }
        if self.0 & Self::USER.0 != 0 {
            flags |= PageTableFlags::USER_ACCESSIBLE;
        }
        if self.0 & Self::NO_CACHE.0 != 0 {
            flags |= PageTableFlags::NO_CACHE;
        }
        if self.0 & Self::NO_EXECUTE.0 != 0 {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        flags
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    OutOfFrames,
    AlreadyMapped,
    NotMapped,
    UnsupportedPageSize,
}

/// Adapts a closure returning physical frame addresses to the `x86_64` crate's
/// `FrameAllocator`, so callers need not depend on that crate.
struct ClosureFrames<'a, F: FnMut() -> Option<u64>>(&'a mut F);

unsafe impl<F: FnMut() -> Option<u64>> FrameAllocator<Size4KiB> for ClosureFrames<'_, F> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        (self.0)().map(|pa| PhysFrame::containing_address(PhysAddr::new(pa)))
    }
}

pub struct AddressSpace {
    mapper: OffsetPageTable<'static>,
    hhdm_offset: u64,
}

impl AddressSpace {
    /// Wraps the page table currently loaded in CR3.
    ///
    /// # Safety
    /// `hhdm_offset` must be the bootloader's higher-half direct map offset,
    /// and all of physical memory must be mapped at that offset.
    pub unsafe fn active(hhdm_offset: u64) -> Self {
        let (frame, _) = Cr3::read();
        unsafe { Self::from_root(hhdm_offset, frame.start_address().as_u64()) }
    }

    /// Wraps a caller-supplied PML4 frame.
    ///
    /// # Safety
    /// `root_pa` must be an exclusively owned 4 KiB frame holding a valid (or
    /// zeroed) PML4, reachable through `hhdm_offset`.
    pub unsafe fn from_root(hhdm_offset: u64, root_pa: u64) -> Self {
        let virt = VirtAddr::new(hhdm_offset + root_pa);
        let table: &'static mut PageTable = unsafe { &mut *virt.as_mut_ptr() };
        let mapper = unsafe { OffsetPageTable::new(table, VirtAddr::new(hhdm_offset)) };
        Self { mapper, hhdm_offset }
    }

    pub fn root_frame(&mut self) -> u64 {
        let table = self.mapper.level_4_table() as *const PageTable;
        VirtAddr::from_ptr(table).as_u64() - self.hhdm_offset
    }

    /// Maps a 4 KiB page.
    ///
    /// # Safety
    /// Creating a mapping can alias memory arbitrarily; the caller owns `pa`
    /// and must ensure `va` is not already in use by something else.
    pub unsafe fn map(
        &mut self,
        va: u64,
        pa: u64,
        flags: PageFlags,
        frames: &mut impl FnMut() -> Option<u64>,
    ) -> Result<(), MapError> {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        let frame = PhysFrame::containing_address(PhysAddr::new(pa));
        let mut allocator = ClosureFrames(frames);
        match unsafe { self.mapper.map_to(page, frame, flags.to_x86(), &mut allocator) } {
            Ok(flush) => {
                flush.flush();
                Ok(())
            }
            Err(MapToError::FrameAllocationFailed) => Err(MapError::OutOfFrames),
            Err(MapToError::PageAlreadyMapped(_)) => Err(MapError::AlreadyMapped),
            Err(MapToError::ParentEntryHugePage) => Err(MapError::UnsupportedPageSize),
        }
    }

    /// Removes a 4 KiB mapping and returns the physical address it pointed at.
    ///
    /// # Safety
    /// Nothing may hold a reference derived from `va` after this returns.
    pub unsafe fn unmap(&mut self, va: u64) -> Result<u64, MapError> {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        match self.mapper.unmap(page) {
            Ok((frame, flush)) => {
                flush.flush();
                Ok(frame.start_address().as_u64())
            }
            Err(UnmapError::PageNotMapped) => Err(MapError::NotMapped),
            Err(UnmapError::ParentEntryHugePage) => Err(MapError::UnsupportedPageSize),
            Err(UnmapError::InvalidFrameAddress(_)) => Err(MapError::NotMapped),
        }
    }

    pub fn translate(&self, va: u64) -> Option<u64> {
        match self.mapper.translate(VirtAddr::new(va)) {
            TranslateResult::Mapped { frame, offset, .. } => {
                Some(frame.start_address().as_u64() + offset)
            }
            _ => None,
        }
    }
}
