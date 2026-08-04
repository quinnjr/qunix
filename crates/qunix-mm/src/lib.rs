#![cfg_attr(not(test), no_std)]

pub mod buddy;

pub const PAGE_SIZE: u64 = 4096;
pub const MAX_ORDER: u8 = 10; // 4 KiB .. 4 MiB

/// Read/write access to the first machine word of a physical frame.
///
/// The buddy allocator threads its free lists through the frames it manages,
/// so it needs exactly this much access to physical memory and nothing more.
/// The kernel implements it over the HHDM; tests implement it over a `Vec`.
pub trait FrameBacking {
    /// # Safety
    /// `pa` must be a page-aligned address inside a region added to the allocator.
    unsafe fn read_link(&self, pa: u64) -> u64;
    /// # Safety
    /// `pa` must be a page-aligned address inside a region added to the allocator.
    unsafe fn write_link(&self, pa: u64, value: u64);
}
