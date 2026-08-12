#![cfg_attr(not(any(test, feature = "std")), no_std)]

//! Split virtqueues: the layout and the arithmetic, and nothing else.
//!
//! Deliberately knows nothing about PCI, MMIO, interrupts or physical
//! addresses. A queue here is three byte buffers and the index arithmetic that
//! walks them; the kernel copies those bytes into DMA memory and tells the
//! device where they went. That separation is what lets this be tested on the
//! host.
//!
//! It is worth the separation because of *how* this code fails. A descriptor
//! that names the wrong buffer does not crash: the device does exactly as it is
//! told, reports success, and the caller gets the wrong sector — the same shape
//! as handing the same memory to two callers, which this project has shipped
//! twice in the allocators. The failure is a wrong answer, so the test has to
//! be an assertion about the answer rather than the absence of a fault.
//!
//! The sharpest edge is that a virtqueue has **two** kinds of index and they
//! are not the same. The available and used rings each carry a free-running
//! `u16` that counts every entry ever published and wraps at 65536; the *slot*
//! that entry occupies is that counter reduced modulo the queue size.
//! Conflating them writes outside the ring for any queue smaller than 65536,
//! which is every queue.

extern crate alloc;

pub mod blk;
pub mod queue;

pub use queue::SplitQueue;

/// Descriptors per queue.
///
/// One queue of 64, matching the timer table's capacity for the same reason: it
/// is "every thread the kernel runs, with room". The queue refuses a request
/// when full rather than reusing a live descriptor, so this bounds latency
/// under load rather than correctness.
pub const QUEUE_SIZE: u16 = 64;

/// This descriptor chains to another, named by `next`.
pub const DESC_F_NEXT: u16 = 1;
/// The device writes this buffer; without it the device only reads.
///
/// Getting this backwards on the status byte is the mistake worth naming: the
/// device must *write* status, so a status descriptor marked read-only makes
/// the device reject the whole chain — which at least fails loudly. The
/// opposite, a read-only data buffer marked writable, does not.
pub const DESC_F_WRITE: u16 = 2;

/// One descriptor, in the layout the device reads.
///
/// `#[repr(C)]` and little-endian by construction: these bytes are parsed by
/// the device at an address the driver publishes, so the layout *is* the
/// contract. A field in the wrong place is a read of the wrong memory,
/// reported as success.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl Descriptor {
    pub const BYTES: usize = 16;

    /// The descriptor as the device reads it.
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[0..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..12].copy_from_slice(&self.len.to_le_bytes());
        out[12..14].copy_from_slice(&self.flags.to_le_bytes());
        out[14..16].copy_from_slice(&self.next.to_le_bytes());
        out
    }
}

/// Where each ring component starts within one allocation, and how much is
/// needed in total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    pub desc: usize,
    pub avail: usize,
    pub used: usize,
    pub bytes: usize,
}

/// Bytes of ring header before the entries: `flags` and `idx`.
///
/// Public because the kernel reads individual used-ring entries out of DMA
/// memory and needs the same offset the layout was built with; a second copy of
/// the number is one edit away from addressing the wrong entry.
pub const RING_HEADER: usize = 4;
/// Bytes of ring footer after the entries: the event suppression field.
const RING_FOOTER: usize = 2;

const fn align_up(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

/// The offsets of the three ring components for a queue of `size` descriptors.
///
/// The alignments are the spec's, not this code's preference: the descriptor
/// table must be 16-byte aligned, the available ring 2-byte, the used ring
/// 4-byte. The device computes nothing — it is told each address separately —
/// but it *reads* each structure with those alignment assumptions, so an
/// unaligned used ring is a torn read of the completion the driver is waiting
/// for.
pub const fn ring_layout(size: u16) -> RingLayout {
    let size = size as usize;
    let desc = 0;
    let avail = desc + Descriptor::BYTES * size;
    let avail_end = avail + RING_HEADER + 2 * size + RING_FOOTER;
    let used = align_up(avail_end, 4);
    let bytes = used + RING_HEADER + 8 * size + RING_FOOTER;
    RingLayout { desc, avail, used, bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_layout_matches_the_alignments_the_spec_requires() {
        // Every offset checked against the spec's rule rather than against a
        // remembered number. The device reads these structures at addresses the
        // driver publishes, so an offset that is wrong by any amount hands it a
        // ring made of the wrong bytes -- and it will do exactly as told.
        let l = ring_layout(QUEUE_SIZE);
        assert_eq!(l.desc, 0);
        assert_eq!(l.desc % 16, 0, "the descriptor table must be 16-byte aligned");
        assert_eq!(l.avail % 2, 0, "the available ring must be 2-byte aligned");
        assert_eq!(l.used % 4, 0, "the used ring must be 4-byte aligned");
        // And the parts must not overlap, which the alignment padding makes
        // easy to get wrong in the direction of "looks fine, shares bytes".
        assert!(
            l.avail >= l.desc + Descriptor::BYTES * QUEUE_SIZE as usize,
            "the available ring overlaps the descriptor table"
        );
        assert!(
            l.used >= l.avail + RING_HEADER + 2 * QUEUE_SIZE as usize + RING_FOOTER,
            "the used ring overlaps the available ring"
        );
        assert!(
            l.bytes >= l.used + RING_HEADER + 8 * QUEUE_SIZE as usize + RING_FOOTER,
            "the allocation is shorter than the used ring it must contain"
        );
    }

    #[test]
    fn the_layout_holds_for_every_legal_queue_size() {
        // A power-of-two size makes every alignment fall out for free, which is
        // exactly why testing only `QUEUE_SIZE` would prove nothing about the
        // padding. Odd sizes are where `align_up` earns its place.
        for size in [1u16, 2, 3, 7, 64, 255, 256] {
            let l = ring_layout(size);
            assert_eq!(l.used % 4, 0, "size {size}: the used ring is misaligned");
            assert!(
                l.used >= l.avail + RING_HEADER + 2 * size as usize + RING_FOOTER,
                "size {size}: the used ring overlaps the available ring"
            );
            assert!(
                l.bytes >= l.used + RING_HEADER + 8 * size as usize + RING_FOOTER,
                "size {size}: the allocation is short"
            );
        }
    }

    #[test]
    fn a_descriptor_serialises_in_the_order_the_device_reads_it() {
        // The layout is the contract. A field in the wrong place points the
        // device at the wrong memory, and it will read or write it without
        // complaint.
        let d = Descriptor {
            addr: 0x1122_3344_5566_7788,
            len: 0x99aa_bbcc,
            flags: DESC_F_NEXT | DESC_F_WRITE,
            next: 0xdeef,
        };
        let b = d.to_bytes();
        assert_eq!(&b[0..8], &0x1122_3344_5566_7788u64.to_le_bytes(), "addr is not first");
        assert_eq!(&b[8..12], &0x99aa_bbccu32.to_le_bytes(), "len is not at offset 8");
        assert_eq!(&b[12..14], &3u16.to_le_bytes(), "flags are not at offset 12");
        assert_eq!(&b[14..16], &0xdeefu16.to_le_bytes(), "next is not at offset 14");
        assert_eq!(core::mem::size_of::<Descriptor>(), Descriptor::BYTES);
    }
}
