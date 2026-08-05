use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::{MapToError, TranslateResult, UnmapError};
use x86_64::structures::paging::page_table::PageTableIndex;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size2MiB, Size4KiB, Translate,
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

    // `pub(crate)` rather than `pub`: host tests need to observe the flag
    // translation without the mapping to `x86_64`'s types becoming public API.
    pub(crate) fn to_x86(self) -> PageTableFlags {
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

/// Non-exhaustive: paging gains failure modes as the MMU layer grows (huge-page
/// teardown, shootdown failures), and adding a variant must not silently widen
/// what an existing exhaustive `match` in a downstream crate claims to handle.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MapError {
    OutOfFrames,
    AlreadyMapped,
    NotMapped,
    UnsupportedPageSize,
    /// An address was not aligned to the page size the operation requires:
    /// 4 KiB for [`AddressSpace::map`]/[`AddressSpace::unmap`], 2 MiB for
    /// [`AddressSpace::map_2mib`]. Also reported when the frame allocator
    /// hands back an unaligned frame for an intermediate table.
    Misaligned,
    /// A page-table entry held a non-page-aligned address. Reporting this as
    /// `NotMapped` would invite a caller to conclude the VA is free and remap it.
    CorruptEntry,
}

/// Translates a `map_to` failure.
///
/// `alloc_failure` is supplied by the caller because `FrameAllocator` can only
/// report absence: a corrupt allocator and an exhausted one both surface as
/// `FrameAllocationFailed`, and only the allocator wrapper knows which it was.
fn map_to_error<S: PageSize>(err: MapToError<S>, alloc_failure: MapError) -> MapError {
    match err {
        MapToError::FrameAllocationFailed => alloc_failure,
        MapToError::PageAlreadyMapped(_) => MapError::AlreadyMapped,
        MapToError::ParentEntryHugePage => MapError::UnsupportedPageSize,
    }
}

fn unmap_error(err: UnmapError) -> MapError {
    match err {
        UnmapError::PageNotMapped => MapError::NotMapped,
        UnmapError::ParentEntryHugePage => MapError::UnsupportedPageSize,
        UnmapError::InvalidFrameAddress(_) => MapError::CorruptEntry,
    }
}

/// Adapts a closure returning physical frame addresses to the `x86_64` crate's
/// `FrameAllocator`, so callers need not depend on that crate.
struct ClosureFrames<'a, F: FnMut() -> Option<u64>> {
    frames: &'a mut F,
    /// `FrameAllocator` can only say "no frame", so `map_to` collapses a
    /// corrupt allocator and an exhausted one into the same
    /// `FrameAllocationFailed`. Recording the offending address lets the
    /// caller report `Misaligned` — a bug — instead of `OutOfFrames`, which
    /// sends the heap into pointless order-degradation on a machine with
    /// gigabytes free.
    saw_misaligned: Option<u64>,
}

impl<'a, F: FnMut() -> Option<u64>> ClosureFrames<'a, F> {
    fn new(frames: &'a mut F) -> Self {
        Self {
            frames,
            saw_misaligned: None,
        }
    }

    /// Which `MapError` a `FrameAllocationFailed` from `map_to` really means.
    fn frame_failure(&self) -> MapError {
        if self.saw_misaligned.is_some() {
            MapError::Misaligned
        } else {
            MapError::OutOfFrames
        }
    }
}

unsafe impl<F: FnMut() -> Option<u64>> FrameAllocator<Size4KiB> for ClosureFrames<'_, F> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        // Rounding down here would install an intermediate page table
        // overlapping the preceding frame — silent page-table corruption.
        let pa = (self.frames)()?;
        match PhysFrame::from_start_address(PhysAddr::new(pa)) {
            Ok(frame) => Some(frame),
            Err(_) => {
                self.saw_misaligned = Some(pa);
                None
            }
        }
    }
}

/// A handle to a page table.
///
/// Deliberately stores a raw pointer rather than an `OffsetPageTable<'static>`.
/// Holding the latter would mean fabricating a `&'static mut PageTable`, and
/// `active()` fabricates one to the *live* CR3 — so two `AddressSpace` values
/// would be two `&'static mut` aliases to the same table, which is UB even
/// though the uses are sequential in practice. Building the `&mut` inside each
/// operation confines the borrow to that call.
pub struct AddressSpace {
    root: *mut PageTable,
    /// Byte-granular pointer to physical address 0 as seen through the HHDM.
    /// Every table pointer is offset from this one so they share a single
    /// exposed provenance; casting each `hhdm_offset + pa` integer separately
    /// would give pointers with no provenance relationship to the map.
    hhdm_base: *mut u8,
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
        let hhdm_base = core::ptr::with_exposed_provenance_mut::<u8>(hhdm_offset as usize);
        let root = unsafe { hhdm_base.add(root_pa as usize).cast::<PageTable>() };
        Self {
            root,
            hhdm_base,
            hhdm_offset,
        }
    }

    /// Borrows the table for the duration of one operation.
    ///
    /// # Safety
    /// The caller must not hold another mapper over the same table concurrently.
    unsafe fn mapper(&mut self) -> OffsetPageTable<'_> {
        unsafe { OffsetPageTable::new(&mut *self.root, VirtAddr::new(self.hhdm_offset)) }
    }

    /// Loads this address space into CR3.
    ///
    /// The first code in this project to write CR3 — until M1 Task 7 the kernel
    /// ran on the tables Limine built and never switched. Two consequences
    /// follow and neither is theoretical:
    ///
    /// The higher half must already be mapped in *this* table before the write.
    /// The instruction after `mov cr3` is fetched through the new tables, so a
    /// root without the kernel's own text mapped faults on the instruction that
    /// would have handled the fault. [`copy_kernel_half`] is what establishes
    /// that, and calling this on a bare `from_root` frame triple-faults.
    ///
    /// Writing CR3 flushes every non-global TLB entry, which is why no explicit
    /// invalidation is needed here — and why switching address spaces is
    /// expensive enough to be worth avoiding in a loop.
    ///
    /// # Safety
    /// The root must contain a valid mapping for all currently-executing kernel
    /// code, the current stack, and any data touched before the next switch.
    pub unsafe fn activate(&self) {
        let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(self.root_frame()));
        // Flags are preserved rather than zeroed: CR3 carries PCID bits on
        // machines that enable them, and clobbering those silently changes
        // which TLB tags apply.
        let (_, flags) = Cr3::read();
        unsafe { Cr3::write(frame, flags) };
    }

    /// Copies the kernel's higher-half PML4 entries into `self`.
    ///
    /// Every address space shares one kernel half. Copying the *top-level*
    /// entries rather than the tables beneath them means the sharing is by
    /// reference: a later kernel mapping becomes visible in every address space
    /// without walking them all. It also means an address space must never free
    /// tables reachable from these entries — they are not its own.
    ///
    /// Entries 256..512 are the higher half on x86-64: bit 47 of the virtual
    /// address is the sign bit, so PML4 index >= 256 is exactly the set of
    /// addresses with the top bits set.
    ///
    /// # Safety
    /// `from` must be an address space whose higher half is the kernel's.
    pub unsafe fn copy_kernel_half(&mut self, from: &AddressSpace) {
        let src = unsafe { &*from.root };
        let dst = unsafe { &mut *self.root };
        for i in 256..512 {
            dst[i] = src[i].clone();
        }
    }

    pub fn root_frame(&self) -> u64 {
        VirtAddr::from_ptr(self.root).as_u64() - self.hhdm_offset
    }

    /// Maps one 2 MiB page.
    ///
    /// A 2 MiB region costs one PD entry instead of a PT frame plus 512 PT
    /// entries, one page-table walk instead of 512, and occupies a single TLB
    /// entry for its whole life rather than competing for 512 of them.
    ///
    /// # Safety
    /// As [`Self::map`]. Both `va` and `pa` must be 2 MiB aligned, and the
    /// region must not be one that later needs per-4-KiB permissions: `unmap`
    /// is 4 KiB-only and will report `UnsupportedPageSize` for a huge mapping.
    pub unsafe fn map_2mib(
        &mut self,
        va: u64,
        pa: u64,
        flags: PageFlags,
        frames: &mut impl FnMut() -> Option<u64>,
    ) -> Result<(), MapError> {
        let page = Page::<Size2MiB>::from_start_address(VirtAddr::new(va))
            .map_err(|_| MapError::Misaligned)?;
        let frame = PhysFrame::<Size2MiB>::from_start_address(PhysAddr::new(pa))
            .map_err(|_| MapError::Misaligned)?;
        let mut allocator = ClosureFrames::new(frames);
        let x86_flags = flags.to_x86() | PageTableFlags::HUGE_PAGE;
        let result = unsafe { self.mapper().map_to(page, frame, x86_flags, &mut allocator) };
        match result {
            Ok(flush) => {
                // Flushing unconditionally rather than `ignore()`: skipping the
                // invlpg would rest on the unstated premise that every prior
                // teardown of this range flushed. One invlpg per 2 MiB is 8
                // instructions across a 16 MiB boot map.
                flush.flush();
                Ok(())
            }
            Err(err) => Err(map_to_error(err, allocator.frame_failure())),
        }
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
        // `containing_address` would silently round down, so `map(0x1234, ..)`
        // would report Ok having mapped 0x1000 — the caller then believes a
        // mapping exists that does not.
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(va))
            .map_err(|_| MapError::Misaligned)?;
        let frame = PhysFrame::from_start_address(PhysAddr::new(pa))
            .map_err(|_| MapError::Misaligned)?;
        let mut allocator = ClosureFrames::new(frames);
        let mut mapper = unsafe { self.mapper() };
        let result = unsafe { mapper.map_to(page, frame, flags.to_x86(), &mut allocator) };
        match result {
            Ok(flush) => {
                flush.flush();
                Ok(())
            }
            Err(err) => Err(map_to_error(err, allocator.frame_failure())),
        }
    }

    /// Removes a 4 KiB mapping and returns the physical address it pointed at.
    ///
    /// # Safety
    /// Nothing may hold a reference derived from `va` after this returns.
    pub unsafe fn unmap(&mut self, va: u64) -> Result<u64, MapError> {
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(va))
            .map_err(|_| MapError::Misaligned)?;
        let mut mapper = unsafe { self.mapper() };
        match mapper.unmap(page) {
            Ok((frame, flush)) => {
                flush.flush();
                Ok(frame.start_address().as_u64())
            }
            Err(err) => Err(unmap_error(err)),
        }
    }

    /// Unmaps a 4 KiB page and frees any intermediate page tables the removal
    /// leaves empty.
    ///
    /// Plain [`Self::unmap`] returns only the leaf frame, so the PT/PD/PDPT
    /// frames that `map_to` allocated stay linked forever — up to 12 KiB
    /// permanently lost per abandoned 2 MiB window.
    ///
    /// Returns the leaf physical address and the number of intermediate tables
    /// freed (0 through 3). A caller that always sees 0 is leaking page tables
    /// — the previous `Result<u64, _>` made that indistinguishable from a
    /// successful prune, so a systematic failure could leak forever unnoticed.
    ///
    /// # Safety
    /// As [`Self::unmap`], plus: every intermediate table along `va` (PDPT, PD,
    /// PT) must have been allocated by the same `frames` closure previously
    /// passed to [`Self::map`] or [`Self::map_2mib`] on *this* address space,
    /// and `free` must be that allocator's matching release path. The body
    /// hands any table it finds empty straight to `free`; it cannot tell who
    /// allocated it.
    ///
    /// Consequently this must never be called on a VA whose tables the
    /// bootloader built — which is every VA reachable through
    /// [`Self::active`] that this kernel did not map itself, including the
    /// HHDM, the kernel image, and Limine's own structures. Doing so releases
    /// firmware-owned memory into the kernel allocator and corrupts it.
    pub unsafe fn unmap_and_prune(
        &mut self,
        va: u64,
        free: &mut impl FnMut(u64),
    ) -> Result<(u64, u8), MapError> {
        let pa = unsafe { self.unmap(va)? };
        let addr = VirtAddr::new(va);

        // Each level's borrow is scoped: the parent is only taken mutably once
        // its child has been proven empty, so no two tables are borrowed at
        // once. An empty table is unreachable by construction, so unlinking it
        // cannot invalidate a live translation.
        let Some(p3_pa) = (unsafe { self.child_of(self.root, addr.p4_index()) }) else {
            return Ok((pa, 0));
        };
        let p3 = self.table_at(p3_pa);
        let Some(p2_pa) = (unsafe { self.child_of(p3, addr.p3_index()) }) else {
            return Ok((pa, 0));
        };
        let p2 = self.table_at(p2_pa);
        let Some(p1_pa) = (unsafe { self.child_of(p2, addr.p2_index()) }) else {
            return Ok((pa, 0));
        };

        if !unsafe { Self::is_empty(self.table_at(p1_pa)) } {
            return Ok((pa, 0));
        }

        // Frames are collected rather than freed inline: the buddy allocator
        // writes free-list links into a frame the instant it is released, but
        // the CPU may still hold that frame in a paging-structure cache until
        // the invalidation below, and would then walk allocator metadata as a
        // page table. Unlink everything, invalidate, and only then release.
        //
        // This is single-CPU correct only. M1 must replace `flush_all` with a
        // cross-CPU shootdown that waits for every other CPU to acknowledge
        // before the frames reach `free`; otherwise a remote CPU's cached
        // structure has the same lifetime hazard.
        let mut freed: [Option<u64>; 3] = [None; 3];
        unsafe { Self::clear_entry(p2, addr.p2_index()) };
        freed[0] = Some(p1_pa);

        if unsafe { Self::is_empty(p2) } {
            unsafe { Self::clear_entry(p3, addr.p3_index()) };
            freed[1] = Some(p2_pa);

            if unsafe { Self::is_empty(p3) } {
                unsafe { Self::clear_entry(self.root, addr.p4_index()) };
                freed[2] = Some(p3_pa);
            }
        }

        // Structures changed above the leaf, so the per-page flush `unmap`
        // already issued does not cover it.
        x86_64::instructions::tlb::flush_all();

        let mut count = 0u8;
        for frame_pa in freed.into_iter().flatten() {
            free(frame_pa);
            count += 1;
        }
        Ok((pa, count))
    }

    fn table_at(&self, frame_pa: u64) -> *mut PageTable {
        // Derived from the exposed HHDM base so the pointer inherits that
        // allocation's provenance, rather than being conjured from an integer.
        unsafe { self.hhdm_base.add(frame_pa as usize).cast() }
    }

    /// Physical address of the next-level table, or `None` when the entry is
    /// absent or maps a huge page (which owns no child table).
    ///
    /// # Safety
    /// `table` must be `table_at(pa)` for a `pa` the caller has established
    /// lies inside the HHDM-mapped range — `table_at` derives a pointer from a
    /// raw entry address without bounds-checking it — and no other reference to
    /// that table may be alive for the duration of the call.
    ///
    /// These three helpers take a raw pointer rather than `&PageTable` because
    /// the caller walks several levels at once. A raw parameter keeps each
    /// reference scoped to a single call, so no two levels are ever borrowed
    /// simultaneously. It does *not* make aliased tables safe: the bodies still
    /// materialise a reference, and two levels that alias — which a
    /// self-referencing entry in a table this kernel did not build would
    /// produce — would also make `unmap_and_prune` free the same frame twice.
    /// Aliased tables remain forbidden by that function's own contract; the raw
    /// pointer shortens the borrow, it does not remove it.
    unsafe fn child_of(&self, table: *mut PageTable, index: PageTableIndex) -> Option<u64> {
        let table: &PageTable = unsafe { &*table };
        let entry = &table[index];
        if entry.is_unused() || entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }
        Some(entry.addr().as_u64())
    }

    /// # Safety
    /// As [`child_of`](Self::child_of).
    unsafe fn is_empty(table: *mut PageTable) -> bool {
        let table: &PageTable = unsafe { &*table };
        table.iter().all(|e| e.is_unused())
    }

    /// # Safety
    /// As [`child_of`](Self::child_of), and the entry must not be one the
    /// caller still intends to walk through.
    unsafe fn clear_entry(table: *mut PageTable, index: PageTableIndex) {
        let table: &mut PageTable = unsafe { &mut *table };
        table[index].set_unused();
    }

    pub fn translate(&mut self, va: u64) -> Option<u64> {
        let mapper = unsafe { self.mapper() };
        match mapper.translate(VirtAddr::new(va)) {
            TranslateResult::Mapped { frame, offset, .. } => {
                Some(frame.start_address().as_u64() + offset)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_x86_maps_each_flag() {
        let cases = [
            (PageFlags::PRESENT, PageTableFlags::PRESENT),
            (PageFlags::WRITABLE, PageTableFlags::WRITABLE),
            (PageFlags::USER, PageTableFlags::USER_ACCESSIBLE),
            (PageFlags::NO_CACHE, PageTableFlags::NO_CACHE),
            (PageFlags::NO_EXECUTE, PageTableFlags::NO_EXECUTE),
        ];
        for (ours, theirs) in cases {
            assert_eq!(ours.to_x86(), theirs);
        }
    }

    #[test]
    fn to_x86_sets_exactly_the_requested_bits() {
        let all = PageFlags::PRESENT
            | PageFlags::WRITABLE
            | PageFlags::USER
            | PageFlags::NO_CACHE
            | PageFlags::NO_EXECUTE;
        let x86 = all.to_x86();
        assert_eq!(
            x86,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::NO_EXECUTE
        );
        // Guards against a stray extra bit (e.g. HUGE_PAGE) leaking in.
        assert_eq!(x86.bits().count_ones(), 5);
    }

    #[test]
    fn to_x86_of_nothing_is_empty() {
        assert_eq!(PageFlags(0).to_x86(), PageTableFlags::empty());
    }

    #[test]
    fn bitor_composes() {
        let combined = PageFlags::PRESENT | PageFlags::WRITABLE;
        assert_eq!(combined, PageFlags(0b11));
        assert_eq!(
            combined.to_x86(),
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        );
        // Idempotent, and order-independent.
        assert_eq!(combined | PageFlags::PRESENT, combined);
        assert_eq!(PageFlags::WRITABLE | PageFlags::PRESENT, combined);
    }

    #[test]
    fn map_to_error_translations() {
        let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(0x1000));
        assert_eq!(
            map_to_error(
                MapToError::<Size4KiB>::FrameAllocationFailed,
                MapError::OutOfFrames
            ),
            MapError::OutOfFrames
        );
        // The whole point of threading `alloc_failure` through: a misaligned
        // frame must not be reported as capacity exhaustion.
        assert_eq!(
            map_to_error(
                MapToError::<Size4KiB>::FrameAllocationFailed,
                MapError::Misaligned
            ),
            MapError::Misaligned
        );
        assert_eq!(
            map_to_error(MapToError::PageAlreadyMapped(frame), MapError::OutOfFrames),
            MapError::AlreadyMapped
        );
        assert_eq!(
            map_to_error(
                MapToError::<Size4KiB>::ParentEntryHugePage,
                MapError::OutOfFrames
            ),
            MapError::UnsupportedPageSize
        );
    }

    #[test]
    fn unmap_error_translations() {
        assert_eq!(unmap_error(UnmapError::PageNotMapped), MapError::NotMapped);
        assert_eq!(
            unmap_error(UnmapError::ParentEntryHugePage),
            MapError::UnsupportedPageSize
        );
        assert_eq!(
            unmap_error(UnmapError::InvalidFrameAddress(PhysAddr::new(0x1001))),
            MapError::CorruptEntry
        );
    }

    #[test]
    fn closure_frames_flags_misalignment_not_exhaustion() {
        let mut supply = || Some(0x1001u64);
        let mut frames = ClosureFrames::new(&mut supply);
        assert!(FrameAllocator::<Size4KiB>::allocate_frame(&mut frames).is_none());
        assert_eq!(frames.saw_misaligned, Some(0x1001));
        assert_eq!(frames.frame_failure(), MapError::Misaligned);
    }

    #[test]
    fn closure_frames_reports_exhaustion_when_aligned() {
        let mut supply = || None;
        let mut frames = ClosureFrames::new(&mut supply);
        assert!(FrameAllocator::<Size4KiB>::allocate_frame(&mut frames).is_none());
        assert_eq!(frames.saw_misaligned, None);
        assert_eq!(frames.frame_failure(), MapError::OutOfFrames);
    }
}
