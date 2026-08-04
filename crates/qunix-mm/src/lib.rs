#![cfg_attr(not(any(test, feature = "std")), no_std)]

//! Physical frame and kernel heap allocators.
//!
//! Neither allocator here takes a lock: they are plain data structures that
//! assume the caller already has exclusive access. The kernel wraps them in
//! `qunix_sync::SpinLock`, which keeps the policy (which lock, how long it is
//! held, whether interrupts are masked) where the kernel can see it instead of
//! baked into this crate.

pub mod buddy;
pub mod slab;

pub const PAGE_SIZE: u64 = 4096;
/// Highest block order the allocator will coalesce to (4 KiB << 18 = 1 GiB).
///
/// Capping at order 10 (4 MiB) would shard a large machine's memory into blocks
/// that can never merge — 64 GiB becomes 16384 order-10 blocks — and would make
/// 1 GiB huge pages and large contiguous DMA regions impossible to satisfy.
/// The cost of raising it is one `u64` of free-list head per order.
pub const MAX_ORDER: u8 = 18;

/// Read/write access to individual machine words inside a physical frame.
///
/// The buddy allocator threads its free lists through the frames it manages,
/// so it needs exactly this much access to physical memory and nothing more.
/// The kernel implements it over the HHDM; tests implement it over a `Vec`.
///
/// A free block carries three words of allocator bookkeeping in its first 24
/// bytes (next, prev, free tag), so `pa` is only required to be 8-byte aligned
/// rather than page aligned; it always lies within the first page of a block
/// the allocator owns.
///
/// `read_link` is called on frames the allocator has *handed out*, not only on
/// frames it holds: coalescing in `free` reads the prospective buddy's tag to
/// decide whether it is free, and that buddy may be allocated and in active use
/// by a driver at that moment. Two obligations follow, and both are required
/// rather than incidental. The read must be a volatile one — the bytes can
/// change under it, and the value is only ever compared against a tag, never
/// trusted as data. And the implementation must never form a `&` or `&mut` to
/// the location, because doing so claims an exclusive or immutable-for-the-
/// lifetime access the owner is actively violating; that is undefined behaviour
/// regardless of what the read returns.
pub trait FrameBacking {
    /// # Safety
    /// `pa` must be an 8-byte-aligned address inside a frame belonging to a
    /// region added to the allocator.
    unsafe fn read_link(&self, pa: u64) -> u64;
    /// # Safety
    /// `pa` must be an 8-byte-aligned address inside a frame belonging to a
    /// region added to the allocator.
    unsafe fn write_link(&self, pa: u64, value: u64);
}
