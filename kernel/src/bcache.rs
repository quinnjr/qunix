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
//!
//! # A write is acknowledged before it reaches the disk
//!
//! [`write_block`] copies into the slot and marks it dirty; [`sync`] is what
//! puts it on the disk. That is the point of a buffer cache and it is also the
//! one place it can lose data silently: the caller has already been told the
//! write succeeded, so a dirty slot that never reaches the device is a loss no
//! later read can reveal — the cache answers that read from the same slot.
//! Every path that can take a slot out of `Dirty` therefore either wrote it or
//! leaves it dirty, including the ones that fail.
//!
//! # A write refuses a block somebody is reading
//!
//! [`BlockRef`] hands out a `&[u8]` over the slot's frame and does not hold the
//! lock while the reader uses it — holding it would mask interrupts for as long
//! as a caller chose to look at a block. So the pin is the whole exclusion:
//! [`write_block`] copies 4096 bytes over that frame, and doing it while a
//! reference is live both tears the block the reader is walking and is a
//! mutation through a raw pointer aliasing a live `&[u8]`. A write therefore
//! waits for the readers to go and reports [`BcacheError::Pinned`] if they do
//! not. `sync` is exempt: the device only *reads* the buffer, so it may share
//! it with any number of readers.

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

/// How long a write waits for a block's readers to release it.
///
/// Much shorter than [`FILL_WAIT_LIMIT`], because the thing being waited for is
/// not I/O. A reader holds a pin only for as long as it walks the block, so a
/// wait this long means the pin is not going away -- most likely because the
/// caller is holding a [`BlockRef`] to the block it is trying to write, and is
/// waiting for itself.
const PIN_WAIT_LIMIT: u32 = 32;

/// How many rounds `sync` waits on writebacks other processors own.
///
/// Only rounds that resolved *nothing* count, so this bounds waiting on other
/// processors rather than the work itself. Sized against the block driver's own
/// request deadline: a writeback that has not resolved by then has already
/// failed and put its slot back to `Dirty`, so a round after that finds work to
/// do rather than waiting again.
const SYNC_ROUND_LIMIT: u32 = 600;

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
    /// The key names a block whose sectors do not fit in an LBA.
    ///
    /// Refused rather than wrapped. `block * SECTORS_PER_BLOCK` has no overflow
    /// check in release -- `[profile.release]` sets none, and only the dev
    /// profile keeps them for workspace members -- and a wrapped product is a
    /// *valid* sector the device accepts. So the write lands on somebody else's
    /// block, successfully, and a `block` of 2^61 resolves to LBA 0: the boot
    /// sector. Refused before a slot is claimed, so the cache never holds a
    /// block it could not write back.
    OutOfRange,
    /// A writeback this call was waiting on never resolved.
    ///
    /// Distinct from [`Self::FillStalled`]: that one means another thread's
    /// *read* of this block is stuck, which points at a lost read completion.
    /// This one means a *write* is stuck, and the data is still only in the
    /// cache.
    WritebackStalled,
    /// A write was refused because a reader still holds the block.
    ///
    /// Not a wait that gave up on a transient reader -- the bound is short, so
    /// this is most often a caller holding a [`BlockRef`] to the block it is
    /// writing. Reported rather than waited out, because that caller is waiting
    /// for itself.
    Pinned,
    /// A write did not cover the whole block.
    ///
    /// Refused rather than padded or merged. Padding invents bytes the caller
    /// never supplied and writes them to the disk; merging is a
    /// read-modify-write, which is a different operation with a different
    /// failure mode, and doing it silently under a `write_block` call would
    /// turn one refused request into two issued ones.
    PartialBlock,
    /// The key names a device this kernel has no driver for.
    ///
    /// Refused here rather than ignored. `dev` is half the cache key, so a
    /// read of device 1 that was filled from device 0's disk would be reported
    /// as a hit for device 1 by every later lookup -- another device's bytes,
    /// returned successfully, with nothing downstream able to notice.
    UnknownDevice,
}

/// The first sector of `key`'s block, or a refusal.
///
/// The multiplication is checked. Unchecked, it wraps in release into a legal
/// sector the device accepts, so a read returns another block's bytes and a
/// write destroys another block -- both successfully. Called at the entry
/// points, before any slot is claimed, so an unwritable key never becomes a
/// dirty slot that eviction cannot resolve.
fn lba_of(key: BlockKey) -> Result<u64, BcacheError> {
    key.block.checked_mul(SECTORS_PER_BLOCK).ok_or(BcacheError::OutOfRange)
}

/// The only block device this kernel attaches.
///
/// [`crate::block`] addresses one virtio-blk device and takes an LBA, not a
/// (device, LBA) pair. Until it takes both, a key naming any other device
/// cannot be served, and the refusal is here so that adding a second device
/// is a change to this constant rather than a silent misfill.
const ONLY_DEVICE: u32 = 0;

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
    Fill(Fill),
    /// Somebody else is filling it.
    Wait,
}

/// A slot claimed for a fill, released if the fill never happens.
///
/// The fill is an `await`, and a future may be dropped without completing --
/// a thread torn down mid-`block_on`, or `read_block` composed into a timeout
/// later. Without this the slot stays `InFlight` forever: `victim` refuses it
/// for good, and every later reader of that key waits out the full retry bound
/// and fails. A leak that only costs capacity is exactly the kind nothing
/// notices, so it is released here rather than documented as a caveat.
struct Fill {
    slot: usize,
    virt: u64,
    /// Set once the fill has been resolved by [`finish_fill`], so `Drop` does
    /// not release a slot that is now legitimately in use.
    settled: bool,
}

impl Drop for Fill {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut guard = CACHE.lock();
        let Some(cache) = guard.as_mut() else { return };
        cache.filling[self.slot] = false;
        cache.table.end_io(self.slot, SlotState::Clean);
        assert!(
            cache.table.evict(self.slot),
            "an abandoned fill left slot {} unreleasable",
            self.slot
        );
    }
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

    let slot = claim_free_slot(cache, key)?;
    cache.filling[slot] = true;
    Ok(Claim::Fill(Fill { slot, virt: cache.frames[slot], settled: false }))
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
        // `end_io` resolves to `Dirty` if the slot was written while the io
        // ran, which would make the eviction below refuse. It cannot happen to
        // a *fill*: the only handle to a buffer is a `BlockRef`, a `BlockRef`
        // comes only from a hit, and a slot being filled never yields one. The
        // assertion says which of those stopped being true rather than
        // reporting the eviction's refusal, which describes nothing.
        assert_eq!(
            cache.table.state_of(slot),
            Some(SlotState::Clean),
            "slot {slot} was written to while it was being filled, so no reader ever saw it"
        );
        assert!(cache.table.evict(slot), "a failed fill left a slot that could not be released");
        return None;
    }
    cache.table.pin(slot);
    Some(BlockRef { slot, virt: cache.frames[slot] })
}

/// Reads a block, from the cache if it is there and from the disk if it is not.
pub async fn read_block(key: BlockKey) -> Result<BlockRef, BcacheError> {
    if key.dev != ONLY_DEVICE {
        return Err(BcacheError::UnknownDevice);
    }
    lba_of(key)?;
    let mut waited = 0u32;
    let mut flushed = 0usize;
    loop {
        let claimed = match claim(key) {
            Ok(claimed) => claimed,
            // Every slot holds something that may not simply be dropped. If any
            // of it is merely *dirty*, writing one block back turns a refusal
            // into a slot -- so the pressure is answered before it is reported.
            Err(BcacheError::Exhausted) => {
                flushed += 1;
                // Each flush cleans one slot, so more attempts than there are
                // slots means something else is consuming them and the loop is
                // not converging.
                if flushed > SLOTS || !make_room().await? {
                    return Err(BcacheError::Exhausted);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        match claimed {
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
                wait_a_tick().await;
            }
            Claim::Fill(mut fill) => {
                let (slot, virt) = (fill.slot, fill.virt);
                // SAFETY: the slot is `InFlight` and marked as filling, so no
                // other thread may read or reuse it until `finish_fill` runs.
                // The frame is reserved for the life of the kernel.
                let buf =
                    unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, BLOCK_BYTES) };
                // Checked at the entry point, so a failure here is a bug in
                // this function rather than a condition the caller can cause.
                let lba = lba_of(key).expect("an unvalidated key reached the fill");
                let outcome = block::read_at(lba, buf).await;
                // From here the slot's fate is `finish_fill`'s, not the
                // guard's.
                fill.settled = true;
                match finish_fill(slot, outcome.is_ok()) {
                    Some(block) => return Ok(block),
                    None => return Err(BcacheError::Device(outcome.unwrap_err())),
                }
            }
        }
    }
}

/// A slot whose writeback is outstanding, left dirty if the write never happens.
///
/// The mirror of [`Fill`], with the opposite recovery. An abandoned fill has
/// nothing worth keeping, so it releases the slot; an abandoned writeback still
/// holds the caller's data, and the caller has already been told the write
/// succeeded. Releasing it -- or resolving it clean -- would drop that data
/// with nothing left to say it existed. So it goes back to `Dirty` and the next
/// [`sync`] tries again.
struct Writeback {
    slot: usize,
    virt: u64,
    key: BlockKey,
    settled: bool,
}

impl Drop for Writeback {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut guard = CACHE.lock();
        let Some(cache) = guard.as_mut() else { return };
        cache.table.end_io(self.slot, SlotState::Dirty);
    }
}

/// Why a store could not proceed yet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blocked {
    /// The device or another thread owns the buffer.
    Io,
    /// A reader holds a reference into it.
    Reader,
}

/// Copies `data` into `key`'s slot and marks it dirty.
///
/// Returns `Err(Blocked)` when the caller must wait. The whole operation runs
/// under the lock and issues nothing: a full-block write has no need to read
/// the block it replaces.
fn store(key: BlockKey, data: &[u8]) -> Result<Result<(), Blocked>, BcacheError> {
    let mut guard = CACHE.lock();
    let cache = guard.as_mut().expect("the buffer cache was used before init");

    let slot = match cache.table.lookup(key) {
        Some(slot) => {
            // Any outstanding I/O, not just a fill. A fill would be overwritten
            // by the read it is waiting for, and a writeback would hand the
            // device a torn mixture of the block it was told to write and the
            // one written over it -- which lands on the disk successfully.
            if cache.table.state_of(slot) == Some(SlotState::InFlight) {
                return Ok(Err(Blocked::Io));
            }
            // A pin means a `BlockRef` is live, and a `BlockRef` hands out a
            // `&[u8]` over this very frame without holding the lock. Copying
            // over it tears the block that reader is walking -- successfully,
            // with nothing downstream able to notice -- and is a write through
            // a raw pointer aliasing a live shared reference. The pin is the
            // only thing that excludes it, so it is checked here.
            if cache.table.pins_of(slot) != Some(0) {
                return Ok(Err(Blocked::Reader));
            }
            slot
        }
        None => {
            let slot = claim_free_slot(cache, key)?;
            // `insert` hands the slot back `InFlight`, which is right for a
            // fill and wrong here: nothing is being read into it and the copy
            // below fills it completely. Resolved without releasing the lock,
            // so no reader can observe a slot that is `InFlight` while not
            // being filled -- which `claim` would read as a writeback and pin.
            cache.table.end_io(slot, SlotState::Clean);
            slot
        }
    };

    // SAFETY: `virt` is the HHDM mapping of a frame reserved for the life of
    // the kernel, `data` is a separate buffer of exactly `BLOCK_BYTES`, and the
    // lock held here excludes every other access to the slot.
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), cache.frames[slot] as *mut u8, BLOCK_BYTES);
    }
    cache.table.mark_dirty(slot);
    Ok(Ok(()))
}

/// A free slot for `key`, evicting a reusable one if the table is full.
///
/// `key` must not already be resident. `Cache::insert` returns `None` both for
/// a full table and for a key it already holds, and this reads that as the
/// former: on a resident key it would evict an innocent clean block, insert
/// again, get `None` again, and report `Exhausted` -- a wrong error and a
/// cached block thrown away for nothing. Both callers look the key up first
/// under this same lock; the assertion is what says so to the third.
fn claim_free_slot(cache: &mut Bcache, key: BlockKey) -> Result<usize, BcacheError> {
    debug_assert!(cache.table.lookup(key).is_none(), "claim_free_slot called for a resident key");
    if let Some(slot) = cache.table.insert(key) {
        return Ok(slot);
    }
    // Full. Evicting is the whole point of `victim` refusing, so a refusal here
    // is reported rather than worked around.
    let victim = cache.table.victim().ok_or(BcacheError::Exhausted)?;
    assert!(cache.table.evict(victim), "victim offered a slot evict refused");
    cache.table.insert(key).ok_or(BcacheError::Exhausted)
}

/// Copies a block into the cache. It reaches the disk at the next [`sync`].
pub async fn write_block(key: BlockKey, data: &[u8]) -> Result<(), BcacheError> {
    if key.dev != ONLY_DEVICE {
        return Err(BcacheError::UnknownDevice);
    }
    if data.len() != BLOCK_BYTES {
        return Err(BcacheError::PartialBlock);
    }
    lba_of(key)?;
    let mut waited = 0u32;
    let mut flushed = 0usize;
    loop {
        let blocked = match store(key, data) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(blocked)) => blocked,
            Err(BcacheError::Exhausted) => {
                flushed += 1;
                if flushed > SLOTS || !make_room().await? {
                    return Err(BcacheError::Exhausted);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        waited += 1;
        let limit = match blocked {
            Blocked::Io => FILL_WAIT_LIMIT,
            Blocked::Reader => PIN_WAIT_LIMIT,
        };
        if waited > limit {
            return Err(match blocked {
                Blocked::Io => BcacheError::FillStalled,
                Blocked::Reader => BcacheError::Pinned,
            });
        }
        wait_a_tick().await;
    }
}

/// Yields for one tick, or just yields if no timer slot is free.
///
/// The infallible `sleep_ticks` panics when the timer table is full, and a full
/// timer table is a transient condition rather than a reason to stop the
/// machine -- several threads waiting at once is exactly when it happens.
async fn wait_a_tick() {
    match crate::task::try_sleep_ticks(FILL_WAIT_TICKS) {
        Ok(sleep) => sleep.await,
        Err(_) => crate::sched::yield_now(),
    }
}

/// Every dirty slot, as a bitmask.
///
/// A mask rather than a list because [`sync`] must not allocate: it runs when
/// memory pressure is what triggered it, and a `Vec` of slot indices is an
/// allocation on the one path that may not make one. `SLOTS` is checked against
/// the mask's width so growing the table cannot silently drop the slots past
/// the thirty-second.
/// Slots this call owes durability for: dirty, plus already under writeback.
///
/// A mask of `Dirty` alone is not enough. A slot another processor is already
/// writing back is `InFlight`, so it is absent from that mask entirely -- and
/// `sync` returns `Ok(())` while the write is still in the air. If it then
/// fails, `finish_writeback` puts the slot back to `Dirty` and reports to *its*
/// caller, long after this one was told the data was on the disk. Acknowledged
/// data loss, and no later read reveals it because the cache answers from that
/// same slot.
///
/// A slot being *filled* is excluded: it holds nothing the caller wrote.
fn unsynced_mask() -> u32 {
    let guard = CACHE.lock();
    let cache = guard.as_ref().expect("the buffer cache was synced before init");
    (0..SLOTS)
        .filter(|slot| match cache.table.state_of(*slot) {
            Some(SlotState::Dirty) => true,
            Some(SlotState::InFlight) => !cache.filling[*slot],
            _ => false,
        })
        .fold(0u32, |mask, slot| mask | (1 << slot))
}

fn dirty_mask() -> u32 {
    const _: () = assert!(SLOTS <= u32::BITS as usize, "the dirty mask cannot address every slot");
    let guard = CACHE.lock();
    // The same refusal `claim` and `store` make, and for a sharper reason: an
    // uninitialised cache reported as having nothing dirty makes `sync` return
    // `Ok(())`, which tells the caller its data reached the disk. A flush that
    // never happened is the one answer this module must never give.
    let cache = guard.as_ref().expect("the buffer cache was synced before init");
    cache.table.dirty_slots().fold(0u32, |mask, slot| mask | (1 << slot))
}

/// Takes a slot for writeback, or `None` if it is no longer dirty.
fn begin_writeback(slot: usize) -> Option<Writeback> {
    let mut guard = CACHE.lock();
    let cache = guard.as_mut()?;
    if cache.table.state_of(slot) != Some(SlotState::Dirty) {
        return None;
    }
    let key = cache.table.key_of(slot)?;
    assert!(!cache.filling[slot], "slot {slot} is dirty and being filled at once");
    // The prior state is `Dirty` by the check above; `begin_io` returning
    // anything else means the table and this function disagree about what a
    // dirty slot is.
    assert_eq!(cache.table.begin_io(slot), SlotState::Dirty);
    Some(Writeback { slot, virt: cache.frames[slot], key, settled: false })
}

/// Ends a writeback. A write that failed leaves the block dirty.
fn finish_writeback(slot: usize, ok: bool) {
    let mut guard = CACHE.lock();
    let cache = guard.as_mut().expect("a writeback completed after the cache was torn down");
    // `Dirty` on failure, not `Clean`. The caller was told the write succeeded,
    // so a slot resolved clean here drops the data with nothing left to record
    // that it ever existed -- and a later read is answered from that same slot,
    // so it reads as the write having worked.
    cache.table.end_io(slot, if ok { SlotState::Clean } else { SlotState::Dirty });
}

/// Writes every block that was dirty when this was called.
///
/// A block dirtied *while* this runs is not covered: `end_io` leaves such a
/// slot dirty and the next `sync` writes it. The alternative -- looping until
/// nothing is dirty -- lets a steady writer keep `sync` from ever returning,
/// which is a hang rather than a stronger guarantee.
///
/// One failing block does not abandon the rest. Stopping at the first error
/// would leave later blocks dirty for a reason that has nothing to do with
/// them, so every slot in the snapshot is attempted and the first error is
/// what is reported.
/// Writes one slot back, or `None` if it is no longer dirty.
///
/// Shared by [`sync`] and by the eviction path, so the two cannot drift about
/// what a writeback leaves behind -- the failure arm in particular, where the
/// difference between `Clean` and `Dirty` is whether the data still exists.
async fn write_back(slot: usize) -> Option<Result<(), BlockError>> {
    let mut writeback = begin_writeback(slot)?;
    let (virt, key) = (writeback.virt, writeback.key);
    // SAFETY: the slot is `InFlight`, so nothing may write to or reuse it until
    // `finish_writeback` runs, and the frame is reserved for the life of the
    // kernel. A reader may hold it concurrently, which is why this is a shared
    // reference: the device only reads it too.
    let buf = unsafe { core::slice::from_raw_parts(virt as *const u8, BLOCK_BYTES) };
    // Checked at the entry point before the slot was claimed, so a failure
    // here is a bug rather than a condition -- and one that would otherwise
    // leave a dirty slot no writeback could ever resolve.
    let lba = lba_of(key).expect("an unvalidated key reached a writeback");
    let outcome = block::write_at(lba, buf).await;
    // From here the slot's fate is `finish_writeback`'s, not the guard's.
    writeback.settled = true;
    finish_writeback(slot, outcome.is_ok());
    Some(outcome)
}

/// Writes every block that was dirty when this was called.
///
/// A block dirtied *while* this runs is not covered: `end_io` leaves such a
/// slot dirty and the next `sync` writes it. The alternative -- looping until
/// nothing is dirty -- lets a steady writer keep `sync` from ever returning,
/// which is a hang rather than a stronger guarantee.
///
/// One failing block does not abandon the rest. Stopping at the first error
/// would leave later blocks dirty for a reason that has nothing to do with
/// them, so every slot in the snapshot is attempted and the first error is
/// what is reported.
pub async fn sync() -> Result<(), BcacheError> {
    // What this call is responsible for, fixed at entry. A block dirtied later
    // is not covered -- looping until nothing is dirty lets a steady writer
    // keep `sync` from ever returning, which is a hang rather than a stronger
    // guarantee.
    let owed = unsynced_mask();
    // Slots this call already tried and the device refused. Retrying one
    // within the same `sync` cannot help and would spin until the round bound
    // ran out, reporting a stall for what is really a device error.
    let mut failed = 0u32;
    let mut failure = None;
    let mut idle_rounds = 0u32;

    loop {
        let mut pending = owed & unsynced_mask() & !failed;
        if pending == 0 {
            break;
        }
        // Whether this round resolved anything. A round that only met slots
        // another processor owns has to wait for that processor rather than
        // spinning on them.
        let mut acted = false;
        while pending != 0 {
            let slot = pending.trailing_zeros() as usize;
            pending &= !(1 << slot);
            match write_back(slot).await {
                Some(Ok(())) => acted = true,
                Some(Err(error)) => {
                    // One failing block does not abandon the rest: stopping
                    // here would leave later blocks dirty for a reason that has
                    // nothing to do with them.
                    failed |= 1 << slot;
                    failure.get_or_insert(BcacheError::Device(error));
                    acted = true;
                }
                // Already clean, or another processor's writeback owns it. The
                // next round re-reads the mask, so a slot that comes back
                // `Dirty` is retried and one that comes back `Clean` is done.
                None => {}
            }
        }
        if acted {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
            if idle_rounds > SYNC_ROUND_LIMIT {
                return Err(BcacheError::WritebackStalled);
            }
            wait_a_tick().await;
        }
    }

    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// A dirty slot that nothing else is holding, and so could be reused once it
/// has been written.
///
/// Pinned slots are skipped: writing one back is harmless, but it would not
/// make room, so choosing one turns a table under pressure into a stream of
/// writebacks that free nothing.
fn flushable_slot() -> Option<usize> {
    let guard = CACHE.lock();
    let cache = guard.as_ref()?;
    cache.table.dirty_slots().find(|slot| cache.table.pins_of(*slot) == Some(0))
}

/// Writes one dirty block back so its slot can be reused.
///
/// `Ok(false)` means there was nothing to write, and so nothing this can do
/// about the pressure -- every slot is pinned or has I/O outstanding. A device
/// error is propagated rather than folded into `Exhausted`, because "the disk
/// refused the write" and "every buffer is busy" call for different answers
/// from the caller, and reporting the second for the first hides a failing
/// disk behind a capacity problem.
///
/// Allocation-free by construction: `flushable_slot` reads a bitmask, and
/// `write_back` uses the slot's own reserved frame and the driver's concrete
/// future. That is the constraint the whole design exists to satisfy -- this
/// runs when memory is already gone.
async fn make_room() -> Result<bool, BcacheError> {
    let Some(slot) = flushable_slot() else { return Ok(false) };
    match write_back(slot).await {
        // Raced with another writer; the slot is no longer dirty, which is
        // progress from this caller's point of view.
        None => Ok(true),
        Some(Ok(())) => Ok(true),
        Some(Err(error)) => Err(BcacheError::Device(error)),
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

/// Slots holding a block that has not reached the disk.
pub fn dirty_count() -> usize {
    let guard = CACHE.lock();
    match guard.as_ref() {
        Some(cache) => cache.table.dirty_slots().count(),
        None => 0,
    }
}

#[cfg(test)]
fn state_of_for_test(key: BlockKey) -> Option<SlotState> {
    let guard = CACHE.lock();
    let cache = guard.as_ref()?;
    cache.table.state_of(cache.table.lookup(key)?)
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
        // A dirty slot refuses eviction, which is the point of `evict`. Between
        // tests the data is deliberately discarded rather than written -- the
        // test that left it dirty asserted what it needed to about the disk
        // already, and flushing here would write a test's payload over a
        // sector another test reads.
        cache.table.mark_clean(slot);
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

    /// Blocks the write tests own. Nothing else reads them, so a payload left
    /// on the disk cannot change what another test sees.
    ///
    /// The image is deliberately *not* regenerated per run (see
    /// `xtask::image::build_test_disk`), so every write test also restores what
    /// it found. A test that asserted "the disk does not yet hold the payload"
    /// against a fixed payload would pass on the first boot and fail on the
    /// second, having been left holding that payload by the first.
    const WRITE_BLOCKS: core::ops::Range<u64> = 240..248;

    /// Reads a block from the device, bypassing the cache.
    ///
    /// Reading *through* the cache would return the dirty slot and assert
    /// nothing at all about the disk, which is the only thing these tests are
    /// about.
    fn read_through_the_device(key: BlockKey) -> alloc::vec::Vec<u8> {
        let mut buf = alloc::vec![0u8; BLOCK_BYTES];
        block_on(block::read_at(key.block * SECTORS_PER_BLOCK, &mut buf))
            .expect("reading the block back from the device failed");
        buf
    }

    /// `original` with **every** byte inverted, so it differs from whatever the
    /// disk currently holds whatever that is.
    ///
    /// Every byte, not just the first sector's. The first version inverted only
    /// the leading sector, which left the rest of the payload equal to what was
    /// already on the disk -- so a writeback that sent one sector instead of
    /// eight produced exactly the right disk contents and every assertion here
    /// passed. Mutation-testing the writeback length is what found that; the
    /// comparison looked whole-block and was not.
    fn altered(original: &[u8]) -> alloc::vec::Vec<u8> {
        original.iter().map(|byte| !byte).collect()
    }

    #[test_case]
    fn a_written_block_reaches_the_disk_only_after_sync() {
        // Both halves. A write that reaches the disk immediately is not a
        // cache; a write that never reaches it is data loss, and the
        // acknowledgement the caller already has is what makes it silent.
        ready();
        let key = BlockKey { dev: 0, block: WRITE_BLOCKS.start };
        let original = read_through_the_device(key);
        let payload = altered(&original);

        block_on(write_block(key, &payload)).expect("the write was refused");
        assert_eq!(dirty_count(), 1, "the write did not leave the block dirty");
        assert_eq!(
            read_through_the_device(key),
            original,
            "the write reached the disk before sync"
        );

        block_on(sync()).expect("sync failed");
        assert_eq!(dirty_count(), 0, "sync left the block dirty");
        // The whole block, not its first bytes: a writeback that wrote only the
        // first sector would satisfy any shorter comparison.
        assert_eq!(read_through_the_device(key), payload, "sync did not write the block back");

        // Restored, because the image outlives the run. Asserted, because a
        // restore that silently failed would leave the next boot's copy of this
        // test starting from the payload.
        block_on(write_block(key, &original)).expect("the restoring write was refused");
        block_on(sync()).expect("the restoring sync failed");
        assert_eq!(read_through_the_device(key), original, "the block was not restored");
    }

    #[test_case]
    fn sync_writes_every_dirty_block_and_not_only_the_first() {
        // A `sync` that stopped after one slot would pass the test above. The
        // blocks are deliberately not adjacent in slot order either: they are
        // claimed in ascending order, so a loop that ran once would leave the
        // later ones dirty and on-disk unchanged.
        ready();
        let keys: alloc::vec::Vec<BlockKey> =
            WRITE_BLOCKS.clone().map(|block| BlockKey { dev: 0, block }).collect();
        let originals: alloc::vec::Vec<_> =
            keys.iter().map(|key| read_through_the_device(*key)).collect();
        let payloads: alloc::vec::Vec<_> = originals.iter().map(|o| altered(o)).collect();

        for (key, payload) in keys.iter().zip(&payloads) {
            block_on(write_block(*key, payload)).expect("the write was refused");
        }
        assert_eq!(dirty_count(), keys.len(), "not every write left its block dirty");

        block_on(sync()).expect("sync failed");
        assert_eq!(dirty_count(), 0, "sync left a block dirty");
        for (key, payload) in keys.iter().zip(&payloads) {
            assert_eq!(
                &read_through_the_device(*key),
                payload,
                "block {} did not reach the disk",
                key.block
            );
        }

        for (key, original) in keys.iter().zip(&originals) {
            block_on(write_block(*key, original)).expect("the restoring write was refused");
        }
        block_on(sync()).expect("the restoring sync failed");
        for (key, original) in keys.iter().zip(&originals) {
            assert_eq!(&read_through_the_device(*key), original, "block {} was not restored", key.block);
        }
    }

    #[test_case]
    fn a_read_after_a_write_sees_the_write_without_touching_the_device() {
        // The dirty slot is the truth until it is flushed. A read that went to
        // the device would return the block's *old* contents and report them as
        // this block -- a stale answer, successfully.
        ready();
        let key = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 1 };
        let original = read_through_the_device(key);
        let payload = altered(&original);

        block_on(write_block(key, &payload)).expect("the write was refused");
        let completions = block::completions();
        let cached = block_on(read_block(key)).expect("the read after the write failed");
        assert_eq!(&cached[..], &payload[..], "the read did not see the write");
        assert_eq!(block::completions(), completions, "the read went to the device");
        drop(cached);

        // Discarded rather than flushed: the point of this test is the cache,
        // and leaving the payload on the disk would perturb the others.
        reset_for_test();
        assert_eq!(read_through_the_device(key), original, "the discarded write reached the disk");
    }

    #[test_case]
    fn a_failed_writeback_leaves_the_block_dirty() {
        // The direction that loses data. The caller has already been told the
        // write succeeded, so a slot resolved clean here drops it with nothing
        // left to say it existed -- and a later read is answered from that same
        // slot, so it reads as though the write worked.
        ready();
        // Past the end of the disk, so the *device* refuses the writeback. The
        // block number is what makes this fail, not the payload.
        let key = BlockKey { dev: 0, block: 4096 };
        let payload = alloc::vec![0xA5u8; BLOCK_BYTES];
        block_on(write_block(key, &payload)).expect("the write into the cache was refused");
        assert_eq!(dirty_count(), 1, "the write did not leave the block dirty");

        let outcome = block_on(sync());
        assert!(outcome.is_err(), "a writeback the device refused was reported as a success");
        assert_eq!(dirty_count(), 1, "a failed writeback dropped the block");
        assert!(
            matches!(state_of_for_test(key), Some(SlotState::Dirty)),
            "a failed writeback left the slot in some other state"
        );
        // A second sync tries again rather than treating it as done.
        assert!(block_on(sync()).is_err(), "the second sync did not retry the block");
        assert_eq!(dirty_count(), 1, "the retry dropped the block");
    }

    /// Fills and pins every slot but one, returning the held references.
    ///
    /// Real pressure has to be constructed. Simply reading many distinct blocks
    /// does not create it: `victim` prefers a clean slot, unpinned clean slots
    /// keep being recycled, and a dirty slot is never even considered — so a
    /// test that read `SLOTS + 1` blocks would assert nothing about eviction,
    /// whether or not the flush existed.
    fn pin_every_slot_but_one() -> alloc::vec::Vec<BlockRef> {
        let mut held = alloc::vec::Vec::new();
        for block in 100..100 + (SLOTS as u64 - 1) {
            held.push(
                block_on(read_block(BlockKey { dev: 0, block })).expect("filling the table"),
            );
        }
        held
    }

    #[test_case]
    fn evicting_a_dirty_slot_writes_it_back_first() {
        // The failure this exists to prevent is silent: the slot is reused, the
        // write is discarded, and the block reads as its old contents forever
        // -- answered from the cache, so no later read reveals it.
        ready();
        let key = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 4 };
        let original = read_through_the_device(key);
        let payload = altered(&original);
        block_on(write_block(key, &payload)).expect("the write was refused");

        let held = pin_every_slot_but_one();
        assert_eq!(resident(), SLOTS, "the table did not fill");
        assert_eq!(dirty_count(), 1, "the written block is not the one dirty slot");

        // One more distinct block. The only slot that can be reused is the
        // dirty one, so serving this read at all requires writing it back.
        let extra = block_on(read_block(BlockKey { dev: 0, block: 200 }))
            .expect("a read under pressure was refused with a dirty slot available");
        assert_eq!(dirty_count(), 0, "the dirty slot was reused without being written");
        assert_eq!(
            read_through_the_device(key),
            payload,
            "a dirty slot was evicted without being written back"
        );
        drop(extra);
        drop(held);

        block_on(write_block(key, &original)).expect("the restoring write was refused");
        block_on(sync()).expect("the restoring sync failed");
        assert_eq!(read_through_the_device(key), original, "the block was not restored");
    }

    #[test_case]
    fn a_dirty_slot_the_device_refuses_is_not_evicted_anyway() {
        // The refusal direction, and the one that loses data. When the
        // writeback fails there is nowhere for the block to go, so the only
        // answers are "report the failure" and "discard the block" -- and the
        // second is invisible, because the caller was already told the write
        // succeeded and the cache answers every later read from that slot.
        ready();
        // Past the end of the disk, so the device refuses the writeback. The
        // block number is what makes this fail, not the payload.
        let key = BlockKey { dev: 0, block: 4096 };
        let payload = alloc::vec![0x5Au8; BLOCK_BYTES];
        block_on(write_block(key, &payload)).expect("the write into the cache was refused");

        let held = pin_every_slot_but_one();
        assert_eq!(dirty_count(), 1, "the written block is not the one dirty slot");

        let outcome = block_on(read_block(BlockKey { dev: 0, block: 201 }));
        assert!(
            matches!(outcome, Err(BcacheError::Device(_))),
            "a read under pressure hid a failing disk behind a capacity answer"
        );
        assert_eq!(dirty_count(), 1, "the block the device refused was evicted anyway");
        assert!(
            matches!(state_of_for_test(key), Some(SlotState::Dirty)),
            "the refused block is no longer dirty"
        );
        drop(held);
    }

    #[test_case]
    fn a_pinned_dirty_slot_is_not_the_one_chosen_to_flush() {
        // Writing back a pinned block frees nothing: the pin is what refuses
        // the eviction, and the writeback does not remove it. Choosing one
        // spends a disk write to make no room, and with enough of them the
        // retry bound runs out and a table that had a reusable slot all along
        // reports `Exhausted`.
        //
        // Mutation-testing found this: `flushable_slot` without its pin filter
        // passed every other test here, because none of them ever had a slot
        // that was dirty *and* pinned.
        ready();
        let pinned = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 6 };
        let loose = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 7 };
        let pinned_original = read_through_the_device(pinned);
        let loose_original = read_through_the_device(loose);
        let pinned_payload = altered(&pinned_original);
        let loose_payload = altered(&loose_original);

        block_on(write_block(pinned, &pinned_payload)).expect("the write was refused");
        block_on(write_block(loose, &loose_payload)).expect("the write was refused");
        // Pinning it *after* the write: `write_block` refuses a block a reader
        // holds, so the order is not interchangeable.
        let holding = block_on(read_block(pinned)).expect("the read of the dirty block failed");
        assert_eq!(dirty_count(), 2, "both blocks should be dirty");

        // Fill and pin the rest, so the only slot that can be freed is the
        // unpinned dirty one.
        let mut held = alloc::vec::Vec::new();
        for block in 100..100 + (SLOTS as u64 - 2) {
            held.push(block_on(read_block(BlockKey { dev: 0, block })).expect("filling the table"));
        }
        assert_eq!(resident(), SLOTS, "the table did not fill");

        let extra = block_on(read_block(BlockKey { dev: 0, block: 202 }))
            .expect("a read under pressure was refused with a flushable slot available");
        // The pinned block is untouched: still dirty, still not on the disk.
        assert!(
            matches!(state_of_for_test(pinned), Some(SlotState::Dirty)),
            "the pinned block was written back, which frees nothing"
        );
        assert_eq!(
            read_through_the_device(pinned),
            pinned_original,
            "the pinned block reached the disk, so it was the one chosen to flush"
        );
        // And the unpinned one was.
        assert_eq!(
            read_through_the_device(loose),
            loose_payload,
            "the unpinned dirty block was not the one flushed"
        );
        drop(extra);
        drop(holding);
        drop(held);

        reset_for_test();
        block_on(write_block(loose, &loose_original)).expect("the restoring write was refused");
        block_on(sync()).expect("the restoring sync failed");
        assert_eq!(read_through_the_device(loose), loose_original, "the block was not restored");
        assert_eq!(read_through_the_device(pinned), pinned_original, "the block was not restored");
    }

    #[test_case]
    fn a_write_under_pressure_also_flushes_rather_than_refusing() {
        // `write_block` claims a slot too, so it meets the same pressure and
        // must answer it the same way. A version that only taught the read path
        // to flush would leave writes failing with `Exhausted` while a dirty
        // slot sat there waiting to be written.
        ready();
        let dirty = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 5 };
        let original = read_through_the_device(dirty);
        let payload = altered(&original);
        block_on(write_block(dirty, &payload)).expect("the first write was refused");

        let held = pin_every_slot_but_one();
        assert_eq!(dirty_count(), 1, "the written block is not the one dirty slot");

        // A different block, so this needs a slot of its own.
        let second = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 6 };
        let second_original = read_through_the_device(second);
        let second_payload = altered(&second_original);
        block_on(write_block(second, &second_payload))
            .expect("a write under pressure was refused with a dirty slot available");
        assert_eq!(
            read_through_the_device(dirty),
            payload,
            "the write under pressure discarded the dirty block"
        );
        drop(held);

        block_on(write_block(dirty, &original)).expect("the restoring write was refused");
        block_on(write_block(second, &second_original)).expect("the restoring write was refused");
        block_on(sync()).expect("the restoring sync failed");
        assert_eq!(read_through_the_device(dirty), original, "the block was not restored");
        assert_eq!(read_through_the_device(second), second_original, "the block was not restored");
    }

    #[test_case]
    fn a_write_is_refused_while_a_reader_holds_the_block() {
        // `BlockRef` hands out a `&[u8]` over the frame and does not hold the
        // lock while the reader walks it, so the pin is the whole exclusion.
        // Copying over the frame while a reference is live tears the block that
        // reader is walking -- successfully, with nothing downstream able to
        // notice -- and is a write through a raw pointer aliasing a live shared
        // reference.
        ready();
        let key = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 3 };
        let original = read_through_the_device(key);
        let payload = altered(&original);

        let held = block_on(read_block(key)).expect("the read failed");
        let before: alloc::vec::Vec<u8> = held.to_vec();
        assert!(
            matches!(block_on(write_block(key, &payload)), Err(BcacheError::Pinned)),
            "a block being read was overwritten"
        );
        // The direction that matters: not merely that the call failed, but that
        // the reader's bytes are the ones it started with.
        assert_eq!(&held[..], &before[..], "the held block changed under its reader");
        assert_eq!(dirty_count(), 0, "the refused write dirtied the slot");

        // And the write succeeds once the reader is gone, which is what says
        // the refusal was about the pin rather than about the block.
        drop(held);
        block_on(write_block(key, &payload)).expect("the write was refused after the reader left");
        assert_eq!(dirty_count(), 1, "the accepted write did not dirty the slot");
        reset_for_test();
        assert_eq!(read_through_the_device(key), original, "the discarded write reached the disk");
    }

    #[test_case]
    fn a_block_number_that_would_wrap_its_lba_is_refused() {
        // `block * SECTORS_PER_BLOCK` has no overflow check in release, and the
        // wrapped product is a *valid* sector the device accepts. A block of
        // 2^61 resolves to LBA 0 -- the boot sector -- so an unchecked write
        // destroys it and reports success, while the cache files the bytes
        // under the huge key. In a test build the same multiply panics, which
        // is a halted machine. Refused before a slot is claimed either way.
        ready();
        let key = BlockKey { dev: 0, block: u64::MAX / SECTORS_PER_BLOCK + 1 };
        let before = block::completions();
        assert!(
            matches!(block_on(read_block(key)), Err(BcacheError::OutOfRange)),
            "a block number that wraps its lba was read"
        );
        let payload = alloc::vec![0u8; BLOCK_BYTES];
        assert!(
            matches!(block_on(write_block(key, &payload)), Err(BcacheError::OutOfRange)),
            "a block number that wraps its lba was written"
        );
        assert_eq!(block::completions(), before, "the refused request reached the device");
        assert_eq!(resident(), 0, "the refused request claimed a slot");
        assert_eq!(dirty_count(), 0, "the refused write dirtied a slot");
        // The largest block that does fit is still served, so the bound refuses
        // the wrap rather than refusing large numbers.
        let largest = BlockKey { dev: 0, block: u64::MAX / SECTORS_PER_BLOCK };
        assert!(
            !matches!(block_on(read_block(largest)), Err(BcacheError::OutOfRange)),
            "a block whose lba fits was refused as out of range"
        );
        reset_for_test();
    }

    #[test_case]
    fn sync_waits_for_a_writeback_it_did_not_start() {
        // `dirty_mask` collects only `Dirty` slots, so a slot another processor
        // is already writing back is absent from it and `sync` returns
        // `Ok(())` while the write is still in the air. If that write then
        // fails, the slot goes back to `Dirty` after this caller was told its
        // data was durable.
        //
        // Driven here by half-polling a `sync` so the slot is left `InFlight`
        // and not filling -- the state another processor's writeback produces
        // -- and then requiring a second `sync` to still owe it.
        ready();
        let key = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 7 };
        let original = read_through_the_device(key);
        let payload = altered(&original);
        block_on(write_block(key, &payload)).expect("the write was refused");

        let mut first = core::pin::pin!(sync());
        let waker = crate::task::waker_for(crate::sched::current_id());
        let mut cx = core::task::Context::from_waker(&waker);
        assert!(first.as_mut().poll(&mut cx).is_pending(), "the writeback did not park");
        assert!(
            matches!(state_of_for_test(key), Some(SlotState::InFlight)),
            "the half-polled sync did not leave the slot under writeback"
        );
        // The slot is InFlight and not filling: exactly what a concurrent
        // writeback looks like. A `sync` that ignores it reports success.
        assert_ne!(unsynced_mask(), 0, "sync would report success with a write still in the air");

        block_on(first);
        assert_eq!(unsynced_mask(), 0, "the completed writeback is still owed");
        assert_eq!(read_through_the_device(key), payload, "the writeback did not land");

        block_on(write_block(key, &original)).expect("the restoring write was refused");
        block_on(sync()).expect("the restoring sync failed");
        assert_eq!(read_through_the_device(key), original, "the block was not restored");
    }

    #[test_case]
    fn a_write_that_does_not_cover_the_block_is_refused() {
        // Padding invents bytes the caller never supplied and puts them on the
        // disk; merging is a read-modify-write, a different operation with a
        // different failure mode. Neither happens silently under this name.
        ready();
        let key = BlockKey { dev: 0, block: WRITE_BLOCKS.start + 2 };
        let short = alloc::vec![0u8; BLOCK_BYTES - 1];
        let long = alloc::vec![0u8; BLOCK_BYTES + 1];
        assert!(
            matches!(block_on(write_block(key, &short)), Err(BcacheError::PartialBlock)),
            "a short write was accepted"
        );
        assert!(
            matches!(block_on(write_block(key, &long)), Err(BcacheError::PartialBlock)),
            "an oversized write was accepted"
        );
        assert_eq!(dirty_count(), 0, "a refused write still dirtied a slot");
        assert_eq!(resident(), 0, "a refused write still claimed a slot");
    }

    #[test_case]
    fn a_key_naming_a_device_that_does_not_exist_is_refused() {
        // `dev` is half the key but the driver takes only an LBA, so a read of
        // device 1 would be filled from device 0's disk and reported as a hit
        // for device 1 by every later lookup. Refused rather than served, and
        // asserted rather than left to a comment.
        ready();
        let before = block::completions();
        assert!(
            matches!(
                block_on(read_block(BlockKey { dev: 1, block: 1 })),
                Err(BcacheError::UnknownDevice)
            ),
            "a read of a device with no driver was served"
        );
        assert_eq!(block::completions(), before, "the refused read still went to a device");
        assert_eq!(resident(), 0, "the refused read claimed a slot");
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
