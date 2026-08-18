//! virtio-blk request layout.
//!
//! Three structures the device reads or writes at addresses the driver
//! publishes, so the layout *is* the contract. A field in the wrong place is a
//! read of the wrong sector, reported as success.

/// Bytes per virtio-blk sector.
///
/// Fixed by the specification at 512 regardless of the underlying device's
/// physical block size — a device with 4096-byte blocks still addresses in
/// 512-byte units here.
pub const SECTOR_BYTES: usize = 512;

/// Read from the device into memory.
pub const REQUEST_IN: u32 = 0;
/// Write from memory to the device.
pub const REQUEST_OUT: u32 = 1;

/// The header the device reads at the start of every request chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RequestHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

impl RequestHeader {
    pub const BYTES: usize = 16;

    /// A request to read `sector` into memory.
    pub const fn read(sector: u64) -> Self {
        Self { kind: REQUEST_IN, reserved: 0, sector }
    }

    /// A request to write memory out to `sector`.
    pub const fn write(sector: u64) -> Self {
        Self { kind: REQUEST_OUT, reserved: 0, sector }
    }

    /// The header as the device reads it.
    ///
    /// Serialised explicitly rather than transmuted. `#[repr(C)]` pins the
    /// field order but not the endianness, and the device reads little-endian
    /// on every architecture — so a transmute would be correct today and wrong
    /// the first time this kernel is built for anything big-endian, in a way
    /// nothing would report.
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[0..4].copy_from_slice(&self.kind.to_le_bytes());
        out[4..8].copy_from_slice(&self.reserved.to_le_bytes());
        out[8..16].copy_from_slice(&self.sector.to_le_bytes());
        out
    }
}

/// The status byte the device writes at the end of a request chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkStatus {
    Ok,
    IoError,
    Unsupported,
    /// A value the specification does not define.
    ///
    /// Carried through rather than folded into `IoError`, because a device
    /// reporting something this driver has never heard of is worth saying so
    /// about — and folding it into *success* would be a silent data-corruption
    /// bug, which is why the catch-all arm points away from `Ok`.
    Unknown(u8),
}

/// Decodes the status byte the device wrote.
pub const fn status_from_byte(b: u8) -> BlkStatus {
    match b {
        0 => BlkStatus::Ok,
        1 => BlkStatus::IoError,
        2 => BlkStatus::Unsupported,
        other => BlkStatus::Unknown(other),
    }
}


/// Whether the device transferred everything the request asked for.
///
/// The used ring's `len` is written by the *device*, so it is input rather than
/// bookkeeping. A device that completes a 4096-byte read with `VIRTIO_BLK_S_OK`
/// and `len = 0` has written nothing into the buffer -- and a driver that
/// copies the full request out anyway hands back whatever the previous request
/// through that buffer left there. Another block's bytes, returned as this
/// one's, which a buffer cache above then serves on every later hit.
///
/// A device reporting *more* than was asked for is not treated as a failure
/// here: the driver bounds the copy by what it asked for, so the excess is
/// unreachable, and refusing it would turn a device that pads its accounting
/// into a device that cannot be used at all.
pub fn transfer_satisfied(reported: u32, asked: usize) -> bool {
    reported as usize >= asked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_transfer_is_not_satisfied_and_a_full_one_is() {
        // The refusing direction is the one that matters. A device reporting
        // fewer bytes than were asked for has left the rest of the buffer
        // holding the previous request through it, and a driver that accepts
        // the completion returns those bytes as this request's data.
        assert!(!transfer_satisfied(0, 4096), "a device that transferred nothing was believed");
        assert!(!transfer_satisfied(4095, 4096), "a transfer one byte short was accepted");
        assert!(!transfer_satisfied(512, 4096), "a one-sector transfer satisfied an eight-sector read");
        assert!(transfer_satisfied(4096, 4096), "an exact transfer was refused");
        // More than asked for is not a failure: the driver bounds its copy by
        // what it asked for, so the excess is unreachable, and refusing it
        // would make a device that pads its accounting unusable.
        assert!(transfer_satisfied(8192, 4096), "a device that over-reported was refused");
        assert!(transfer_satisfied(0, 0), "an empty request was refused");
    }

    #[test]
    fn the_request_header_has_the_layout_the_device_reads() {
        // The device parses these bytes at a physical address the driver
        // publishes; a field in the wrong place is a read of the wrong sector,
        // reported as success.
        assert_eq!(core::mem::size_of::<RequestHeader>(), RequestHeader::BYTES);
        let bytes = RequestHeader::read(0x1122_3344_5566_7788).to_bytes();
        assert_eq!(&bytes[0..4], &REQUEST_IN.to_le_bytes(), "kind is not first, little-endian");
        assert_eq!(&bytes[4..8], &[0, 0, 0, 0], "the reserved word is not zero");
        assert_eq!(
            &bytes[8..16],
            &0x1122_3344_5566_7788u64.to_le_bytes(),
            "the sector is not at offset 8"
        );
    }

    #[test]
    fn a_read_and_a_write_differ_only_in_their_kind() {
        // The one field that decides whether the device reads memory or writes
        // it. Getting it backwards on a read destroys the sector it was meant
        // to fetch, and the request still completes successfully.
        let r = RequestHeader::read(9).to_bytes();
        let w = RequestHeader::write(9).to_bytes();
        assert_ne!(r, w, "a read and a write serialise identically");
        assert_eq!(&r[4..], &w[4..], "the kind is not the only difference");
        assert_eq!(&r[0..4], &REQUEST_IN.to_le_bytes());
        assert_eq!(&w[0..4], &REQUEST_OUT.to_le_bytes());
    }

    #[test]
    fn an_unknown_status_byte_is_reported_rather_than_treated_as_success() {
        // The status byte is device-supplied. Mapping anything non-zero to a
        // generic error would be fine; mapping an *unknown* value to `Ok` is a
        // silent data-corruption bug, so the negative direction is what is
        // asserted -- across every byte, not a sample.
        assert_eq!(status_from_byte(0), BlkStatus::Ok);
        assert_eq!(status_from_byte(1), BlkStatus::IoError);
        assert_eq!(status_from_byte(2), BlkStatus::Unsupported);
        assert_eq!(status_from_byte(0xff), BlkStatus::Unknown(0xff));
        for b in 1..=u8::MAX {
            assert_ne!(status_from_byte(b), BlkStatus::Ok, "status {b} decoded as success");
        }
    }
}
