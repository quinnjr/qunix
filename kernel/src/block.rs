//! Asynchronous block I/O over virtio-blk.
//!
//! A request is a three-descriptor chain — header, data, status — published to
//! the one virtqueue and completed by an MSI-X interrupt, which unparks the
//! thread waiting in `task::block_on`.
//!
//! # Every device-visible address is one the kernel derived
//!
//! There is no IOMMU. The device addresses physical memory directly and will
//! write anywhere it is told to, so the only thing between a driver bug and an
//! arbitrary memory write is that every address published in a descriptor came
//! from a frame this module allocated.
//!
//! That is why a read lands in a kernel-owned bounce buffer and is copied to
//! the caller afterwards, rather than the caller's slice being published. A
//! caller's buffer can straddle frames, sit partly outside one, or — once
//! syscalls reach this path — be a user pointer. One place derives
//! device-visible addresses, and it derives them from frames it owns.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};

use qunix_hal_x86_64::pci;
use qunix_sched::ThreadId;
use qunix_sync::IrqSpinLock;
use qunix_virtio::blk::{BlkStatus, RequestHeader, SECTOR_BYTES, status_from_byte};
use qunix_virtio::{QUEUE_SIZE, SplitQueue, ring_layout};

use crate::virtio::{Transport, VIRTIO_BLK_MODERN, VIRTIO_F_VERSION_1};

/// The vector completions arrive on. 32 is the timer, 33 the TLB shootdown,
/// 34 the wake IPI.
pub const COMPLETION_VECTOR: u8 = 35;

/// Bytes of bounce buffer per in-flight request, and so the largest transfer.
///
/// One page. A larger request would need either a contiguous multi-page
/// allocation per slot — 64 of them — or a descriptor chain per page, and
/// neither is worth building before a filesystem exists to say what sizes it
/// actually asks for.
const SLOT_BYTES: usize = 4096;
/// The largest transfer, in sectors.
pub const MAX_SECTORS: usize = SLOT_BYTES / SECTOR_BYTES;

/// Why a request could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// The buffer is not a whole number of sectors.
    ///
    /// Refused here rather than by the device: a partial sector makes the
    /// device write past the end of the buffer, and nothing between this
    /// function and the DMA engine would object.
    Unaligned,
    /// The buffer is larger than one slot's bounce buffer.
    TooLarge,
    /// Every descriptor is in flight.
    QueueFull,
    /// No virtio-blk device was found or it could not be brought up.
    NoDevice,
    /// The device's MSI-X table would fall outside the BAR that holds it.
    MsixOutsideBar,
    /// The device reported a failure for this request.
    Device(BlkStatus),
}

/// One in-flight request.
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// The thread to unpark when this completes.
    thread: ThreadId,
    /// Set by the interrupt handler.
    done: bool,
    /// The status byte the device wrote.
    status: u8,
}

/// The device and everything that must be consistent with it, under one lock.
struct Blk {
    transport: Transport,
    queue: SplitQueue,
    /// Virtual address of the ring allocation, through the HHDM.
    ring_virt: u64,
    /// Physical base of the per-slot request areas (header, then status).
    meta_phys: u64,
    meta_virt: u64,
    /// Physical base of the per-slot bounce buffers.
    data_phys: u64,
    data_virt: u64,
    /// Used-ring entries already consumed.
    last_used: u16,
    slots: [Option<Slot>; QUEUE_SIZE as usize],
}

// SAFETY: `Blk` is reachable only through `DEVICE`, an `IrqSpinLock`, so every
// access is serialised. The raw addresses it holds name DMA memory and device
// registers, both of which are equally valid from any processor.
unsafe impl Send for Blk {}

static DEVICE: IrqSpinLock<Option<Blk>, qunix_hal_x86_64::Irq> = IrqSpinLock::new(None);

/// Completions taken from the used ring since boot.
///
/// Exposed so a test can require the *interrupt handler* to have run, rather
/// than inferring it from the request having finished — which a polling loop
/// would also satisfy.
static COMPLETIONS: AtomicU64 = AtomicU64::new(0);

pub fn completions() -> u64 {
    COMPLETIONS.load(Ordering::Acquire)
}

/// Bytes of metadata per slot: a 16-byte header and a 1-byte status.
const META_STRIDE: u64 = 32;

/// Brings up the block device. Idempotent.
///
/// # Safety
/// Must be called from the bootstrap processor with interrupts enabled and the
/// LAPIC mapped, before any request is submitted.
pub unsafe fn init() -> Result<(), BlockError> {
    let mut guard = DEVICE.lock();
    if guard.is_some() {
        return Ok(());
    }

    // SAFETY: the caller's obligation, forwarded.
    let mut transport = unsafe { crate::virtio::probe(VIRTIO_BLK_MODERN) }
        .map_err(|_| BlockError::NoDevice)?;
    transport.negotiate(VIRTIO_F_VERSION_1).map_err(|_| BlockError::NoDevice)?;

    let layout = ring_layout(QUEUE_SIZE);
    let ring_phys = crate::frames::alloc(0).ok_or(BlockError::NoDevice)?;
    // 64 slots of 32 bytes is one page of metadata.
    let meta_phys = crate::frames::alloc(0).ok_or(BlockError::NoDevice)?;
    // 64 slots of 4 KiB is 256 KiB: order 6.
    let data_phys = crate::frames::alloc(6).ok_or(BlockError::NoDevice)?;
    let hhdm = crate::boot::hhdm_offset();

    // Zeroed before the device is told about any of it. The device reads the
    // available ring's index the moment the queue is enabled, and a frame
    // carrying whatever its last owner left would have it fetch descriptors
    // that were never written -- from addresses that were never derived.
    // SAFETY: the frames were just allocated, so nothing else refers to them,
    // and the HHDM maps all of RAM.
    unsafe {
        core::ptr::write_bytes((hhdm + ring_phys) as *mut u8, 0, 4096);
        core::ptr::write_bytes((hhdm + meta_phys) as *mut u8, 0, 4096);
        core::ptr::write_bytes((hhdm + data_phys) as *mut u8, 0, SLOT_BYTES * QUEUE_SIZE as usize);
    }

    transport.configure_queue(layout, ring_phys).map_err(|_| BlockError::NoDevice)?;
    // SAFETY: the caller guarantees the LAPIC is mapped and this runs on the
    // bootstrap processor.
    unsafe { install_msix(&mut transport)? };
    transport.finish().map_err(|_| BlockError::NoDevice)?;

    *guard = Some(Blk {
        transport,
        queue: SplitQueue::new(QUEUE_SIZE),
        ring_virt: hhdm + ring_phys,
        meta_phys,
        meta_virt: hhdm + meta_phys,
        data_phys,
        data_virt: hhdm + data_phys,
        last_used: 0,
        slots: [None; QUEUE_SIZE as usize],
    });
    Ok(())
}

/// Programs the device's MSI-X table entry 0 to deliver to
/// [`COMPLETION_VECTOR`], and points the queue at it.
///
/// # Safety
/// The LAPIC must be mapped and this CPU's IDT must already carry the vector.
unsafe fn install_msix(transport: &mut Transport) -> Result<(), BlockError> {
    let bdf = transport.bdf();
    // SAFETY: port I/O from ring 0.
    let cfg = unsafe { pci::config_snapshot(bdf) };
    let caps = pci::walk_capabilities(&cfg).map_err(|_| BlockError::NoDevice)?;
    let cap = caps.iter().find(|c| c.id == pci::CAP_ID_MSIX).ok_or(BlockError::NoDevice)?;
    let msix = pci::parse_msix(&cfg, *cap).ok_or(BlockError::NoDevice)?;

    // SAFETY: port I/O.
    let (bar, bar_size) = unsafe { transport.bar_base(msix.bar) }.ok_or(BlockError::NoDevice)?;
    let table_bytes = (msix.entries as u32).saturating_mul(pci::MSIX_ENTRY_BYTES as u32);
    // The table must lie inside its BAR. `msix.offset` and `msix.entries` are
    // both device-supplied, and `MsixTable::program` writes four dwords per
    // entry -- so without this the device chooses where those writes land.
    let end = (msix.offset as u64).saturating_add(table_bytes as u64);
    if end > bar_size {
        return Err(BlockError::MsixOutsideBar);
    }
    let table_phys = bar + msix.offset as u64;
    let table_virt = crate::virtio::map_device_window(table_phys, table_bytes)
        .ok_or(BlockError::NoDevice)?;

    let table = pci::MsixTable { base: table_virt, entries: msix.entries };
    let address = pci::msix_message_address(
        qunix_hal_x86_64::apic::phys_base(),
        // Delivered to the bootstrap processor. Any processor would do -- the
        // handler unparks a thread the scheduler may then run anywhere -- but
        // naming one keeps the destination a fact rather than a race.
        0,
    );
    // SAFETY: the table was just mapped uncacheable with `entries` entries.
    unsafe { table.program(0, address, pci::msix_message_data(COMPLETION_VECTOR)) }
        .map_err(|_| BlockError::NoDevice)?;

    // Enable MSI-X, and clear the function mask. Enabling without clearing the
    // mask is the shape of mistake that produces a device which accepts every
    // request and never interrupts.
    // SAFETY: port I/O.
    unsafe {
        let control_dword = msix.control_offset & !0b11;
        let raw = pci::config_read32(bdf, control_dword);
        let shift = (msix.control_offset & 0b11) * 8;
        let control = ((raw >> shift) as u16) | 0x8000; // enable
        let control = control & !0x4000; // function mask clear
        let cleared = raw & !(0xffffu32 << shift);
        pci::config_write32(bdf, control_dword, cleared | ((control as u32) << shift));
    }

    transport.set_queue_msix_vector(0);
    Ok(())
}

/// Reads `buf.len()` bytes starting at sector `lba`.
pub async fn read_at(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    let head = submit(lba, buf.len(), None)?;
    let status = Completion { head }.await;
    let result = finish(head, status, Some(buf));
    result
}

/// Writes `buf` starting at sector `lba`.
pub async fn write_at(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let head = submit(lba, buf.len(), Some(buf))?;
    let status = Completion { head }.await;
    finish(head, status, None)
}

/// Validates, builds and publishes a request chain. Returns its head.
fn submit(lba: u64, len: usize, out: Option<&[u8]>) -> Result<u16, BlockError> {
    if len == 0 || len % SECTOR_BYTES != 0 {
        return Err(BlockError::Unaligned);
    }
    if len > SLOT_BYTES {
        return Err(BlockError::TooLarge);
    }

    let mut guard = DEVICE.lock();
    let blk = guard.as_mut().ok_or(BlockError::NoDevice)?;

    let head = blk.queue.alloc_chain(3).ok_or(BlockError::QueueFull)?;
    let data = blk.queue.next_in_chain(head).expect("a three-chain has a second descriptor");
    let status = blk.queue.next_in_chain(data).expect("a three-chain has a third descriptor");

    // The slot index is the *head descriptor*, so the metadata and bounce
    // buffer belong to this request for as long as the chain does. Indexing by
    // anything else -- a rotating counter, say -- would let two in-flight
    // requests share a bounce buffer, and the device would fill both with one
    // sector's contents.
    let slot = head as u64;
    let header_phys = blk.meta_phys + slot * META_STRIDE;
    let status_phys = header_phys + RequestHeader::BYTES as u64;
    let data_phys = blk.data_phys + slot * SLOT_BYTES as u64;
    let header_virt = blk.meta_virt + slot * META_STRIDE;
    let data_virt = blk.data_virt + slot * SLOT_BYTES as u64;

    let request =
        if out.is_some() { RequestHeader::write(lba) } else { RequestHeader::read(lba) };
    // SAFETY: `header_virt` is inside the metadata frame this module allocated
    // and mapped through the HHDM.
    unsafe {
        core::ptr::copy_nonoverlapping(
            request.to_bytes().as_ptr(),
            header_virt as *mut u8,
            RequestHeader::BYTES,
        );
        // The status byte must not carry a stale success from the previous use
        // of this slot: the device writes it, but a request that never reaches
        // the device would otherwise read as `Ok`.
        //
        // Unfalsifiable here, and said so: every request in this suite does
        // reach the device, which overwrites the byte, so removing this line
        // leaves the tests green. It guards the case where a request is
        // published and the device never fetches it -- a lost notification, a
        // ring the device rejected -- where the difference is between reporting
        // an error and reporting success on data that was never transferred.
        core::ptr::write_volatile((header_virt + RequestHeader::BYTES as u64) as *mut u8, 0xff);
    }
    if let Some(bytes) = out {
        // SAFETY: the bounce buffer is `SLOT_BYTES` and `len` was bounded by it.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), data_virt as *mut u8, len) };
    }

    blk.queue.describe(head, header_phys, RequestHeader::BYTES as u32, false);
    // Device-writable exactly when the device is the one producing the data.
    // Marking a read's buffer read-only makes the device refuse the chain,
    // which at least fails loudly; marking a write's buffer device-writable
    // hands it a page it may overwrite with nothing in particular.
    blk.queue.describe(data, data_phys, len as u32, out.is_none());
    blk.queue.describe(status, status_phys, 1, true);

    blk.slots[head as usize] =
        Some(Slot { thread: crate::sched::current_id(), done: false, status: 0xff });

    blk.queue.publish(head);
    write_rings(blk);
    blk.transport.notify();
    Ok(head)
}

/// Copies the driver's descriptor table and available ring into DMA memory.
fn write_rings(blk: &Blk) {
    let layout = ring_layout(QUEUE_SIZE);
    let desc = blk.queue.desc_bytes();
    let avail = blk.queue.avail_bytes();
    // SAFETY: both offsets are inside the ring frame this module allocated, and
    // `ring_layout` is what told the device where they are.
    unsafe {
        core::ptr::copy_nonoverlapping(
            desc.as_ptr(),
            (blk.ring_virt + layout.desc as u64) as *mut u8,
            desc.len(),
        );
        core::ptr::copy_nonoverlapping(
            avail.as_ptr(),
            (blk.ring_virt + layout.avail as u64) as *mut u8,
            avail.len(),
        );
    }
    // The device may read the ring the instant it is notified, and the
    // notification is a store to a device register. Without a fence the two
    // stores can reach memory in the other order, and the device reads a
    // descriptor table that does not yet describe the request it was told
    // about.
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// A request's completion.
struct Completion {
    head: u16,
}

impl Future for Completion {
    type Output = u8;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u8> {
        let guard = DEVICE.lock();
        let Some(blk) = guard.as_ref() else { return Poll::Ready(0xff) };
        match blk.slots[self.head as usize] {
            Some(slot) if slot.done => Poll::Ready(slot.status),
            // The waker is deliberately unused: `handle_completion` unparks the
            // thread recorded in the slot, which is the same thread this future
            // is being polled on and the same thing the waker would do. That is
            // only true because the thread *is* the task -- see `task`'s module
            // documentation.
            _ => Poll::Pending,
        }
    }
}

/// Copies the result out, releases the chain, and reports the device's verdict.
fn finish(head: u16, status: u8, into: Option<&mut [u8]>) -> Result<(), BlockError> {
    let mut guard = DEVICE.lock();
    let blk = guard.as_mut().ok_or(BlockError::NoDevice)?;
    if let Some(buf) = into {
        let data_virt = blk.data_virt + head as u64 * SLOT_BYTES as u64;
        // SAFETY: the bounce buffer is `SLOT_BYTES` and `buf.len()` was bounded
        // by it at submission.
        unsafe { core::ptr::copy_nonoverlapping(data_virt as *const u8, buf.as_mut_ptr(), buf.len()) };
    }
    blk.queue.free_chain(head);
    blk.slots[head as usize] = None;
    match status_from_byte(status) {
        BlkStatus::Ok => Ok(()),
        other => Err(BlockError::Device(other)),
    }
}

/// Drains the used ring and wakes whoever was waiting.
///
/// Runs in interrupt context. Everything it touches is behind `IrqSpinLock`, so
/// a tick cannot land on a processor already holding it.
pub fn handle_completion() {
    let mut woken: [Option<ThreadId>; QUEUE_SIZE as usize] = [None; QUEUE_SIZE as usize];
    let mut count = 0;
    {
        let mut guard = DEVICE.lock();
        let Some(blk) = guard.as_mut() else { return };
        let layout = ring_layout(QUEUE_SIZE);
        let used_len = ring_layout(QUEUE_SIZE).bytes - layout.used;
        // SAFETY: the used ring is inside the frame this module allocated and
        // told the device about.
        let bytes = unsafe {
            core::slice::from_raw_parts((blk.ring_virt + layout.used as u64) as *const u8, used_len)
        };
        let Some(device_idx) = blk.queue.ingest_used(bytes) else { return };
        // Consumed in order from the last index seen. Reading only the newest
        // entry would drop every completion that arrived while this handler was
        // between the read and the lock.
        while blk.last_used != device_idx {
            let slot = blk.last_used % QUEUE_SIZE;
            if let Some((head, _len)) = blk.queue.take_used(slot) {
                let status_virt =
                    blk.meta_virt + head as u64 * META_STRIDE + RequestHeader::BYTES as u64;
                // SAFETY: inside the metadata frame this module allocated.
                let status = unsafe { core::ptr::read_volatile(status_virt as *const u8) };
                if let Some(entry) = blk.slots[head as usize].as_mut() {
                    entry.done = true;
                    entry.status = status;
                    woken[count] = Some(entry.thread);
                    count += 1;
                }
                COMPLETIONS.fetch_add(1, Ordering::AcqRel);
            }
            blk.last_used = blk.last_used.wrapping_add(1);
        }
    }
    // Unparked after the device lock is released: `unpark` takes the scheduler
    // lock and may take a run queue, and holding three from an interrupt is an
    // order nothing else in the kernel has.
    for id in woken.iter().flatten() {
        crate::sched::unpark(*id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::block_on;

    fn ready() {
        // SAFETY: the suite runs on the bootstrap processor with the LAPIC
        // mapped and the vector installed.
        unsafe { init() }.expect("the block device did not come up");
    }

    #[test_case]
    fn a_sector_reads_back_the_lba_it_was_written_with() {
        // The disk is generated with each sector beginning with its own LBA, so
        // reading the *wrong* sector is detectable rather than plausible. A
        // test that only checked "the bytes are not zero" would pass on an
        // off-by-one anywhere in the descriptor chain.
        ready();
        let mut buf = [0u8; SECTOR_BYTES];
        block_on(read_at(7, &mut buf)).expect("the read failed");
        assert_eq!(
            u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            7,
            "sector 7 does not identify itself; the request named the wrong sector"
        );
        // And the tail, which also depends on the LBA -- a read that returned
        // the right first eight bytes and the wrong rest would otherwise pass.
        assert_eq!(buf[8], 7, "the sector's filler does not match its LBA");
        assert_eq!(buf[9], 8, "the filler is not the LBA plus its offset");
    }

    #[test_case]
    fn a_read_completes_from_the_interrupt_handler() {
        // The claim the milestone is about. `block_on` parks, and only the
        // MSI-X handler can make the thread runnable again -- so the completion
        // counter must move. Without it the request would have to have been
        // finished by something else, which is the polling loop this design
        // exists to avoid.
        ready();
        let before = completions();
        let mut buf = [0u8; SECTOR_BYTES];
        block_on(read_at(3, &mut buf)).expect("the read failed");
        assert!(
            completions() > before,
            "the request completed without the interrupt handler taking it from the used ring"
        );
        assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 3);
    }

    #[test_case]
    fn a_write_is_visible_to_a_later_read() {
        ready();
        let mut out = [0xa5u8; SECTOR_BYTES];
        out[0..8].copy_from_slice(&0xdead_beefu64.to_le_bytes());
        block_on(write_at(11, &out)).expect("the write failed");
        let mut back = [0u8; SECTOR_BYTES];
        block_on(read_at(11, &mut back)).expect("the read failed");
        assert_eq!(back, out, "what came back is not what went out");
    }

    #[test_case]
    fn a_misaligned_or_oversized_request_is_refused_before_it_reaches_the_device() {
        // Refused in the driver, not by the device. A buffer that is not a
        // whole number of sectors makes the device write past its end -- and it
        // will, because nothing between here and the DMA engine checks.
        ready();
        let mut short = [0u8; SECTOR_BYTES - 1];
        assert_eq!(block_on(read_at(0, &mut short)), Err(BlockError::Unaligned));
        let mut empty = [0u8; 0];
        assert_eq!(block_on(read_at(0, &mut empty)), Err(BlockError::Unaligned));
        assert_eq!(MAX_SECTORS * SECTOR_BYTES, SLOT_BYTES, "MAX_SECTORS does not describe the bound");
        let mut huge = [0u8; SLOT_BYTES + SECTOR_BYTES];
        assert_eq!(block_on(read_at(0, &mut huge)), Err(BlockError::TooLarge));
        // The largest legal transfer is accepted, so the bound cannot drift
        // downward and refuse a request it should carry.
        let mut biggest = [0u8; SLOT_BYTES];
        block_on(read_at(0, &mut biggest)).expect("the largest legal read was refused");
        assert_eq!(u64::from_le_bytes(biggest[0..8].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(biggest[SECTOR_BYTES..SECTOR_BYTES + 8].try_into().unwrap()),
            1,
            "a multi-sector read did not advance the LBA across sectors"
        );
    }

    #[test_case]
    fn every_descriptor_is_returned_after_a_request_completes() {
        // A chain leaked per request drains the queue after 21 of them, and the
        // symptom is `QueueFull` in code that has nothing to do with the leak.
        ready();
        let free_before = { DEVICE.lock().as_ref().unwrap().queue.free_count() };
        let mut buf = [0u8; SECTOR_BYTES];
        for lba in 0..8u64 {
            block_on(read_at(lba, &mut buf)).expect("the read failed");
            assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), lba);
        }
        let free_after = { DEVICE.lock().as_ref().unwrap().queue.free_count() };
        assert_eq!(free_after, free_before, "requests leaked descriptors");
    }
}
