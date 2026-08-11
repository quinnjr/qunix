# M2 T4 — virtio-blk Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `block::read_at(lba, buf).await` returns real bytes from a real disk,
completed by a device interrupt.

**Architecture:** PCI is enumerated through the legacy configuration ports; the
virtio-blk device is driven in modern (1.0) mode through its PCI capabilities;
one split virtqueue carries requests; MSI-X delivers completions straight to a
LAPIC vector, whose handler unparks the thread waiting in T3's `block_on`. The
ring arithmetic — where the bugs live — is a host-tested crate that never
touches hardware.

**Tech Stack:** virtio 1.2 spec (split virtqueues, PCI transport), PCI 3.0
configuration space, MSI-X, the T3 runtime (`task::block_on`, `sched::unpark`).

## Global Constraints

From `CLAUDE.md`; every task's requirements implicitly include these.

- `no_std` in every crate touched. `xtask` and `fuzz/` are the only host-only
  members.
- No floating point in kernel crates. The target is soft-float.
- Edition 2024 unsafe attributes: `#[unsafe(no_mangle)]`, `#[unsafe(naked)]`.
- `static mut` is banned. `UnsafeCell` in a `Sync` newtype, naming the single
  writer.
- No TODOs or stubs. A required future change is a compile error, not a note.
- Comments explain *why*. When you change behaviour, re-read the comment above
  it.
- Every commit leaves `cargo xtask test` green — **both boots**, with and
  without `sched-invariants`.
- Host crates test against musl.
- Never widen `TOLERANCE_PP` to make the coverage ratchet pass.

## Design decisions

### PCI is reached through the legacy configuration ports, not ECAM

`0xCF8`/`0xCFC`, not memory-mapped configuration space.

ECAM is the modern mechanism and is what a grown-up kernel uses. It also
requires the base address of the MMIO window, which is published only in the
ACPI `MCFG` table — so ECAM means an ACPI parser: RSDP discovery, XSDT walking,
table checksums, and a new class of attacker-controlled input to validate. That
is a subsystem, and it buys nothing here: q35 implements the legacy ports for
bus 0, which is where the device is.

The cost is real and bounded: the legacy ports reach only the first 256 bytes of
configuration space. Virtio 1.0 puts its capabilities inside that window, so
nothing this milestone needs is out of reach. **A device whose capability list
runs past offset 255 is refused rather than truncated** — silently stopping the
walk would look identical to a device with fewer capabilities.

Limine already provides the HHDM, so mapping the BARs needs no ACPI either.

### Modern virtio (1.0+), not the legacy interface

Feature negotiation refuses a device that does not offer `VIRTIO_F_VERSION_1`.

Legacy virtio puts its registers in an I/O BAR at fixed offsets, with a
queue-address register that assumes 4 KiB pages and hands the device a page
frame number. Modern virtio puts each structure behind a PCI capability with an
explicit BAR, offset and length, and takes full 64-bit physical addresses for
the three ring components separately. The second is more code to discover and
much less to get wrong: nothing is inferred from a layout constant.

QEMU's `virtio-blk-pci` is modern by default.

### MSI-X, and it is the *simpler* choice here

The spec asks for MSI-X per queue, and that reads like the ambitious option. It
is the opposite.

The alternative is INTx, the legacy pin interrupt — which arrives at an I/O APIC
redirection entry, and finding the I/O APIC means the ACPI `MADT`, which means
the ACPI parser this plan just declined to write. MSI-X is a *device-initiated
memory write* to an address the kernel already understands: the LAPIC's message
address, with the vector in the data word. The kernel has a LAPIC and knows its
address. So MSI-X costs one BAR mapping and two register writes, and INTx costs
a table parser.

Vector 35: 32 is the timer, 33 the TLB shootdown, 34 the wake IPI.

### One virtqueue, depth 64

virtio-blk may offer multiple queues. One is used, and requests from every
processor serialise on it behind a lock.

That is a real bottleneck and it is deliberate: a per-CPU queue is a measured
optimisation, and there is nothing to measure until a filesystem is generating
load. The buffer cache in T5 is what will produce it. Depth 64 matches the timer
table's `CAPACITY` for the same reason — it is "every thread the kernel runs,
with room" — and the queue **refuses** a request when full rather than
overwriting a live descriptor.

### The device sees physical addresses, and there is no IOMMU

DMA buffers come from `frames::alloc` and are handed to the device by physical
address; the kernel reaches the same memory through the HHDM. No IOMMU is
programmed, so **the device can write anywhere it is told to**, and every
address published in a descriptor is one the kernel must have derived itself.

This is why `read_at` copies into the caller's slice from a kernel-owned bounce
buffer rather than publishing the caller's address: a caller's buffer may be
partially outside a frame, span non-contiguous frames, or — once syscalls reach
this path — be a user pointer. One place derives device-visible addresses, and
it derives them from frames it allocated.

### The ring arithmetic lives in a host-tested crate

`crates/qunix-virtio` holds the split-virtqueue layout and index arithmetic and
knows nothing about PCI, MMIO or interrupts. It is where the descriptor table,
available ring and used ring are constructed and walked.

This is the same trade `qunix-mm` and `qunix-sched` make, for the same reason:
the failure mode is arithmetic that hands the device the wrong buffer, and that
is a wrong answer rather than a crash. `u16` ring indices that wrap at 65536
while the ring holds 64 entries are exactly the shape of bug this project has
shipped twice in the allocators.

## File structure

| File | Responsibility |
| --- | --- |
| `crates/qunix-virtio/Cargo.toml` (new) | `no_std` + `std` feature, like `qunix-mm` |
| `crates/qunix-virtio/src/lib.rs` (new) | Ring layout constants, `Descriptor`, `RingLayout` |
| `crates/qunix-virtio/src/queue.rs` (new) | `SplitQueue`: descriptor allocation, avail/used arithmetic |
| `crates/qunix-virtio/src/blk.rs` (new) | virtio-blk request header layout and status decoding |
| `crates/qunix-hal-x86_64/src/pci.rs` (new) | Config-space addressing, enumeration, capability walk, MSI-X |
| `kernel/src/virtio.rs` (new) | PCI transport: capability discovery, feature negotiation, queue setup |
| `kernel/src/block.rs` (new) | `read_at`/`write_at`, the completion futures, the interrupt handler |
| `kernel/src/main.rs` (modify) | `mod virtio; mod block;`, vector 35, in-QEMU tests |
| `xtask/src/qemu.rs` (modify) | Attach a virtio-blk drive |
| `xtask/src/image.rs` (modify) | Generate the test disk image |

`qunix-virtio` is a new crate rather than a kernel module because the whole
point is that it builds and tests on the host.

---

### Task 1: PCI configuration space

**Files:**
- Create: `crates/qunix-hal-x86_64/src/pci.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs` (add `pub mod pci;`)
- Test: in `pci.rs` (`#[cfg(test)] mod tests`, host tests — the crate already
  builds for the host)

**Interfaces:**
- Consumes: `crate::port::{inl, outl}`.
- Produces:
  - `pub struct Bdf { bus: u8, device: u8, function: u8 }` with
    `Bdf::new(bus, device, function) -> Option<Bdf>`
  - `pub const fn config_address(bdf: Bdf, offset: u8) -> u32`
  - `pub unsafe fn config_read32(bdf: Bdf, offset: u8) -> u32`
  - `pub unsafe fn config_write32(bdf: Bdf, offset: u8, value: u32)`
  - `pub unsafe fn find_device(vendor: u16, device: u16) -> Option<Bdf>`
  - `pub enum CapError { Unaligned(u8), OutOfRange(u8), Cycle }`
  - `pub unsafe fn capabilities(bdf: Bdf) -> Result<CapIter, CapError>`

- [ ] **Step 1: Write the failing tests**

The address encoding and the capability walk are pure; test them on the host.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_address_sets_the_enable_bit_and_aligns_the_offset() {
        // Bit 31 is the enable bit; without it the write to 0xCF8 selects
        // nothing and the read from 0xCFC returns whatever was last latched --
        // which looks exactly like a device that answered all-ones.
        let addr = config_address(Bdf::new(0, 3, 0).unwrap(), 0x10);
        assert_eq!(addr & (1 << 31), 1 << 31, "the enable bit is clear");
        // The low two bits are reserved and must be zero: the port addresses a
        // dword. A non-zero value there selects a different register on real
        // hardware and is silently ignored on some, which is worse.
        assert_eq!(addr & 0b11, 0, "the offset was not dword-aligned");
        assert_eq!(addr, 0x8000_1810, "the encoding drifted");
    }

    #[test]
    fn the_fields_land_in_their_own_bits() {
        // Each field asserted alone, so a shift that is wrong by one is not
        // masked by another field happening to be zero.
        assert_eq!(config_address(Bdf::new(0xff, 0, 0).unwrap(), 0) >> 16 & 0xff, 0xff);
        assert_eq!(config_address(Bdf::new(0, 0x1f, 0).unwrap(), 0) >> 11 & 0x1f, 0x1f);
        assert_eq!(config_address(Bdf::new(0, 0, 7).unwrap(), 0) >> 8 & 0x7, 7);
        assert_eq!(config_address(Bdf::new(0, 0, 0).unwrap(), 0xfc) & 0xfc, 0xfc);
    }

    #[test]
    fn an_out_of_range_device_or_function_is_refused() {
        // The negative direction. A device number above 31 or a function above
        // 7 does not fit its field, and truncating silently addresses a
        // *different* device -- a config write then lands on a device the
        // caller never named.
        assert!(Bdf::new(0, 32, 0).is_none(), "device 32 does not fit five bits");
        assert!(Bdf::new(0, 0, 8).is_none(), "function 8 does not fit three bits");
        assert!(Bdf::new(255, 31, 7).is_some(), "the largest legal address was refused");
    }

    #[test]
    fn a_capability_pointer_that_loops_is_refused_rather_than_walked_forever() {
        // Capability lists are a linked list in device-controlled memory, and
        // a device that points a capability at itself is a hang in the kernel's
        // enumeration path. Bounded and refused, because "the device is
        // malformed" is a thing the caller can act on and a hang is not.
        let mut cfg = [0u8; 256];
        cfg[0x34] = 0x40;          // capability pointer
        cfg[0x40] = 0x09;          // vendor-specific
        cfg[0x41] = 0x40;          // next -> itself
        assert_eq!(walk_capabilities(&cfg), Err(CapError::Cycle));
    }

    #[test]
    fn a_capability_pointer_past_the_legacy_window_is_refused() {
        // The legacy config ports reach only the first 256 bytes. A pointer
        // beyond that is not readable by this driver, and truncating the walk
        // would be indistinguishable from a device with fewer capabilities --
        // so the device would look like it lacked the virtio capability it
        // actually has.
        let mut cfg = [0u8; 256];
        cfg[0x34] = 0xfc;
        cfg[0xfc] = 0x09;
        cfg[0xfd] = 0x00;
        assert!(walk_capabilities(&cfg).is_ok(), "the last readable capability was refused");

        let mut past = [0u8; 256];
        past[0x34] = 0xfe;         // a header straddling the end of the window
        assert_eq!(walk_capabilities(&past), Err(CapError::OutOfRange(0xfe)));
    }

    #[test]
    fn a_misaligned_capability_pointer_is_refused() {
        // PCI requires capability structures to be dword-aligned. A device
        // reporting otherwise is malformed, and following it reads a header
        // straddling two registers.
        let mut cfg = [0u8; 256];
        cfg[0x34] = 0x41;
        assert_eq!(walk_capabilities(&cfg), Err(CapError::Unaligned(0x41)));
    }

    #[test]
    fn an_empty_capability_pointer_yields_no_capabilities() {
        // Zero terminates the list, and must not be followed. A walk that
        // treated 0 as an offset would read the vendor ID as a capability.
        let cfg = [0u8; 256];
        assert_eq!(walk_capabilities(&cfg), Ok(alloc::vec![]));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run:
```sh
cargo test -p qunix-hal-x86_64 --features std --target x86_64-unknown-linux-musl
```
Expected: FAIL — `cannot find function config_address`.

- [ ] **Step 3: Implement**

`walk_capabilities` takes a `&[u8; 256]` snapshot rather than doing port I/O, so
the whole walk is host-testable; the hardware path reads the snapshot first.

```rust
//! PCI configuration space through the legacy ports.
//!
//! `0xCF8`/`0xCFC` rather than memory-mapped configuration space. ECAM is the
//! modern mechanism and needs the window's base address, which is published
//! only in the ACPI `MCFG` table -- so ECAM means an ACPI parser, and a new
//! class of attacker-controlled input to validate, for a device that q35 puts
//! on bus 0 where the legacy ports reach it.
//!
//! The cost is that only the first 256 bytes of each device's configuration
//! space are readable. Virtio 1.0 keeps its capabilities inside that window, so
//! nothing here needs more -- but a capability list running past the end is
//! *refused* rather than truncated, because a truncated walk looks exactly like
//! a device that has fewer capabilities than it does.

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

/// Bus, device and function: a device's address on the PCI tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bdf {
    bus: u8,
    device: u8,
    function: u8,
}

impl Bdf {
    /// Refuses a device or function that does not fit its field.
    ///
    /// Truncating instead would address a *different* device: device 32 has the
    /// same low five bits as device 0, so a config write meant for one would
    /// land on the other. Refusing is the only outcome a caller can act on.
    pub const fn new(bus: u8, device: u8, function: u8) -> Option<Self> {
        if device > 31 || function > 7 {
            return None;
        }
        Some(Self { bus, device, function })
    }
}

/// The value written to `0xCF8` to select one dword of configuration space.
pub const fn config_address(bdf: Bdf, offset: u8) -> u32 {
    // Bit 31 enables the mechanism. Without it the write selects nothing and
    // the subsequent read returns whatever was last latched -- which is
    // indistinguishable from a device answering all-ones, i.e. "absent".
    (1 << 31)
        | ((bdf.bus as u32) << 16)
        | ((bdf.device as u32) << 11)
        | ((bdf.function as u32) << 8)
        // The low two bits are reserved: the port addresses a dword, and the
        // caller's byte offset is rounded down here rather than asserted, so
        // that reading a byte field by its own offset works.
        | ((offset as u32) & 0xfc)
}

/// Why a capability list could not be walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// A capability offset was not dword-aligned. PCI requires it; following
    /// it would read a header straddling two registers.
    Unaligned(u8),
    /// A capability header would extend past the 256-byte window the legacy
    /// ports can reach.
    OutOfRange(u8),
    /// The list points back into itself.
    Cycle,
}

/// One capability: its id and where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub id: u8,
    pub offset: u8,
}

/// Walks the capability list in a configuration-space snapshot.
///
/// Takes a snapshot rather than reading ports, so every refusal above is
/// host-testable. The hardware path reads the 256 bytes first and calls this.
pub fn walk_capabilities(cfg: &[u8; 256]) -> Result<alloc::vec::Vec<Capability>, CapError> {
    let mut caps = alloc::vec::Vec::new();
    let mut offset = cfg[0x34];
    // Bounded by the number of dword-aligned slots in the window, so a list
    // that is long but acyclic still terminates and a cycle is reported rather
    // than spun on.
    for _ in 0..64 {
        if offset == 0 {
            return Ok(caps);
        }
        if offset & 0b11 != 0 {
            return Err(CapError::Unaligned(offset));
        }
        // A header is two bytes: id then next.
        if offset as usize + 1 >= cfg.len() {
            return Err(CapError::OutOfRange(offset));
        }
        if caps.iter().any(|c: &Capability| c.offset == offset) {
            return Err(CapError::Cycle);
        }
        caps.push(Capability { id: cfg[offset as usize], offset });
        offset = cfg[offset as usize + 1];
    }
    Err(CapError::Cycle)
}
```

Plus the port-I/O wrappers and `find_device`, which scans bus 0 for a matching
vendor/device pair and returns the first match.

- [ ] **Step 4: Run to verify they pass**

Expected: PASS, 6 new host tests.

- [ ] **Step 5: Falsify each guard**

| Mutation | Test that must fail |
| --- | --- |
| `config_address`: drop the `1 << 31` | `a_config_address_sets_the_enable_bit_and_aligns_the_offset` |
| `config_address`: `& 0xfc` → `& 0xff` | same test's alignment assertion |
| `Bdf::new`: accept any device | `an_out_of_range_device_or_function_is_refused` |
| `walk_capabilities`: drop the cycle check | `a_capability_pointer_that_loops_is_refused_rather_than_walked_forever` (as a hang — bound the test if so) |
| `walk_capabilities`: `>= cfg.len()` → `> cfg.len()` | `a_capability_pointer_past_the_legacy_window_is_refused` |
| `walk_capabilities`: drop the alignment check | `a_misaligned_capability_pointer_is_refused` |

- [ ] **Step 6: Commit**

```sh
cargo xtask test   # both boots
git add crates/qunix-hal-x86_64/src/pci.rs crates/qunix-hal-x86_64/src/lib.rs
git commit -m "feat(hal): PCI configuration space through the legacy ports"
```

---

### Task 2: The split virtqueue

The heart of the milestone, and the only part that can be wrong without
crashing. Everything here is host-tested.

**Files:**
- Create: `crates/qunix-virtio/Cargo.toml`, `crates/qunix-virtio/src/lib.rs`,
  `crates/qunix-virtio/src/queue.rs`
- Modify: root `Cargo.toml` (workspace members), `xtask/src/coverage.rs`
  baseline, `xtask/src/main.rs` host-test package list
- Test: `crates/qunix-virtio/src/queue.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const QUEUE_SIZE: u16 = 64;`
  - `pub struct Descriptor { addr: u64, len: u32, flags: u16, next: u16 }`
  - `pub struct RingLayout { desc: usize, avail: usize, used: usize, bytes: usize }`
  - `pub const fn ring_layout(size: u16) -> RingLayout`
  - `pub struct SplitQueue` with:
    - `SplitQueue::new(size: u16) -> Self`
    - `alloc_chain(&mut self, n: u16) -> Option<u16>` — head index, or `None`
    - `free_chain(&mut self, head: u16)`
    - `publish(&mut self, head: u16) -> u16` — new avail index
    - `take_used(&mut self, used_idx: u16) -> Option<(u16, u32)>` — (head, len)
    - `free_count(&self) -> u16`
    - `last_slot(&self) -> u16` — the ring slot the last `publish` wrote
    - `#[cfg(test)] set_avail_index_for_test(&mut self, idx: u16)`
    - `#[cfg(test)] write_used_for_test(&mut self, slot: u16, id: u16, len: u32)`
  - The ring bytes, for the kernel to copy into DMA memory:
    - `desc_bytes(&self) -> &[u8]`, `avail_bytes(&self) -> &[u8]`,
      `used_bytes_mut(&mut self) -> &mut [u8]`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_layout_matches_the_alignments_the_spec_requires() {
        // Every offset checked against the spec's rule, not against a
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
        assert!(l.avail >= l.desc + 16 * QUEUE_SIZE as usize, "avail overlaps desc");
        assert!(l.used >= l.avail + 6 + 2 * QUEUE_SIZE as usize, "used overlaps avail");
        assert!(l.bytes >= l.used + 6 + 8 * QUEUE_SIZE as usize, "the ring is short");
    }

    #[test]
    fn a_queue_hands_out_every_descriptor_and_then_refuses() {
        // The refusal is the point. A queue that wrapped and reused a live
        // descriptor would overwrite a request the device is still reading --
        // and the device would complete the *wrong* request, reporting success.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let mut heads = alloc::vec![];
        for _ in 0..QUEUE_SIZE {
            heads.push(q.alloc_chain(1).expect("a free descriptor was refused"));
        }
        assert_eq!(q.free_count(), 0);
        assert_eq!(q.alloc_chain(1), None, "a full queue handed out a live descriptor");
        // Every index distinct: a free list that linked a descriptor to itself
        // would hand the same one out twice while the count still looked right.
        heads.sort_unstable();
        heads.dedup();
        assert_eq!(heads.len(), QUEUE_SIZE as usize, "a descriptor was handed out twice");
    }

    #[test]
    fn a_chain_longer_than_the_queue_is_refused_rather_than_truncated() {
        // A truncated chain is a request whose data buffer is missing: the
        // device reads the header, finds no NEXT, and completes a request that
        // transferred nothing -- successfully.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        assert_eq!(q.alloc_chain(QUEUE_SIZE + 1), None);
        assert_eq!(q.free_count(), QUEUE_SIZE, "the refused chain consumed descriptors");
    }

    #[test]
    fn freeing_a_chain_returns_every_descriptor_in_it() {
        // Not just the head. A free that released only the head leaks the rest,
        // and the queue runs dry after `QUEUE_SIZE / 3` requests -- long after
        // the code that leaked them ran.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(3).unwrap();
        assert_eq!(q.free_count(), QUEUE_SIZE - 3);
        q.free_chain(head);
        assert_eq!(q.free_count(), QUEUE_SIZE, "freeing a chain leaked descriptors");
    }

    #[test]
    fn the_available_index_wraps_at_the_ring_size_but_counts_past_it() {
        // The subtle one, and the reason this crate exists. The avail *index*
        // is a free-running u16 that the device reduces modulo the queue size;
        // the *slot* it names wraps at the queue size. Conflating the two makes
        // the driver publish into slot `idx % 65536`, which is out of the ring
        // for any queue smaller than 65536 -- i.e. always.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        for i in 0..(QUEUE_SIZE as u32 * 3) {
            let head = q.alloc_chain(1).unwrap();
            let idx = q.publish(head);
            assert_eq!(idx as u32, i + 1, "the available index did not advance monotonically");
            assert_eq!(
                q.last_slot(),
                (i % QUEUE_SIZE as u32) as u16,
                "the published slot did not wrap at the queue size"
            );
            q.free_chain(head);
        }
    }

    #[test]
    fn the_available_index_survives_a_u16_wrap() {
        // 65536 requests is minutes of filesystem traffic, and the wrap is
        // where a `>` that should be `!=` turns into a queue that stops
        // publishing. Driven directly rather than by 65536 round trips.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        q.set_avail_index_for_test(u16::MAX);
        let head = q.alloc_chain(1).unwrap();
        assert_eq!(q.publish(head), 0, "the available index did not wrap to zero");
        assert_eq!(q.last_slot(), (u16::MAX % QUEUE_SIZE) as u16);
    }

    #[test]
    fn a_used_entry_naming_a_descriptor_outside_the_ring_is_refused() {
        // The device writes the used ring, so its contents are device-supplied
        // input in exactly the sense the ELF loader's input is. An id past the
        // end of the descriptor table would index out of bounds; a *free*
        // descriptor means the device completed something never submitted.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        q.write_used_for_test(0, QUEUE_SIZE, 512);
        assert_eq!(q.take_used(0), None, "a used id past the ring was accepted");
        q.write_used_for_test(0, 3, 512);
        assert_eq!(q.take_used(0), None, "a used id naming a free descriptor was accepted");
    }

    #[test]
    fn a_used_entry_is_taken_exactly_once() {
        // Taking one twice frees its chain twice, which puts one descriptor on
        // the free list twice -- and the next two allocations hand the same
        // descriptor to two requests.
        let mut q = SplitQueue::new(QUEUE_SIZE);
        let head = q.alloc_chain(2).unwrap();
        q.publish(head);
        q.write_used_for_test(0, head, 512);
        assert_eq!(q.take_used(0), Some((head, 512)));
        assert_eq!(q.take_used(0), None, "the same completion was taken twice");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run:
```sh
cargo test -p qunix-virtio --features std --target x86_64-unknown-linux-musl
```
Expected: FAIL — the crate does not exist. Create `Cargo.toml` modelled on
`crates/qunix-mm/Cargo.toml` (same `std` feature shape) and add the member to
the root `Cargo.toml`.

- [ ] **Step 3: Implement `queue.rs`**

`SplitQueue` owns the ring *contents* as plain arrays; the kernel copies them
into DMA memory and tells the device where they are. That separation is what
lets this be host-tested: the crate never dereferences a physical address.

Key points the implementation must honour, each already asserted above:

- the free list is an explicit `next` chain through the descriptor table, with
  `free_count` tracked separately so a corrupted chain cannot silently look
  full;
- `alloc_chain(n)` fails atomically — a refusal consumes nothing;
- `publish` advances a free-running `u16` and writes the ring slot at
  `idx % size`;
- `take_used` validates the device-supplied id against both the ring bound and
  the *in-use* set before touching it.

- [ ] **Step 4: Run to verify they pass**

Expected: PASS, 8 new host tests.

- [ ] **Step 5: Falsify each guard**

| Mutation | Test that must fail |
| --- | --- |
| `publish`: write slot at `idx` instead of `idx % size` | `the_available_index_wraps_at_the_ring_size_but_counts_past_it` |
| `publish`: saturating instead of wrapping index | `the_available_index_survives_a_u16_wrap` |
| `alloc_chain`: allow `n > free_count` | `a_chain_longer_than_the_queue_is_refused_rather_than_truncated` |
| `alloc_chain`: reuse the head when the list is empty | `a_queue_hands_out_every_descriptor_and_then_refuses` |
| `free_chain`: free only the head | `freeing_a_chain_returns_every_descriptor_in_it` |
| `take_used`: drop the bound check | `a_used_entry_naming_a_descriptor_outside_the_ring_is_refused` |
| `take_used`: drop the in-use check | `a_used_entry_is_taken_exactly_once` |

- [ ] **Step 6: Add the crate to the ratchet and commit**

Add `qunix-virtio` to the host-test package list in `xtask/src/main.rs` and to
the coverage baseline. Then:

```sh
cargo xtask test
git add crates/qunix-virtio Cargo.toml xtask/src
git commit -m "feat(virtio): split virtqueue rings, host-tested"
```

---

### Task 3: virtio-blk request layout

**Files:**
- Create: `crates/qunix-virtio/src/blk.rs`
- Modify: `crates/qunix-virtio/src/lib.rs` (`pub mod blk;`)
- Test: in `blk.rs`

**Interfaces:**
- Produces:
  - `pub const SECTOR_BYTES: usize = 512;`
  - `#[repr(C)] pub struct RequestHeader { kind: u32, reserved: u32, sector: u64 }`
    with `RequestHeader::read(sector: u64) -> Self`,
    `RequestHeader::write(sector: u64) -> Self`, and
    `as_bytes(&self) -> &[u8; 16]`
  - `pub const REQUEST_IN: u32 = 0; pub const REQUEST_OUT: u32 = 1;`
  - `pub enum BlkStatus { Ok, IoError, Unsupported, Unknown(u8) }`
  - `pub const fn status_from_byte(b: u8) -> BlkStatus`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_request_header_has_the_layout_the_device_reads() {
    // The device parses these bytes at a physical address; a field in the
    // wrong place is a read of the wrong sector, reported as success.
    assert_eq!(core::mem::size_of::<RequestHeader>(), 16);
    assert_eq!(core::mem::align_of::<RequestHeader>(), 8);
    let h = RequestHeader::read(0x1122_3344_5566_7788);
    let bytes = h.as_bytes();
    assert_eq!(&bytes[0..4], &REQUEST_IN.to_le_bytes(), "kind is not first, little-endian");
    assert_eq!(&bytes[8..16], &0x1122_3344_5566_7788u64.to_le_bytes(), "sector is not at offset 8");
}

#[test]
fn an_unknown_status_byte_is_reported_rather_than_treated_as_success() {
    // The status byte is device-supplied. Mapping anything that is not 0 to a
    // generic error is fine; mapping an *unknown* value to `Ok` is a silent
    // data-corruption bug, so the negative direction is what is asserted.
    assert!(matches!(status_from_byte(0), BlkStatus::Ok));
    assert!(matches!(status_from_byte(1), BlkStatus::IoError));
    assert!(matches!(status_from_byte(2), BlkStatus::Unsupported));
    assert!(matches!(status_from_byte(0xff), BlkStatus::Unknown(0xff)));
    for b in 3..=u8::MAX {
        assert!(!matches!(status_from_byte(b), BlkStatus::Ok), "status {b} decoded as success");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run:
```sh
cargo test -p qunix-virtio --features std --target x86_64-unknown-linux-musl
```
Expected: FAIL — `cannot find type RequestHeader`.

- [ ] **Step 3: Implement**

`RequestHeader` is `#[repr(C)]` with the three fields in the order the device
reads them, and `as_bytes` transmutes to a byte array rather than serialising
field by field — the layout *is* the contract, so a manual serialiser would let
the struct and the wire format drift apart. `status_from_byte` maps 0/1/2 and
carries anything else through as `Unknown(b)`.

- [ ] **Step 4: Run to verify they pass**

Expected: PASS, 2 new host tests.

- [ ] **Step 5: Falsify**

| Mutation | Test that must fail |
| --- | --- |
| swap `kind` and `sector` in the struct | `the_request_header_has_the_layout_the_device_reads` |
| `status_from_byte`: `_ => BlkStatus::Ok` | `an_unknown_status_byte_is_reported_rather_than_treated_as_success` |

- [ ] **Step 6: Commit**

```sh
git commit -m "feat(virtio): virtio-blk request layout and status decoding"
```

---

### Task 4: A disk for the guest

Done before the driver, so the driver has something to talk to from its first
boot and no task is blocked on a missing image.

**Files:**
- Modify: `xtask/src/image.rs` (create the test disk), `xtask/src/qemu.rs`
  (attach it)
- Test: `xtask/src/qemu.rs` (the args test already there)

**Interfaces:**
- Produces: `pub fn build_test_disk(target_dir: &Path) -> Result<PathBuf>`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn the_test_disk_reaches_the_guest_as_a_virtio_block_device() {
    let a = qemu_args("/fw/OVMF.fd", Path::new("/t/esp"), Path::new("/t/disk.img"), true, false);
    // The drive and the device are two halves of one thing: a drive with no
    // device is invisible to the guest, and a device with no drive makes QEMU
    // refuse to start. Both asserted, because either alone looks like success
    // in the arg list.
    assert!(a.iter().any(|x| x.contains("file=/t/disk.img")), "{a:?}");
    assert!(
        a.windows(2).any(|w| w[0] == "-device" && w[1].starts_with("virtio-blk-pci")),
        "{a:?}"
    );
    // And they must be joined by id, or QEMU attaches the device to nothing.
    let id = a.iter().find(|x| x.contains("id=")).expect("the drive has no id");
    let drive_id = id.split("id=").nth(1).unwrap().split(',').next().unwrap();
    assert!(
        a.iter().any(|x| x.contains(&format!("drive={drive_id}"))),
        "the device does not name the drive: {a:?}"
    );
}
```

- [ ] **Step 2: Implement**

`build_test_disk` writes a 1 MiB raw image whose every sector begins with its
own LBA in little-endian, so a read of sector *n* has a verifiable expected
value and a read of the *wrong* sector is detectable rather than plausible. It
is regenerated only when absent, and `prune_unexpected` must not delete it (it
lives outside the ESP).

QEMU gains:

```rust
push!("-drive");
args.push(format!("format=raw,if=none,id=qunixdisk,file={}", disk.display()));
push!("-device", "virtio-blk-pci,drive=qunixdisk,disable-legacy=on,disable-modern=off");
```

`disable-legacy=on` is not decoration: it makes QEMU refuse to present the
legacy interface, so a driver bug that falls back to it fails loudly here
instead of working in QEMU and breaking on hardware.

- [ ] **Step 3: Run, commit**

```sh
cargo xtask test
git commit -m "test(xtask): give the guest a virtio-blk disk with self-identifying sectors"
```

---

### Task 5: The virtio-pci transport

**Files:**
- Create: `kernel/src/virtio.rs`
- Modify: `kernel/src/main.rs` (`mod virtio;`)
- Test: `kernel/src/virtio.rs` (`#[test_case]`)

**Interfaces:**
- Consumes: `qunix_hal_x86_64::pci::*`, `qunix_virtio::{ring_layout, QUEUE_SIZE}`,
  `crate::frames::alloc`, `crate::boot::hhdm_offset`.
- Produces:
  - `pub struct Transport` — the four capability windows, mapped
  - `pub unsafe fn probe(vendor: u16, device: u16) -> Result<Transport, ProbeError>`
  - `pub enum ProbeError { NotFound, NoModernCapability, NoVersion1, BadBar(u8), QueueTooSmall(u16) }`
  - `Transport::negotiate(&mut self, wanted: u64) -> Result<u64, ProbeError>`
  - `Transport::configure_queue(&mut self, desc: u64, avail: u64, used: u64)`
  - `Transport::notify(&self, queue: u16)`

- [ ] **Step 1: Write the failing tests**

The device-status handshake is a state machine and is the part worth asserting
in-QEMU, because getting it wrong leaves a device that looks initialised and
silently ignores every request.

```rust
#[test_case]
fn the_device_reaches_driver_ok_and_keeps_version_1() {
    // The handshake is ordered and the device enforces it: ACKNOWLEDGE, then
    // DRIVER, then features, then FEATURES_OK, then DRIVER_OK. A driver that
    // skips a step gets a device that accepts configuration and ignores it.
    let t = unsafe { probe(VIRTIO_VENDOR, VIRTIO_BLK_DEVICE) }.expect("no virtio-blk device");
    assert_eq!(t.status() & STATUS_DRIVER_OK, STATUS_DRIVER_OK);
    // FEATURES_OK must still be set: the device clears it to *refuse* the
    // feature set, and a driver that does not re-read it proceeds against a
    // device that has rejected it.
    assert_eq!(t.status() & STATUS_FEATURES_OK, STATUS_FEATURES_OK,
        "the device rejected the negotiated features and the driver did not notice");
    assert_eq!(t.status() & STATUS_FAILED, 0, "the device reported failure");
}

#[test_case]
fn a_device_without_version_1_is_refused() {
    // Negotiation is asserted by *offering* nothing: `negotiate(0)` must fail
    // rather than proceed, because a driver that does not require
    // VIRTIO_F_VERSION_1 is talking legacy protocol to a modern device -- the
    // rings are laid out differently and every request reads the wrong bytes.
    let mut t = unsafe { probe(VIRTIO_VENDOR, VIRTIO_BLK_DEVICE) }.expect("no device");
    assert!(matches!(t.negotiate(0), Err(ProbeError::NoVersion1)));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo xtask test`
Expected: FAIL to compile — `cannot find function probe`.

- [ ] **Step 3: Implement**

The implementation walks the capability list for the four virtio structures
(`COMMON_CFG=1`, `NOTIFY_CFG=2`, `ISR_CFG=3`, `DEVICE_CFG=4`), maps each BAR
region uncacheable through the HHDM using the pattern `map_lapic` establishes,
and drives the status handshake. Every BAR is validated before mapping: a
capability whose offset plus length exceeds its BAR is `BadBar`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo xtask test`
Expected: `all 79 tests passed`, on both boots.

- [ ] **Step 5: Falsify**

| Mutation | Test that must fail |
| --- | --- |
| skip re-reading status after `FEATURES_OK` | `the_device_reaches_driver_ok_and_keeps_version_1` |
| `negotiate`: accept a feature set without `VERSION_1` | `a_device_without_version_1_is_refused` |
| write `DRIVER_OK` before configuring the queue | `the_device_reaches_driver_ok_and_keeps_version_1` (the device sets `FAILED`) |

- [ ] **Step 6: Commit**

```sh
git commit -m "feat(virtio): the modern PCI transport and device handshake"
```

---

### Task 6: MSI-X

Split from the transport because it is the one piece with no in-QEMU test of
its own until Task 7 exists — a reviewer can reject the vector wiring without
rejecting the handshake, and the encoding is host-testable on its own.

**Files:**
- Modify: `crates/qunix-hal-x86_64/src/pci.rs` (the MSI-X capability),
  `kernel/src/virtio.rs` (programming it)
- Test: `crates/qunix-hal-x86_64/src/pci.rs`

**Interfaces:**
- Consumes: `Capability`, `crate::apic::phys_base`.
- Produces:
  - `pub const CAP_ID_MSIX: u8 = 0x11;`
  - `pub const fn msix_message_address(lapic_base: u64, cpu: u8) -> u64`
  - `pub const fn msix_message_data(vector: u8) -> u32`
  - `pub struct MsixTable { base: u64, entries: u16 }`
  - `MsixTable::program(&mut self, entry: u16, address: u64, data: u32) -> Result<(), MsixError>`
  - `pub enum MsixError { NoSuchEntry(u16) }`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_message_address_targets_one_processor_and_stays_in_the_lapic_window() {
    // An MSI is a memory write to the LAPIC's address, with the destination
    // processor in bits 19:12. Getting the shift wrong sends every completion
    // to processor 0 -- which *works* on a single-processor guest and silently
    // stops working when the waiting thread is elsewhere.
    let addr = msix_message_address(0xfee0_0000, 3);
    assert_eq!(addr & 0xfff0_0000, 0xfee0_0000, "the message left the LAPIC window");
    assert_eq!((addr >> 12) & 0xff, 3, "the destination processor is wrong");
    assert_ne!(msix_message_address(0xfee0_0000, 0), addr,
        "every processor got the same message address");
}

#[test]
fn a_message_data_word_carries_the_vector_and_nothing_else() {
    // Delivery mode must be fixed (000) and the trigger edge: a level-triggered
    // MSI needs an EOI protocol the kernel does not implement, and the device
    // would stop delivering after the first completion.
    let data = msix_message_data(35);
    assert_eq!(data & 0xff, 35, "the vector is not in the low byte");
    assert_eq!((data >> 8) & 0b111, 0, "delivery mode is not Fixed");
    assert_eq!((data >> 15) & 1, 0, "the message is level-triggered");
}

#[test]
fn programming_an_entry_past_the_table_is_refused() {
    // The entry count comes from the device's own capability. Trusting it past
    // its stated size writes into whatever follows the table in the BAR --
    // which is device registers.
    let mut table = MsixTable { base: 0x1000, entries: 2 };
    assert_eq!(table.program(2, 0, 0), Err(MsixError::NoSuchEntry(2)));
    assert_eq!(table.program(1, 0, 0), Ok(()));
}
```

- [ ] **Step 2: Run to verify they fail**

Run:
```sh
cargo test -p qunix-hal-x86_64 --features std --target x86_64-unknown-linux-musl
```
Expected: FAIL — `cannot find function msix_message_address`.

- [ ] **Step 3: Implement**

`MsixTable::program` writes the four dwords of an entry (address low, address
high, data, vector control) and clears the mask bit last, so an entry is never
live with a half-written address. The capability's Message Control register
carries the table size minus one and the BAR indicator; both are read before the
table is mapped, and the global mask bit is cleared only after every entry is
programmed.

- [ ] **Step 4: Run to verify they pass**

Expected: PASS, 3 new host tests.

- [ ] **Step 5: Falsify**

| Mutation | Test that must fail |
| --- | --- |
| `msix_message_address`: drop the `<< 12` | `a_message_address_targets_one_processor_and_stays_in_the_lapic_window` |
| `msix_message_data`: set the level-trigger bit | `a_message_data_word_carries_the_vector_and_nothing_else` |
| `program`: accept any entry index | `programming_an_entry_past_the_table_is_refused` |

- [ ] **Step 6: Commit**

```sh
cargo xtask test
git add crates/qunix-hal-x86_64/src/pci.rs kernel/src/virtio.rs
git commit -m "feat(hal): MSI-X, so completions reach a LAPIC vector without an IOAPIC"
```

---

### Task 7: Async block reads and writes

**Files:**
- Create: `kernel/src/block.rs`
- Modify: `kernel/src/main.rs` (`mod block;`, vector 35, the handler)
- Test: `kernel/src/block.rs`

**Interfaces:**
- Consumes: everything above, plus `crate::task::block_on` and
  `crate::sched::unpark`.
- Produces:
  - `pub async fn read_at(lba: u64, buf: &mut [u8]) -> Result<(), BlockError>`
  - `pub async fn write_at(lba: u64, buf: &[u8]) -> Result<(), BlockError>`
  - `pub enum BlockError { Unaligned, TooLarge, QueueFull, Device(BlkStatus) }`
  - `pub extern "x86-interrupt" fn completion_handler(_: InterruptStackFrame)`
  - `pub fn completions() -> u64` — completions taken from the used ring since
    boot, so a test can require the interrupt handler to have run

- [ ] **Step 1: Write the failing tests**

```rust
#[test_case]
fn a_sector_reads_back_the_lba_it_was_written_with() {
    // The disk image is generated with each sector beginning with its own LBA,
    // so reading the *wrong* sector is detectable rather than plausible. A test
    // that only checked "the bytes are not zero" would pass on an off-by-one in
    // the descriptor chain.
    let mut buf = [0u8; SECTOR_BYTES];
    block_on(read_at(7, &mut buf)).expect("read failed");
    assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 7,
        "sector 7 does not identify itself; the request named the wrong sector");
}

#[test_case]
fn a_write_is_visible_to_a_later_read() {
    let mut out = [0xa5u8; SECTOR_BYTES];
    out[0..8].copy_from_slice(&0xdead_beefu64.to_le_bytes());
    block_on(write_at(11, &out)).expect("write failed");
    let mut back = [0u8; SECTOR_BYTES];
    block_on(read_at(11, &mut back)).expect("read failed");
    assert_eq!(back, out, "what came back is not what went out");
}

#[test_case]
fn a_misaligned_or_oversized_request_is_refused_before_it_reaches_the_device() {
    // Refused in the driver, not by the device. A buffer that is not a whole
    // number of sectors makes the device write past its end -- and it will,
    // because nothing between here and the DMA engine checks.
    let mut short = [0u8; SECTOR_BYTES - 1];
    assert!(matches!(block_on(read_at(0, &mut short)), Err(BlockError::Unaligned)));
    let mut huge = [0u8; SECTOR_BYTES * (QUEUE_SIZE as usize + 2)];
    assert!(matches!(block_on(read_at(0, &mut huge)), Err(BlockError::TooLarge)));
}

#[test_case]
fn a_read_completes_from_the_interrupt_handler_and_not_from_a_poll_loop() {
    // The claim the milestone is about. `block_on` parks; only the MSI-X
    // handler can make the thread runnable again, so the completion count must
    // have moved and the thread must have actually parked.
    let parked_before = crate::block::completions();
    let mut buf = [0u8; SECTOR_BYTES];
    block_on(read_at(3, &mut buf)).expect("read failed");
    assert!(crate::block::completions() > parked_before,
        "the request completed without the interrupt handler running");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo xtask test`
Expected: FAIL to compile — `cannot find function read_at`.

- [ ] **Step 3: Implement**

The request path: take the queue lock, allocate a three-descriptor chain
(header, data, status), copy the caller's bytes into a kernel bounce buffer for
a write, publish, notify, then release the lock and `park`. The handler drains
the used ring, frees each chain, and unparks the thread recorded against that
head. **The lock is released before parking** — holding it across a park would
stop every other processor submitting, and the deadlock would present as a hang.

- [ ] **Step 5: Falsify**

| Mutation | Test that must fail |
| --- | --- |
| `read_at`: drop the sector-multiple check | `a_misaligned_or_oversized_request_is_refused_before_it_reaches_the_device` |
| build the chain with the status descriptor writable-by-driver | `a_sector_reads_back_the_lba_it_was_written_with` (the device sets FAILED) |
| handler: free the chain without unparking | `a_read_completes_from_the_interrupt_handler_and_not_from_a_poll_loop` (as a hang — give it a rescuer) |
| `read_at`: publish without notifying | same, and it must fail rather than hang |

- [ ] **Step 4: Run to verify they pass**

Run: `cargo xtask test`
Expected: `all 83 tests passed`, on both boots.

- [ ] **Step 5: Update `CLAUDE.md` and commit**

Record: DMA addresses are physical and unprotected by any IOMMU, so every
address published in a descriptor must be one the kernel derived from a frame it
allocated; the used ring is device-supplied input and is validated like any
other; and the queue lock is never held across a park.

```sh
cargo xtask test
git add kernel/src/block.rs kernel/src/main.rs CLAUDE.md
git commit -m "feat(block): async reads and writes completed by the device interrupt"
```

---

## Execution Deviations

Record what reality contradicts here rather than diverging silently. M0 needed
sixteen, M1 seven, T3 two.

### D10 — PCI through the legacy ports rather than ECAM

**Spec said:** "virtio-blk over PCI" without naming the access mechanism.

**Plan does:** legacy `0xCF8`/`0xCFC`.

**Why:** ECAM's base address lives in the ACPI `MCFG` table, so ECAM means an
ACPI parser and a new class of attacker-controlled input, for a device q35 puts
where the legacy ports already reach it. The limitation — only the first 256
bytes of configuration space — is inside what virtio 1.0 needs, and a capability
list running past it is refused rather than truncated.

---

## What T4 does not do

- **No IOMMU.** The device addresses physical memory directly. Every published
  address is derived from a frame the kernel allocated, which is the only thing
  standing between a driver bug and an arbitrary memory write.
- **One queue, no multiqueue.** Requests from every processor serialise. T5's
  buffer cache is what will make that measurable.
- **No readahead, no merging, no elevator.** One request per call.
- **No partition table.** LBAs are absolute. T9's ext4 will read a whole-device
  filesystem.
