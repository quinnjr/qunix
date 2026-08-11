//! The modern virtio PCI transport.
//!
//! Discovers a virtio device's four configuration structures from its PCI
//! capability list, maps them uncacheable, and drives the initialisation
//! handshake to `DRIVER_OK`.
//!
//! Modern (1.0+) only. The legacy interface puts its registers in an I/O BAR at
//! fixed offsets and takes a *page frame number* for the queue, which assumes
//! 4 KiB pages and cannot express a ring anywhere else. The modern interface
//! names each structure with an explicit BAR, offset and length, and takes full
//! 64-bit addresses for the three ring components separately — more to
//! discover, and much less to infer. Nothing here is derived from a layout
//! constant the device did not state.
//!
//! `disable-legacy=on` in the QEMU arguments means a fallback to the legacy
//! interface fails here rather than working under emulation and breaking on
//! hardware that does not implement it.

use qunix_hal_x86_64::pci::{self, Bdf, Capability};
use qunix_virtio::{QUEUE_SIZE, RingLayout};

/// Every virtio device answers with this vendor.
pub const VIRTIO_VENDOR: u16 = 0x1af4;
/// Modern virtio-blk. `0x1040 + 2`, where 2 is the block device type.
pub const VIRTIO_BLK_MODERN: u16 = 0x1042;
/// Transitional virtio-blk, which a device offering the legacy interface uses.
///
/// Recognised so that finding one can be *reported* rather than looking like an
/// absent device: with `disable-legacy=on` this should never appear, and if it
/// does the difference between "no disk attached" and "the disk is the wrong
/// kind" is the difference between a five-minute diagnosis and an hour's.
pub const VIRTIO_BLK_TRANSITIONAL: u16 = 0x1001;

/// The PCI capability id virtio uses for its vendor-specific structures.
const CAP_ID_VENDOR: u8 = 0x09;

/// Which virtio structure a vendor capability describes.
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;

/// Spins allowed for a device to complete a reset.
///
/// A count of iterations, not a duration -- there is no clock at this point in
/// bring-up. Generous enough that no real device reaches it, and finite because
/// an unbounded wait on a device register is a hang with no message.
const RESET_SPIN_LIMIT: u32 = 1_000_000;

/// The largest device window this driver will map.
///
/// Every structure it needs is far smaller -- the common configuration is 56
/// bytes, the ISR one -- and `map_window` allocates a page table per page, so
/// an unbounded length is a boot-time denial of service from a device that
/// merely lies about a `u32`.
const MAX_WINDOW_BYTES: u32 = 64 * 1024;

/// A virtio PCI capability is 16 bytes, and `cap_len` must say so.
const VIRTIO_CAP_BYTES: usize = 16;

/// Device status bits, in the order the handshake sets them.
pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_NEEDS_RESET: u8 = 64;
pub const STATUS_FAILED: u8 = 128;

/// The feature that says this device speaks the modern protocol.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Offsets within the common configuration structure.
mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0;
    pub const DEVICE_FEATURE: u64 = 4;
    pub const DRIVER_FEATURE_SELECT: u64 = 8;
    pub const DRIVER_FEATURE: u64 = 12;
    pub const NUM_QUEUES: u64 = 18;
    pub const DEVICE_STATUS: u64 = 20;
    pub const QUEUE_SELECT: u64 = 22;
    pub const QUEUE_SIZE: u64 = 24;
    pub const QUEUE_MSIX_VECTOR: u64 = 26;
    pub const QUEUE_ENABLE: u64 = 28;
    pub const QUEUE_NOTIFY_OFF: u64 = 30;
    pub const QUEUE_DESC: u64 = 32;
    pub const QUEUE_DRIVER: u64 = 40;
    pub const QUEUE_DEVICE: u64 = 48;
}

/// Why a device could not be brought up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// No device with the wanted vendor and device id on bus 0.
    NotFound,
    /// A transitional device was found where a modern one was required.
    LegacyOnly,
    /// The capability list could not be walked.
    Capabilities(pci::CapError),
    /// One of the four virtio structures is missing from the capability list.
    MissingStructure(u8),
    /// A capability names a BAR that does not exist, is I/O rather than memory,
    /// or is too small for the window the capability claims.
    ///
    /// The size half of that sentence was a lie until the BAR probe was added:
    /// `read_bar` returned a base and nothing measured the extent, so the
    /// comment described a check that had never been written. It is the check
    /// now -- `read_bar` probes the size and `probe` refuses a window that runs
    /// past it.
    BadBar(u8),
    /// The device does not offer `VIRTIO_F_VERSION_1`.
    NoVersion1,
    /// `queue_notify_off * notify_off_multiplier` lands outside the notify
    /// window. Both factors are device-supplied, so this is the device naming
    /// an address rather than an offset.
    BadNotifyOffset(u16),
    /// A capability's window runs past the BAR that holds it, or the BAR is
    /// larger than this driver will map.
    WindowTooLarge(u32),
    /// A device window could not be mapped.
    MapFailed(u64),
    /// The device's queue is smaller than the driver's ring.
    QueueTooSmall(u16),
    /// The device cleared `FEATURES_OK`, refusing the feature set.
    FeaturesRejected,
    /// The device set `FAILED` during the handshake.
    DeviceFailed,
}

/// One mapped virtio structure.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// Virtual address, through the HHDM.
    base: u64,
    len: u32,
}

impl Window {
    /// Refuses an access that would leave the window.
    ///
    /// A real assertion, not `debug_assert!`. These bounds were debug-only, and
    /// the release profile does not re-enable debug assertions -- while CI boots
    /// release -- so in the build that actually ships there was *no* bound at
    /// all. That mattered because `notify`'s offset is computed from two
    /// device-supplied values: an unbounded offset from a HHDM base is an
    /// arbitrary physical address, and the write is two zero bytes into it.
    ///
    /// `checked_add` because the sum itself wraps in release, where overflow
    /// checks are off -- a wrapped sum compares small and passes any bound.
    ///
    /// The messages are static strings, which is not a style preference: a
    /// formatted message here (`"...at {offset:#x}..."`) triple-faults the
    /// machine. These are generic over `T` and called from every register
    /// access, so a formatting panic is instantiated at each one, and the
    /// resulting panic path is more than this call site can carry. A
    /// triple-fault reports nothing at all, so a message that cannot be printed
    /// is worse than no message.
    fn in_bounds(&self, offset: u64, size: usize) -> bool {
        offset.checked_add(size as u64).is_some_and(|end| end <= self.len as u64)
    }

    /// # Safety
    /// `offset` must be correctly aligned for `T`. The bound is checked.
    unsafe fn read<T>(&self, offset: u64) -> T {
        assert!(self.in_bounds(offset, core::mem::size_of::<T>()), "device window read out of bounds");
        unsafe { core::ptr::read_volatile((self.base + offset) as *const T) }
    }

    /// # Safety
    /// As [`Self::read`], and the write reaches a device register.
    unsafe fn write<T>(&self, offset: u64, value: T) {
        assert!(self.in_bounds(offset, core::mem::size_of::<T>()), "device window write out of bounds");
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut T, value) }
    }
}

/// A virtio device, mapped and ready to be configured.
pub struct Transport {
    bdf: Bdf,
    common: Window,
    notify: Window,
    notify_multiplier: u32,
    #[allow(dead_code)]
    isr: Window,
    #[allow(dead_code)]
    device: Window,
    queue_notify_off: u16,
}

impl Transport {
    pub fn status(&self) -> u8 {
        // SAFETY: the common window was validated to cover this offset when it
        // was mapped.
        unsafe { self.common.read::<u8>(common::DEVICE_STATUS) }
    }

    fn set_status(&self, bits: u8) {
        let now = self.status() | bits;
        // SAFETY: as `status`.
        unsafe { self.common.write::<u8>(common::DEVICE_STATUS, now) };
    }

    pub fn bdf(&self) -> Bdf {
        self.bdf
    }

    /// Reads the device's 64-bit feature set.
    fn device_features(&self) -> u64 {
        let mut features = 0u64;
        for half in 0..2u32 {
            // SAFETY: both offsets are inside the common window.
            unsafe {
                self.common.write::<u32>(common::DEVICE_FEATURE_SELECT, half);
                features |= (self.common.read::<u32>(common::DEVICE_FEATURE) as u64) << (half * 32);
            }
        }
        features
    }

    /// Negotiates `wanted`, and confirms the device accepted it.
    ///
    /// Two refusals, and both matter. A device that does not offer
    /// `VIRTIO_F_VERSION_1` is a legacy device, and driving it with the modern
    /// ring layout reads the wrong bytes for every request. And the device
    /// signals rejection by *clearing* `FEATURES_OK` rather than by any error,
    /// so a driver that writes the bit and does not read it back proceeds
    /// against a device that has refused it.
    pub fn negotiate(&mut self, wanted: u64) -> Result<u64, ProbeError> {
        let offered = self.device_features();
        let agreed = offered & wanted;
        if agreed & VIRTIO_F_VERSION_1 == 0 {
            return Err(ProbeError::NoVersion1);
        }
        for half in 0..2u32 {
            // SAFETY: both offsets are inside the common window.
            unsafe {
                self.common.write::<u32>(common::DRIVER_FEATURE_SELECT, half);
                self.common
                    .write::<u32>(common::DRIVER_FEATURE, (agreed >> (half * 32)) as u32);
            }
        }
        self.set_status(STATUS_FEATURES_OK);
        // Read back, not assumed.
        //
        // Not falsifiable against QEMU, and said so rather than left to look
        // covered: this device accepts every feature set the driver offers, so
        // nothing here can make it clear the bit. Deleting this check leaves
        // the suite green. It stays because the *device* decides, the
        // specification says it signals refusal this way and no other, and a
        // driver that proceeds against a device that has refused its features
        // is talking a protocol the device is not.
        if self.status() & STATUS_FEATURES_OK == 0 {
            return Err(ProbeError::FeaturesRejected);
        }
        if self.status() & STATUS_FAILED != 0 {
            return Err(ProbeError::DeviceFailed);
        }
        Ok(agreed)
    }

    /// Points queue 0 at a ring and enables it.
    ///
    /// The three addresses are physical, and each is stated separately — the
    /// device derives none of them. That is the modern interface's advantage
    /// over the legacy one, which took a page frame number and computed the
    /// rest from a layout the driver had to match exactly.
    pub fn configure_queue(&mut self, layout: RingLayout, ring_phys: u64) -> Result<(), ProbeError> {
        self.configure_queue_sized(QUEUE_SIZE, layout, ring_phys)
    }

    /// The largest ring the device will accept.
    pub fn device_queue_size(&self) -> u16 {
        // SAFETY: both offsets are inside the common window.
        unsafe {
            self.common.write::<u16>(common::QUEUE_SELECT, 0);
            self.common.read::<u16>(common::QUEUE_SIZE)
        }
    }

    /// Points queue 0 at a ring of `size` descriptors.
    ///
    /// Separated from [`configure_queue`](Self::configure_queue) so the size
    /// refusal is reachable from a test: with one entry point the only way to
    /// trip it would be a device whose queue is smaller than `QUEUE_SIZE`, and
    /// there is no such device to hand.
    pub fn configure_queue_sized(
        &mut self,
        size: u16,
        layout: RingLayout,
        ring_phys: u64,
    ) -> Result<(), ProbeError> {
        // SAFETY: every offset below is inside the common window, validated
        // when it was mapped.
        unsafe {
            self.common.write::<u16>(common::QUEUE_SELECT, 0);
            let device_size = self.common.read::<u16>(common::QUEUE_SIZE);
            // The device states the largest ring it will accept. Writing a
            // larger one back would have it read descriptors past the end of
            // its own table, at addresses derived from a ring that is not
            // there.
            if device_size < size {
                return Err(ProbeError::QueueTooSmall(device_size));
            }
            self.common.write::<u16>(common::QUEUE_SIZE, size);
            self.common.write::<u64>(common::QUEUE_DESC, ring_phys + layout.desc as u64);
            self.common.write::<u64>(common::QUEUE_DRIVER, ring_phys + layout.avail as u64);
            self.common.write::<u64>(common::QUEUE_DEVICE, ring_phys + layout.used as u64);
            let notify_off = self.common.read::<u16>(common::QUEUE_NOTIFY_OFF);
            // Validated here rather than trusted at every `notify`. The offset
            // is `queue_notify_off * notify_off_multiplier`, and *both* come
            // from the device -- so without this the device chooses an offset
            // from a HHDM base, which is an arbitrary physical address. The
            // window's own bound catches it too, but as a panic on the I/O path
            // rather than a refusal at bring-up, and refusing a malformed device
            // is the outcome a caller can act on.
            let offset = (notify_off as u64).checked_mul(self.notify_multiplier as u64);
            let fits = offset
                .and_then(|o| o.checked_add(2))
                .is_some_and(|end| end <= self.notify.len as u64);
            if !fits {
                return Err(ProbeError::BadNotifyOffset(notify_off));
            }
            self.queue_notify_off = notify_off;
            self.common.write::<u16>(common::QUEUE_ENABLE, 1);
        }
        Ok(())
    }

    /// Binds queue 0's completions to an MSI-X table entry.
    pub fn set_queue_msix_vector(&mut self, entry: u16) {
        // SAFETY: both offsets are inside the common window.
        unsafe {
            self.common.write::<u16>(common::QUEUE_SELECT, 0);
            self.common.write::<u16>(common::QUEUE_MSIX_VECTOR, entry);
        }
    }

    /// The physical base of one of this device's BARs.
    ///
    /// # Safety
    /// Port I/O from ring 0.
    pub unsafe fn bar_base(&self, index: u8) -> Option<(u64, u64)> {
        // SAFETY: the caller's obligation, forwarded.
        unsafe { read_bar(self.bdf, index) }
    }

    /// Tells the device that queue 0 has new work.
    pub fn notify(&self) {
        let offset = self.queue_notify_off as u64 * self.notify_multiplier as u64;
        // SAFETY: `offset` was derived from the device's own notify offset and
        // multiplier, and the window's length was validated to cover it when it
        // was mapped.
        unsafe { self.notify.write::<u16>(offset, 0) };
    }

    /// Declares the driver live. Nothing may be submitted before this.
    pub fn finish(&mut self) -> Result<(), ProbeError> {
        self.set_status(STATUS_DRIVER_OK);
        if self.status() & STATUS_FAILED != 0 {
            return Err(ProbeError::DeviceFailed);
        }
        Ok(())
    }

    pub fn num_queues(&self) -> u16 {
        // SAFETY: the offset is inside the common window.
        unsafe { self.common.read::<u16>(common::NUM_QUEUES) }
    }
}

/// Reads a BAR, returning its physical base and whether it is 64-bit.
///
/// # Safety
/// Port I/O.
/// Where BAR 0 lives in the standard PCI header.
const PCI_BAR0: u8 = 0x10;
/// Bits 3:0 of a memory BAR are type flags, not address.
const PCI_BAR_ADDR_MASK: u32 = 0xffff_fff0;

/// Decodes a BAR pair into a physical base.
///
/// Separated from the port I/O for the reason `pci::walk_capabilities` gives
/// for taking a snapshot: the branches here -- refusing an I/O BAR, assembling
/// a 64-bit base from two slots -- are the only interesting part, and inline
/// port I/O would put every one of them out of reach of a host test. The first
/// run of this driver died on exactly one of them.
pub const fn decode_bar(low: u32, high: u32, index: u8) -> Option<u64> {
    // Bit 0 set means an I/O BAR. Virtio's modern structures live in memory
    // BARs; treating an I/O BAR's value as a physical address would map a port
    // number as memory.
    if low & 1 != 0 {
        return None;
    }
    let kind = (low >> 1) & 0b11;
    let base = (low & PCI_BAR_ADDR_MASK) as u64;
    match kind {
        0b00 => Some(base),
        // A 64-bit BAR's upper half lives in the next slot, which must
        // therefore exist.
        0b10 if index < 5 => Some(base | ((high as u64) << 32)),
        _ => None,
    }
}

/// Decodes a BAR's size from its probe read-back.
///
/// The standard dance: write all-ones, read back, and the lowest set bit is the
/// size. Without it nothing ties a capability's `offset + length` to anything
/// real -- and `ProbeError::BadBar`'s own documentation claimed that check
/// existed when it did not, which is the most expensive kind of comment.
pub const fn decode_bar_size(probe_low: u32) -> u64 {
    let masked = probe_low & PCI_BAR_ADDR_MASK;
    if masked == 0 {
        return 0;
    }
    (!masked as u64 + 1) & 0xffff_ffff
}

/// Reads a BAR's base and size.
///
/// # Safety
/// Port I/O, and the BAR is written and restored: the caller must ensure the
/// device is not decoding through it concurrently. `probe` does this before
/// enabling memory decoding for any window it maps.
unsafe fn read_bar(bdf: Bdf, index: u8) -> Option<(u64, u64)> {
    if index > 5 {
        return None;
    }
    let offset = PCI_BAR0 + index * 4;
    // SAFETY: the caller's obligation, forwarded.
    let low = unsafe { pci::config_read32(bdf, offset) };
    let high = if index < 5 {
        // SAFETY: as above.
        unsafe { pci::config_read32(bdf, offset + 4) }
    } else {
        0
    };
    let base = decode_bar(low, high, index)?;
    // Size probe: write all-ones, read back, restore. Restoring matters --
    // leaving all-ones in a BAR relocates the device's window on top of
    // whatever else decodes there.
    // SAFETY: as above.
    let size = unsafe {
        pci::config_write32(bdf, offset, u32::MAX);
        let probed = pci::config_read32(bdf, offset);
        pci::config_write32(bdf, offset, low);
        decode_bar_size(probed)
    };
    if size == 0 {
        return None;
    }
    Some((base, size))
}

/// Maps a device window uncacheable through the HHDM.
///
/// Uncacheable is not optional: these are device registers, and a write-back
/// mapping lets a status write sit in a cache line while the driver waits for
/// the device to react to it.
pub fn map_device_window(phys: u64, len: u32) -> Option<u64> {
    map_window(phys, len).map(|w| w.base)
}

fn map_window(phys: u64, len: u32) -> Option<Window> {
    // Refused here rather than by every caller. `phys + len - 1` underflows for
    // a zero length, and the loop below then maps every page in the address
    // space until the frame allocator is empty -- a hang with no message, which
    // is the failure this kernel is worst at reporting. The guard used to live
    // in `probe` only, which left it out of reach of the arithmetic it protects.
    if len == 0 {
        return None;
    }
    use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};
    let hhdm = crate::boot::hhdm_offset();
    // SAFETY: reads the live CR3 and wraps it; nothing is dereferenced.
    let mut space = unsafe { AddressSpace::active(hhdm) };
    let first = phys & !0xfff;
    let last = (phys + len as u64 - 1) & !0xfff;
    let mut page = first;
    while page <= last {
        let va = hhdm + page;
        if space.translate(va).is_some() {
            // Limine's HHDM may already cover this range, and `translate`
            // reports presence rather than cacheability -- a write-back mapping
            // of device registers is unusable. Replaced rather than trusted,
            // exactly as `map_lapic` does for the LAPIC page.
            // SAFETY: nothing holds a reference derived from this address; the
            // device has not been touched yet.
            unsafe {
                space.unmap(va).expect(
                    "a virtio BAR is covered by a huge HHDM mapping; cannot make it \
                     uncacheable without splitting the parent entry",
                );
            }
        }
        // SAFETY: `page` is a device physical address from a BAR, not RAM, so
        // no allocator owns it and no alias is created.
        let mapped = unsafe {
            space.map(
                va,
                page,
                PageFlags::PRESENT
                    | PageFlags::WRITABLE
                    | PageFlags::NO_CACHE
                    | PageFlags::NO_EXECUTE,
                &mut || crate::frames::alloc(0),
            )
        };
        // Reported, not asserted. The failure this actually has is frame
        // exhaustion -- a resource condition, on a path whose every other
        // failure is a `ProbeError` the caller can act on -- so panicking here
        // would turn "no block device" into "the kernel halts" for a machine
        // that is merely short of memory at boot.
        if mapped.is_err() {
            return None;
        }
        page += 4096;
    }
    Some(Window { base: hhdm + phys, len })
}

/// One virtio capability, as read from configuration space.
struct VirtioCap {
    cfg_type: u8,
    bar: u8,
    offset: u32,
    length: u32,
    /// Only meaningful for the notify capability.
    notify_multiplier: u32,
}

fn parse_virtio_cap(cfg: &[u8; 256], cap: Capability) -> Option<VirtioCap> {
    let body = pci::capability_body(cfg, cap, VIRTIO_CAP_BYTES).ok()?;
    // `cap_len` must cover the structure the driver is about to read. A device
    // reporting a shorter one is describing a different layout, and reading 16
    // bytes anyway would take the tail from whatever follows.
    if body[2] < VIRTIO_CAP_BYTES as u8 {
        return None;
    }
    let cfg_type = body[3];
    let bar = body[4];
    let offset = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
    let length = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
    // The notify capability carries a further u32 the others do not, so it is
    // read only when the whole 20 bytes are present.
    let notify_multiplier = pci::capability_body(cfg, cap, VIRTIO_CAP_BYTES + 4)
        .ok()
        .map(|b| u32::from_le_bytes([b[16], b[17], b[18], b[19]]))
        .unwrap_or(0);
    Some(VirtioCap { cfg_type, bar, offset, length, notify_multiplier })
}

/// Finds, maps and resets a virtio device, leaving it ready to negotiate.
///
/// # Safety
/// Port I/O and MMIO mapping. Must be called once, from the bootstrap
/// processor, before anything submits work to the device.
pub unsafe fn probe(device_id: u16) -> Result<Transport, ProbeError> {
    // SAFETY: the caller's obligation, forwarded.
    let bdf = unsafe { pci::find_device(VIRTIO_VENDOR, device_id) }.ok_or_else(|| {
        // A transitional device answers to a different id. Distinguishing the
        // two turns "no disk" into "the disk is the wrong kind", which is the
        // difference between a five-minute diagnosis and an hour of looking for
        // a device that is right there.
        // SAFETY: as above.
        if unsafe { pci::find_device(VIRTIO_VENDOR, VIRTIO_BLK_TRANSITIONAL) }.is_some() {
            ProbeError::LegacyOnly
        } else {
            ProbeError::NotFound
        }
    })?;

    // Memory space and *bus mastering*. The second is what lets the device
    // issue DMA at all: without it every ring address the driver publishes is
    // ignored, the device never reads a descriptor, and the symptom is a
    // request that is accepted and never completes.
    //
    // Not falsifiable under QEMU, and this is a correction to what an earlier
    // commit here predicted. The guess was that the first real `read_at` would
    // make it fail; it does not. QEMU does not enforce the bus-master bit, so
    // the device happily DMAs without it and every block test stays green with
    // the bit cleared.
    //
    // It stays because real hardware does enforce it, and the symptom there is
    // the worst kind: every request accepted, no descriptor ever fetched, and
    // a thread parked forever on a completion that cannot arrive. A guard that
    // only matters off the emulator is exactly the kind this project has to
    // write down rather than test.
    // SAFETY: the caller's obligation, forwarded.
    unsafe {
        let command = pci::config_read32(bdf, pci::COMMAND_REGISTER);
        pci::config_write32(
            bdf,
            pci::COMMAND_REGISTER,
            command | pci::COMMAND_MEMORY_SPACE | pci::COMMAND_BUS_MASTER,
        );
    }

    // SAFETY: as above.
    let cfg = unsafe { pci::config_snapshot(bdf) };
    let caps = pci::walk_capabilities(&cfg).map_err(ProbeError::Capabilities)?;

    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut isr = None;
    let mut device = None;

    for cap in caps.iter().filter(|c| c.id == CAP_ID_VENDOR) {
        let Some(v) = parse_virtio_cap(&cfg, *cap) else { continue };
        // Only the four structures this driver maps are fatal to get wrong.
        //
        // A virtio device also advertises `VIRTIO_PCI_CAP_PCI_CFG` (type 5), an
        // alternative access path this driver does not use, and it may name a
        // BAR that is I/O rather than memory -- which a modern-only device
        // still carries for the transitional layout it is not offering.
        // Failing on it would refuse a perfectly good device because of a
        // capability nothing here reads. That is what the first run did: an
        // I/O BAR 0 turned into `BadBar(0)` for a device whose modern
        // structures were all present in BAR 4.
        if !matches!(v.cfg_type, CFG_COMMON | CFG_NOTIFY | CFG_ISR | CFG_DEVICE) {
            continue;
        }
        // SAFETY: port I/O, as above.
        let Some((bar_base, bar_size)) = (unsafe { read_bar(bdf, v.bar) }) else {
            return Err(ProbeError::BadBar(v.bar));
        };
        // A capability describing a window that starts past its own BAR, or
        // whose length wraps, is malformed. Mapping it anyway would map
        // whatever physical memory the arithmetic landed on -- which, with no
        // IOMMU between the device and RAM, is a page the kernel may own.
        let Some(end) = v.offset.checked_add(v.length) else {
            return Err(ProbeError::BadBar(v.bar));
        };
        if v.length == 0 {
            return Err(ProbeError::BadBar(v.bar));
        }
        // The window must lie inside the BAR that holds it. Without this the
        // device names any physical address it likes: `map_window` would remap
        // live kernel RAM as uncacheable and the driver would then write ring
        // addresses and status bytes into it. This is the check `BadBar`'s
        // documentation claimed to be, and was not.
        if end as u64 > bar_size {
            return Err(ProbeError::BadBar(v.bar));
        }
        // And a window larger than anything this driver needs is refused rather
        // than mapped: `map_window` allocates a page table per 4 KiB, so a
        // device declaring a 4 GiB structure exhausts the frame allocator at
        // boot and panics ring 0.
        if v.length > MAX_WINDOW_BYTES {
            return Err(ProbeError::WindowTooLarge(v.length));
        }
        let window = map_window(bar_base + v.offset as u64, v.length)
            .ok_or(ProbeError::MapFailed(bar_base + v.offset as u64))?;
        match v.cfg_type {
            CFG_COMMON => common = Some(window),
            CFG_NOTIFY => {
                notify = Some(window);
                notify_multiplier = v.notify_multiplier;
            }
            CFG_ISR => isr = Some(window),
            CFG_DEVICE => device = Some(window),
            _ => {}
        }
    }

    let transport = Transport {
        bdf,
        common: common.ok_or(ProbeError::MissingStructure(CFG_COMMON))?,
        notify: notify.ok_or(ProbeError::MissingStructure(CFG_NOTIFY))?,
        notify_multiplier,
        isr: isr.ok_or(ProbeError::MissingStructure(CFG_ISR))?,
        device: device.ok_or(ProbeError::MissingStructure(CFG_DEVICE))?,
        queue_notify_off: 0,
    };

    // Reset first. The device may be mid-configuration from a previous boot --
    // OVMF enumerates PCI and this kernel is not the first thing to touch the
    // bus -- and the handshake below is only defined from a reset device.
    // SAFETY: the common window covers this offset.
    unsafe { transport.common.write::<u8>(common::DEVICE_STATUS, 0) };
    // The spec requires reading the status back until it is zero: reset is not
    // guaranteed to complete synchronously.
    let mut spins = 0u32;
    while transport.status() != 0 {
        spins += 1;
        // Bounded, because an unbounded wait on a device register is a hang
        // with no message -- the failure mode this kernel is worst at
        // reporting.
        assert!(spins < RESET_SPIN_LIMIT, "virtio device did not reset");
        core::hint::spin_loop();
    }
    transport.set_status(STATUS_ACKNOWLEDGE);
    transport.set_status(STATUS_DRIVER);
    Ok(transport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn the_device_is_found_and_reaches_the_driver_state() {
        // `probe` leaves the device reset and acknowledged, which is the state
        // feature negotiation is defined from. A device that skipped the reset
        // would accept the handshake and keep configuration from whatever
        // touched it before this kernel did.
        let t = unsafe { probe(VIRTIO_BLK_MODERN) }.expect("no modern virtio-blk device");
        assert_eq!(t.status() & STATUS_ACKNOWLEDGE, STATUS_ACKNOWLEDGE);
        assert_eq!(t.status() & STATUS_DRIVER, STATUS_DRIVER);
        assert_eq!(t.status() & STATUS_FAILED, 0, "the device reported failure");
        // Not yet live: nothing may be submitted before `finish`.
        assert_eq!(t.status() & STATUS_DRIVER_OK, 0, "the device went live before it was told to");
        assert!(t.num_queues() >= 1, "a block device with no queues");
    }

    #[test_case]
    fn a_device_offering_nothing_is_refused_rather_than_driven_as_legacy() {
        // Negotiation asserted by *offering* nothing. A driver that does not
        // require VIRTIO_F_VERSION_1 is speaking the modern ring layout to a
        // device that may be using the legacy one -- the structures sit at
        // different offsets, so every request reads the wrong bytes and the
        // device reports success.
        let mut t = unsafe { probe(VIRTIO_BLK_MODERN) }.expect("no device");
        assert_eq!(t.negotiate(0), Err(ProbeError::NoVersion1));
        // And the real thing succeeds, so a `negotiate` that refused everything
        // would not pass the assertion above by accident.
        let agreed = t.negotiate(VIRTIO_F_VERSION_1).expect("the device refused VERSION_1");
        assert_eq!(agreed & VIRTIO_F_VERSION_1, VIRTIO_F_VERSION_1);
        assert_eq!(
            t.status() & STATUS_FEATURES_OK,
            STATUS_FEATURES_OK,
            "the device rejected the feature set and the driver did not notice"
        );
    }

    #[test_case]
    fn the_device_accepts_a_ring_and_goes_live() {
        // The handshake's last two steps, against a ring in real DMA memory.
        // Configuring a queue is where a device rejects the driver most
        // readily -- an address it cannot reach, a size larger than its own
        // table -- and it says so by setting FAILED rather than by any return
        // value, so the status is read back rather than assumed.
        let mut t = unsafe { probe(VIRTIO_BLK_MODERN) }.expect("no device");
        t.negotiate(VIRTIO_F_VERSION_1).expect("VERSION_1 refused");

        let layout = qunix_virtio::ring_layout(QUEUE_SIZE);
        assert!(layout.bytes <= 4096, "the ring no longer fits one frame");
        let ring = crate::frames::alloc(0).expect("no frame for the virtqueue");
        // Zeroed through the HHDM before the device is told about it: the
        // device reads the available ring's index immediately, and a frame
        // carrying whatever the last owner left would have it fetch
        // descriptors that were never written.
        // SAFETY: the frame was just allocated, so nothing else refers to it,
        // and the HHDM maps all of RAM.
        unsafe {
            core::ptr::write_bytes((crate::boot::hhdm_offset() + ring) as *mut u8, 0, 4096);
        }

        t.configure_queue(layout, ring).expect("the device refused the ring");
        t.finish().expect("the device failed while going live");
        assert_eq!(t.status() & STATUS_DRIVER_OK, STATUS_DRIVER_OK, "the device is not live");
        assert_eq!(t.status() & STATUS_FAILED, 0, "the device reported failure");
        assert_eq!(t.status() & STATUS_NEEDS_RESET, 0, "the device asked to be reset");
        // The device's own address, so a `bdf` that returned a fixed value
        // would not go unnoticed once a second device exists.
        assert_eq!(t.bdf(), unsafe { pci::find_device(VIRTIO_VENDOR, VIRTIO_BLK_MODERN) }.unwrap());

        // And the notify path is exercised, on an empty queue the device is
        // free to ignore. What is being asserted is the *address*: the offset
        // is `queue_notify_off * notify_off_multiplier`, both read from the
        // device, and getting either wrong writes outside the mapped window --
        // a page fault in ring 0, which panics the kernel rather than failing a
        // test. Surviving the call is the assertion.
        t.notify();
        assert_eq!(t.status() & STATUS_FAILED, 0, "the device failed on a notification");
    }

    #[test_case]
    fn a_ring_larger_than_the_device_will_accept_is_refused() {
        // The device states the largest ring it will take. Writing a larger one
        // back has it read descriptors past the end of its own table -- and it
        // would, because nothing between the driver and the DMA engine checks.
        // Asserted through the real path rather than by inspection: the guard
        // is one comparison, and the direction that matters is refusal.
        let mut t = unsafe { probe(VIRTIO_BLK_MODERN) }.expect("no device");
        t.negotiate(VIRTIO_F_VERSION_1).expect("VERSION_1 refused");
        let device_size = t.device_queue_size();
        assert!(device_size >= QUEUE_SIZE, "the device queue is smaller than the driver's ring");
        assert!(
            matches!(
                t.configure_queue_sized(device_size + 1, qunix_virtio::ring_layout(QUEUE_SIZE), 0),
                Err(ProbeError::QueueTooSmall(_))
            ),
            "the device accepted a ring larger than its own table"
        );
    }

    #[test_case]
    fn an_io_bar_is_refused_rather_than_mapped_as_memory() {
        // The regression for this driver's first run, which died with
        // `BadBar(0)`. A virtio device carries an I/O BAR for the transitional
        // layout it is not offering, and treating that value as a physical
        // address maps a port number as memory.
        assert_eq!(decode_bar(0x0000_c041, 0, 0), None, "an I/O BAR was accepted");
        // And a memory BAR at the same slot is not refused, so the check cannot
        // drift into refusing everything.
        assert_eq!(decode_bar(0xfebd_0000, 0, 0), Some(0xfebd_0000));
    }

    #[test_case]
    fn a_64_bit_bar_takes_its_upper_half_from_the_next_slot() {
        // Type bits 0b10 mean the address continues in the following slot.
        // Ignoring the upper half truncates the base to 32 bits, which on a
        // machine that places BARs high maps the wrong physical page entirely.
        assert_eq!(decode_bar(0xfe00_0004, 0x1, 4), Some(0x1_fe00_0000));
        // There is no slot after BAR 5, so a 64-bit BAR declared there is
        // malformed rather than half-read.
        assert_eq!(decode_bar(0xfe00_0004, 0x1, 5), None, "a 64-bit BAR in slot 5 was accepted");
        // The type bits must not leak into the address.
        assert_eq!(decode_bar(0xfebd_0004, 0, 4), Some(0xfebd_0000));
    }

    #[test_case]
    fn a_bar_size_is_the_lowest_set_bit_of_the_probe() {
        // The standard probe: all-ones in, and the read-back's lowest set
        // address bit is the size. Getting this wrong is what let a capability
        // claim a window past the end of its BAR.
        assert_eq!(decode_bar_size(0xffff_f000), 4096);
        assert_eq!(decode_bar_size(0xffff_c000), 16 * 1024);
        // An unimplemented BAR reads back as zero and has no size, which must
        // not be mistaken for "any window fits".
        assert_eq!(decode_bar_size(0), 0, "an unimplemented BAR was given a size");
    }

    #[test_case]
    fn a_capability_shorter_than_the_structure_is_refused() {
        // `cap_len` is device-supplied. A capability claiming fewer than 16
        // bytes describes a different layout, and reading 16 anyway takes the
        // tail from whatever follows it in configuration space.
        let mut cfg = [0u8; 256];
        cfg[0x40] = CAP_ID_VENDOR;
        cfg[0x42] = 15; // cap_len
        cfg[0x43] = CFG_COMMON;
        let cap = pci::Capability { id: CAP_ID_VENDOR, offset: 0x40 };
        assert!(parse_virtio_cap(&cfg, cap).is_none(), "a short capability was parsed");
        cfg[0x42] = 16;
        assert!(parse_virtio_cap(&cfg, cap).is_some(), "a well-formed capability was refused");
    }

    #[test_case]
    fn a_notify_capability_too_short_for_its_multiplier_reads_zero_rather_than_garbage() {
        // The notify capability carries a `u32` the other three do not. A
        // capability placed where those four bytes fall outside the window must
        // yield zero rather than whatever the snapshot buffer held -- the
        // multiplier is one of the two factors in the notify offset, so a torn
        // read there is an address the device did not choose and the driver did
        // not derive.
        let mut cfg = [0u8; 256];
        cfg[0xf0] = CAP_ID_VENDOR;
        cfg[0xf2] = 16;
        cfg[0xf3] = CFG_NOTIFY;
        let cap = pci::Capability { id: CAP_ID_VENDOR, offset: 0xf0 };
        let parsed = parse_virtio_cap(&cfg, cap).expect("the capability was refused");
        assert_eq!(parsed.notify_multiplier, 0, "a multiplier was read past the window");
    }

    #[test_case]
    fn an_absent_device_id_is_reported_as_not_found() {
        // The negative direction of `find_device`, and the one that would
        // otherwise be indistinguishable from a bus scan that stopped early:
        // an id no device answers to must be `NotFound`, not a `Transport`
        // pointing at whatever was in slot 0.
        assert!(matches!(unsafe { probe(0xbeef) }, Err(ProbeError::NotFound)));
    }
}
