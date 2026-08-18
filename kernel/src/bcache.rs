//! The buffer cache: block-sized frames, reserved at boot, keyed by device and
//! block number.
//!
//! [`qunix_bcache`] owns the bookkeeping — which slot holds which block and
//! what may be done with it — and is tested on the host. This module owns the
//! memory that bookkeeping names, and the one thing that cannot be tested on
//! the host: that a hit returns the bytes the disk actually holds.
//!
//! # The frames are reserved once and never returned
//!
//! Every slot's frame is allocated in [`init`] and kept for the life of the
//! kernel. That is not a simplification to be tidied up later: memory pressure
//! triggers writeback, writeback issues I/O, and I/O needs a buffer. A cache
//! that allocated its buffer at writeback time would ask the frame allocator
//! for memory in exactly the situation where there is none, while holding the
//! pages whose release depends on that allocation. Reserving up front is what
//! makes the flush path allocation-free, and a change that makes these frames
//! on-demand reintroduces the deadlock however carefully it is written.
//!
//! # A reader holds a pin, not a bare reference
//!
//! [`read_block`] returns a [`BlockRef`], which pins its slot and unpins on
//! drop. A bare `&[u8]` into a slot would stay valid-looking after the slot was
//! evicted and refilled with another block: the read succeeds, the bytes are
//! wrong, and nothing between here and the program that asked is in a position
//! to notice. The pin is what makes eviction refuse while a reader is live.

use core::ops::Deref;

use qunix_bcache::{BlockKey, Cache, SlotState};
use qunix_hal_x86_64::Irq;
use qunix_sync::IrqSpinLock;
use qunix_virtio::blk::SECTOR_BYTES;

use crate::block::{self, BlockError};

/// Slots, and so frames reserved at boot.
///
/// 32 pages, 128 KiB. Small enough that reserving it up front is not a
/// meaningful charge against a machine's memory, large enough that the linear
/// scan in [`qunix_bcache::Cache`] is what the host benchmarks measure.
pub const SLOTS: usize = 32;

/// Bytes per cached block: one frame, and one whole driver transfer.
///
/// The three have to agree. A block larger than the driver's maximum transfer
/// could not be read in one request; a block larger than a frame would need a
/// contiguous multi-page allocation per slot.
pub const BLOCK_BYTES: usize = block::MAX_SECTORS * SECTOR_BYTES;

const SECTORS_PER_BLOCK: u64 = block::MAX_SECTORS as u64;

const _: () = assert!(BLOCK_BYTES == 4096, "a cached block must be exactly one frame");

/// How long a caller waits for another thread's fill of the same block.
///
/// Bounded, and reported rather than waited out forever: a fill that never
/// completes is a driver fault, and a cache that hangs on it turns one lost
/// completion into a machine that stops. The block driver's own request
/// deadline is shorter, so under any fault the filling thread fails first and
/// this is the backstop.
const FILL_WAIT_TICKS: u64 = 1;
const FILL_WAIT_LIMIT: u32 = 4096;

/// Why a cached read could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BcacheError {
    /// [`init`] could not reserve every slot's frame.
    NoFrames,
    /// The device refused or failed the read.
    Device(BlockError),
    /// Every slot holds a block that may not be evicted.
    ///
    /// Distinct from a device error: the disk is fine and the request is
    /// well-formed, but every buffer is dirty, in flight, or pinned. Reporting
    /// it is the alternative to evicting something that must not be evicted.
    Exhausted,
    /// Another thread's fill of this block did not finish in time.
    FillStalled,
}

struct Bcache {
    table: Cache<SLOTS>,
    /// Virtual address of each slot's frame, through the HHDM.
    frames: [u64; SLOTS],
    /// Whether a slot's outstanding I/O is a *fill* rather than a writeback.
    ///
    /// The distinction decides whether a hit is readable. A slot being filled
    /// holds nothing yet, so a reader must wait; a slot being written back
    /// holds the caller's own data and the device is only reading it, so a
    /// reader may proceed. Collapsing the two either serves an unfilled buffer
    /// or stalls every reader behind every flush.
    filling: [bool; SLOTS],
}

// SAFETY: `Bcache` is reachable only through `CACHE`, an `IrqSpinLock`, so
// every access is serialised. The addresses it holds name HHDM mappings of
// frames, which are equally valid from any processor.
unsafe impl Send for Bcache {}

static CACHE: IrqSpinLock<Option<Bcache>, Irq> = IrqSpinLock::new(None);

/// Reserves one frame per slot. Idempotent.
pub fn init() -> Result<(), BcacheError> {
    let mut guard = CACHE.lock();
    if guard.is_some() {
        return Ok(());
    }
    let mut frames = [0u64; SLOTS];
    for frame in frames.iter_mut() {
        // Order 0: one frame, which is one block. Never freed -- see the module
        // docs. A failure here leaves the frames already taken reserved too,
        // deliberately: this runs once at boot, and a kernel that cannot afford
        // 128 KiB of cache is not one that should proceed to mount a
        // filesystem.
        let phys = crate::frames::alloc(0).ok_or(BcacheError::NoFrames)?;
        *frame = crate::boot::hhdm_offset() + phys;
    }
    *guard = Some(Bcache { table: Cache::new(), frames, filling: [false; SLOTS] });
    Ok(())
}

/// A cached block, and the pin that keeps its slot from being reused.
pub struct BlockRef {
    slot: usize,
    virt: u64,
}

impl Deref for BlockRef {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `virt` is the HHDM mapping of a frame reserved for the life
        // of the kernel, and the pin this `BlockRef` holds stops the slot being
        // reused while the reference exists.
        unsafe { core::slice::from_raw_parts(self.virt as *const u8, BLOCK_BYTES) }
    }
}

impl Drop for BlockRef {
    fn drop(&mut self) {
        let mut guard = CACHE.lock();
        if let Some(cache) = guard.as_mut() {
            cache.table.unpin(self.slot);
        }
    }
}

/// What the lock said to do about a key.
enum Claim {
    /// Readable now, and already pinned.
    Hit(BlockRef),
    /// Claimed for this thread to fill.
    Fill { slot: usize, virt: u64 },
    /// Somebody else is filling it.
    Wait,
}

/// Decides what to do about `key`, under the lock and without any I/O.
///
/// Separate from [`read_block`] because the lock must not be held across the
/// `await`: the fill parks, and a parked thread holding an `IrqSpinLock` stops
/// every other processor that touches the cache — with interrupts masked, so
/// the completion that would release it can never be delivered.
fn claim(key: BlockKey) -> Result<Claim, BcacheError> {
    let mut guard = CACHE.lock();
    let cache = guard.as_mut().expect("the buffer cache was used before init");

    if let Some(slot) = cache.table.lookup(key) {
        if cache.table.state_of(slot) == Some(SlotState::InFlight) && cache.filling[slot] {
            return Ok(Claim::Wait);
        }
        cache.table.pin(slot);
        return Ok(Claim::Hit(BlockRef { slot, virt: cache.frames[slot] }));
    }

    let slot = match cache.table.insert(key) {
        Some(slot) => slot,
        None => {
            // Full. Evicting is the whole point of `victim` refusing, so a
            // refusal here is reported rather than worked around.
            let victim = cache.table.victim().ok_or(BcacheError::Exhausted)?;
            assert!(cache.table.evict(victim), "victim offered a slot evict refused");
            cache.table.insert(key).ok_or(BcacheError::Exhausted)?
        }
    };
    cache.filling[slot] = true;
    Ok(Claim::Fill { slot, virt: cache.frames[slot] })
}

/// Ends a fill, leaving the slot readable or free.
///
/// A failed fill releases the slot rather than leaving it clean and empty. The
/// buffer holds nothing the disk agreed to, so keeping the key would serve
/// whatever the frame contained to every later reader of that block, with the
/// read reported as a hit.
fn finish_fill(slot: usize, ok: bool) -> Option<BlockRef> {
    let mut guard = CACHE.lock();
    let cache = guard.as_mut().expect("a fill completed after the cache was torn down");
    cache.filling[slot] = false;
    cache.table.end_io(slot, SlotState::Clean);
    if !ok {
        assert!(cache.table.evict(slot), "a failed fill left a slot that could not be released");
        return None;
    }
    cache.table.pin(slot);
    Some(BlockRef { slot, virt: cache.frames[slot] })
}

/// Reads a block, from the cache if it is there and from the disk if it is not.
pub async fn read_block(key: BlockKey) -> Result<BlockRef, BcacheError> {
    let mut waited = 0u32;
    loop {
        match claim(key)? {
            Claim::Hit(block) => return Ok(block),
            Claim::Wait => {
                // Retried rather than queued. A waiter list is per-slot,
                // unbounded state in a table whose fixed size is the reason
                // the flush path cannot allocate, and the thing being waited
                // for is a disk read -- so a tick of latency is small beside
                // it. The bound is what stops a lost completion becoming a
                // machine that stops rather than a request that fails.
                waited += 1;
                if waited > FILL_WAIT_LIMIT {
                    return Err(BcacheError::FillStalled);
                }
                crate::task::sleep_ticks(FILL_WAIT_TICKS).await;
            }
            Claim::Fill { slot, virt } => {
                // SAFETY: the slot is `InFlight` and marked as filling, so no
                // other thread may read or reuse it until `finish_fill` runs.
                // The frame is reserved for the life of the kernel.
                let buf =
                    unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, BLOCK_BYTES) };
                let outcome = block::read_at(key.block * SECTORS_PER_BLOCK, buf).await;
                match finish_fill(slot, outcome.is_ok()) {
                    Some(block) => return Ok(block),
                    None => return Err(BcacheError::Device(outcome.unwrap_err())),
                }
            }
        }
    }
}

/// Slots currently holding a block. For tests and diagnostics.
pub fn resident() -> usize {
    let guard = CACHE.lock();
    match guard.as_ref() {
        Some(cache) => {
            (0..SLOTS).filter(|s| cache.table.state_of(*s) != Some(SlotState::Free)).count()
        }
        None => 0,
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut guard = CACHE.lock();
    let Some(cache) = guard.as_mut() else { return };
    for slot in 0..SLOTS {
        if cache.table.state_of(slot) == Some(SlotState::Free) {
            continue;
        }
        if cache.table.state_of(slot) == Some(SlotState::InFlight) {
            cache.table.end_io(slot, SlotState::Clean);
        }
        cache.filling[slot] = false;
        assert!(cache.table.evict(slot), "a slot could not be released between tests");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::block_on;

    fn ready() {
        // SAFETY: the suite runs on the bootstrap processor with the LAPIC
        // mapped and the completion vector installed.
        unsafe { block::init() }.expect("the block device did not come up");
        init().expect("the buffer cache could not reserve its frames");
        reset_for_test();
    }

    #[test_case]
    fn a_cached_read_returns_the_bytes_the_disk_holds() {
        // The test disk's every sector begins with its own LBA, so a slot
        // holding the wrong block shows up in the data rather than only in the
        // bookkeeping. A test that checked the bytes were merely non-zero would
        // pass with the cache returning any block at all.
        ready();
        let block = block_on(read_block(BlockKey { dev: 0, block: 1 }))
            .expect("a cached read of a live block failed");
        // Block 1 is sectors 8..16, so the first eight bytes are LBA 8.
        assert_eq!(
            u64::from_le_bytes(block[0..8].try_into().unwrap()),
            8,
            "the cache served the wrong block"
        );
        // And the last sector of the block, which a fill one sector short would
        // leave holding whatever the frame held before.
        let last = BLOCK_BYTES - SECTOR_BYTES;
        assert_eq!(
            u64::from_le_bytes(block[last..last + 8].try_into().unwrap()),
            15,
            "the fill did not cover the whole block"
        );
    }

    #[test_case]
    fn a_second_read_of_the_same_block_does_no_further_io() {
        // The point of the cache. Asserted against the driver's own completion
        // counter rather than against timing, which would pass on any machine
        // slow enough.
        ready();
        let key = BlockKey { dev: 0, block: 2 };
        let first = block_on(read_block(key)).expect("the first read failed");
        let completions = block::completions();
        let second = block_on(read_block(key)).expect("the second read failed");
        assert_eq!(
            block::completions(),
            completions,
            "a cache hit still went to the device"
        );
        // And it is the same buffer, not a second copy that happens to match.
        assert_eq!(first.as_ptr(), second.as_ptr(), "a hit returned a different frame");
    }

    #[test_case]
    fn a_miss_goes_to_the_device_and_a_different_block_is_a_miss() {
        // The other direction, which the hit test cannot assert: a cache that
        // returned its one resident block for every key would pass the test
        // above and fail this one.
        ready();
        let before = block::completions();
        let a = block_on(read_block(BlockKey { dev: 0, block: 3 })).expect("read of block 3");
        let after_first = block::completions();
        assert!(after_first > before, "a miss did not go to the device");
        let b = block_on(read_block(BlockKey { dev: 0, block: 4 })).expect("read of block 4");
        assert!(block::completions() > after_first, "a second block was served from the first");
        assert_ne!(a.as_ptr(), b.as_ptr(), "two blocks landed in one slot");
        assert_eq!(u64::from_le_bytes(a[0..8].try_into().unwrap()), 24);
        assert_eq!(u64::from_le_bytes(b[0..8].try_into().unwrap()), 32);
    }

    #[test_case]
    fn a_pinned_block_is_not_evicted_out_from_under_its_reader() {
        // The reason `read_block` returns a pin rather than a slice. Filling
        // the table while holding a reference must not reuse the slot that
        // reference names -- if it did, the read would succeed and the bytes
        // would be another block's.
        ready();
        let held = block_on(read_block(BlockKey { dev: 0, block: 5 })).expect("read of block 5");
        let held_ptr = held.as_ptr();
        // Enough distinct blocks to fill every slot several times over.
        for block in 100..100 + (SLOTS as u64 * 2) {
            let other = block_on(read_block(BlockKey { dev: 0, block }))
                .expect("a read failed while the table was under pressure");
            assert_ne!(other.as_ptr(), held_ptr, "block {block} was filled into a pinned slot");
        }
        assert_eq!(
            u64::from_le_bytes(held[0..8].try_into().unwrap()),
            40,
            "the pinned block's bytes changed while it was held"
        );
    }

    #[test_case]
    fn a_table_of_pinned_blocks_reports_exhaustion_rather_than_evicting_one() {
        // The refusal direction. A cache that evicted a pinned slot under
        // pressure would return every read successfully and hand out another
        // block's bytes to whoever still held the reference.
        ready();
        let mut held = alloc::vec::Vec::new();
        for block in 0..SLOTS as u64 {
            held.push(block_on(read_block(BlockKey { dev: 0, block })).expect("filling the table"));
        }
        assert_eq!(resident(), SLOTS, "the table did not fill");
        // A block that is on the disk, so a device error cannot be mistaken
        // for the refusal under test -- the first draft asked for block 999,
        // which is past the end of a 2048-sector image, and the follow-up read
        // failed for that reason instead.
        let spare = BlockKey { dev: 0, block: SLOTS as u64 };
        assert!(
            matches!(block_on(read_block(spare)), Err(BcacheError::Exhausted)),
            "a fully pinned table gave up a slot"
        );
        // Releasing one is enough to make progress again, which is what says
        // the refusal was about the pins rather than about the table being full.
        held.pop();
        assert!(
            block_on(read_block(spare)).is_ok(),
            "the table stayed exhausted after a reader released its block"
        );
    }
}
