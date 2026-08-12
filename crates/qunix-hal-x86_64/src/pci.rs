//! PCI configuration space through the legacy ports.
//!
//! `0xCF8`/`0xCFC` rather than memory-mapped configuration space. ECAM is the
//! modern mechanism and needs the window's base address, which is published
//! only in the ACPI `MCFG` table — so ECAM means an ACPI parser: RSDP
//! discovery, XSDT walking, table checksums, and a new class of
//! attacker-controlled input to validate. That is a subsystem, and it buys
//! nothing here, because q35 implements the legacy ports for bus 0 and that is
//! where the device is.
//!
//! The cost is real and bounded: only the first 256 bytes of each device's
//! configuration space are reachable. Virtio 1.0 keeps its capabilities inside
//! that window, so nothing this milestone needs is out of reach — but a
//! capability *body* read past the end is **refused** rather than truncated,
//! because a truncated read looks exactly like a device with different
//! contents, and the structure it would corrupt is the one being looked for.
//! See [`capability_body`]; the list *walk* cannot overrun, because offsets are
//! dword-aligned `u8`s and a two-byte header at the largest of them still fits.

extern crate alloc;

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
    pub const fn device(&self) -> u8 {
        self.device
    }

    pub const fn function(&self) -> u8 {
        self.function
    }
}

impl Bdf {
    /// Refuses a device or function that does not fit its field.
    ///
    /// Truncating instead would address a *different* device: device 32 has the
    /// same low five bits as device 0, so a configuration write meant for one
    /// would land on the other and the caller would never learn it had been
    /// redirected. Refusing is the only outcome a caller can act on.
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
    // the subsequent read from `0xCFC` returns whatever was last latched --
    // which is indistinguishable from a device answering all-ones, i.e.
    // "absent". A missing enable bit therefore presents as "no devices at all"
    // rather than as an error.
    (1 << 31)
        | ((bdf.bus as u32) << 16)
        | ((bdf.device as u32) << 11)
        | ((bdf.function as u32) << 8)
        // The low two bits are reserved: the port addresses a dword. The
        // caller's byte offset is rounded down here rather than asserted, so
        // that reading a byte-sized field by its own offset works and lands in
        // the register that contains it.
        | ((offset as u32) & 0xfc)
}

/// Reads one dword of configuration space.
///
/// # Safety
/// Port I/O. The caller must be running with the privilege to issue it, which
/// in this kernel means ring 0.
pub unsafe fn config_read32(bdf: Bdf, offset: u8) -> u32 {
    unsafe {
        crate::port::outl(CONFIG_ADDRESS, config_address(bdf, offset));
        crate::port::inl(CONFIG_DATA)
    }
}

/// Writes one dword of configuration space.
///
/// # Safety
/// Port I/O, and the write reaches a device register: the caller must know what
/// the register does. Writing a BAR while the device is enabled, for instance,
/// moves its window out from under any existing mapping.
pub unsafe fn config_write32(bdf: Bdf, offset: u8, value: u32) {
    unsafe {
        crate::port::outl(CONFIG_ADDRESS, config_address(bdf, offset));
        crate::port::outl(CONFIG_DATA, value);
    }
}

/// Reads the whole legacy configuration window of a device.
///
/// A snapshot, so everything that walks it is a pure function over bytes and
/// can be tested on the host. The alternative -- reading registers as the walk
/// proceeds -- would put the only interesting logic behind port I/O.
///
/// # Safety
/// Port I/O, as [`config_read32`].
pub unsafe fn config_snapshot(bdf: Bdf) -> [u8; 256] {
    let mut cfg = [0u8; 256];
    for dword in 0..64u16 {
        let offset = (dword * 4) as u8;
        // SAFETY: the caller's obligation, forwarded.
        let value = unsafe { config_read32(bdf, offset) };
        cfg[dword as usize * 4..dword as usize * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    cfg
}

/// An absent device answers all-ones on its vendor id.
const NO_VENDOR: u16 = 0xffff;

/// Finds the first device on bus 0 matching a vendor and device id.
///
/// Bus 0 only. A bridge would have to be walked to reach anything else, and q35
/// puts virtio devices on the root bus — so scanning further would be code with
/// no device behind it, which is code that cannot be tested.
///
/// # Safety
/// Port I/O, as [`config_read32`].
pub unsafe fn find_device(vendor: u16, device: u16) -> Option<Bdf> {
    // SAFETY: the caller's obligation, forwarded to each read.
    scan_for(vendor, device, |bdf| unsafe { config_read32(bdf, 0) })
}

/// The scan itself, over a reader.
///
/// Separated from the port I/O for the reason `config_snapshot` gives: the only
/// interesting parts are the iteration order and the multi-function break, and
/// inline port I/O puts both out of reach of a host test. If that break fired
/// one level too eagerly -- on any absent function rather than on function 0 --
/// a device sitting behind an absent slot would be missed, and the symptom is
/// `NotFound` for a device that is right there.
pub fn scan_for(vendor: u16, device: u16, read_id: impl Fn(Bdf) -> u32) -> Option<Bdf> {
    for slot in 0..32u8 {
        for function in 0..8u8 {
            let bdf = Bdf::new(0, slot, function)?;
            let id = read_id(bdf);
            let (got_vendor, got_device) = ((id & 0xffff) as u16, (id >> 16) as u16);
            if got_vendor == NO_VENDOR {
                // Function 0 absent means the whole slot is absent; a
                // multi-function device must implement function 0.
                if function == 0 {
                    break;
                }
                continue;
            }
            if got_vendor == vendor && got_device == device {
                return Some(bdf);
            }
        }
    }
    None
}

/// Why a capability list could not be walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// A capability offset was not dword-aligned. PCI requires it, and
    /// following it would read a header straddling two registers.
    Unaligned(u8),
    /// A capability *body* would extend past the 256-byte window the legacy
    /// ports can reach.
    ///
    /// Not raised by the list walk itself: a header is two bytes and offsets
    /// are dword-aligned, so the largest legal offset is 252 and its header
    /// always fits. It is raised by [`capability_body`], where a caller reads a
    /// structure whose length it knows — virtio's is 16 bytes, which overruns
    /// from offset 252.
    OutOfRange(u8),
    /// The list points back into itself, or is longer than the window can hold.
    Cycle,
}

/// One capability: its id and where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub id: u8,
    pub offset: u8,
}

/// The command register in the standard header.
pub const COMMAND_REGISTER: u8 = 0x04;
/// Lets the device decode memory accesses to its BARs.
pub const COMMAND_MEMORY_SPACE: u32 = 1 << 1;
/// Lets the device issue DMA. Without it a device fetches no descriptors, and
/// the symptom is every request accepted and none completed.
pub const COMMAND_BUS_MASTER: u32 = 1 << 2;

/// MSI-X message control: enables the capability.
pub const MSIX_CONTROL_ENABLE: u16 = 1 << 15;
/// MSI-X message control: masks every vector while set.
pub const MSIX_CONTROL_FUNCTION_MASK: u16 = 1 << 14;

/// Where the capability pointer lives in the standard header.
const CAP_POINTER: usize = 0x34;

/// Walks the capability list in a configuration-space snapshot.
///
/// Takes a snapshot rather than reading ports, so every refusal below is
/// reachable from a host test. The hardware path reads the window first with
/// [`config_snapshot`] and calls this.
pub fn walk_capabilities(cfg: &[u8; 256]) -> Result<alloc::vec::Vec<Capability>, CapError> {
    let mut caps = alloc::vec::Vec::new();
    let mut offset = cfg[CAP_POINTER];
    // The iteration bound *is* the cycle guard, and 64 is exactly right rather
    // than merely generous: the window holds 64 dword-aligned offsets, an
    // acyclic list visits each at most once, and offset 0 terminates -- so any
    // list that reaches a 64th step has necessarily revisited one. A separate
    // "have I seen this offset" scan was written first and removed: it returned
    // the same error for the same inputs, so it was a second guard that could
    // not fail on its own, which reads like defence and is really just code.
    //
    // A device's capability list is device-controlled data, in exactly the
    // sense the ELF loader's input is.
    //
    // Only half of this is falsifiable, and that is stated rather than implied:
    // a test can pin the bound from *below* (the longest legal list must still
    // be walked in full), but not that the walk terminates at all -- a function
    // that never returns never returns to be asserted about, and deleting the
    // bound hangs the suite instead of failing it.
    for _ in 0..64 {
        if offset == 0 {
            return Ok(caps);
        }
        if offset & 0b11 != 0 {
            return Err(CapError::Unaligned(offset));
        }
        // No bounds check on the header itself: `offset` is dword-aligned by
        // the check above and is a `u8`, so the largest it can be is 252 and
        // its two-byte header always fits inside the window. A check here would
        // be unreachable code that reads like a guard -- see `CapError::
        // OutOfRange`, which is raised where a body of known length is read.
        caps.push(Capability { id: cfg[offset as usize], offset });
        offset = cfg[offset as usize + 1];
    }
    Err(CapError::Cycle)
}

/// Borrows a capability's body, refusing one that runs past the window.
///
/// The length comes from the caller, which knows the structure it is reading —
/// a virtio capability is 16 bytes. A device can place a capability at offset
/// 252, where a 16-byte body runs off the end of everything the legacy ports
/// can reach; reading it anyway would return the tail of another device's
/// registers or whatever the snapshot buffer happened to hold.
pub fn capability_body(
    cfg: &[u8; 256],
    cap: Capability,
    len: usize,
) -> Result<&[u8], CapError> {
    let end = cap.offset as usize + len;
    if end > cfg.len() {
        return Err(CapError::OutOfRange(cap.offset));
    }
    Ok(&cfg[cap.offset as usize..end])
}

/// The PCI capability id for MSI-X.
pub const CAP_ID_MSIX: u8 = 0x11;

/// Why an MSI-X table entry could not be programmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsixError {
    /// The entry is past the table the device declared.
    NoSuchEntry(u16),
}

/// The address an MSI writes to, for a message delivered to one processor.
///
/// An MSI is a *memory write*, not a pin: the address selects the local APIC
/// and the destination within it. That is the whole reason this kernel can use
/// MSI-X without an ACPI parser — the alternative, INTx, arrives at an I/O APIC
/// redirection entry, and finding the I/O APIC means the ACPI MADT.
///
/// Bits 19:12 carry the destination APIC id. Getting that shift wrong sends
/// every message to processor 0, which *works* on a one-processor guest and
/// silently stops working the moment the waiting thread is elsewhere.
pub const fn msix_message_address(lapic_base: u64, apic_id: u8) -> u64 {
    lapic_base | ((apic_id as u64) << 12)
}

/// The data word an MSI writes, carrying the vector.
///
/// Delivery mode Fixed (000) and edge-triggered. A level-triggered message
/// needs an end-of-interrupt protocol this kernel does not implement for
/// devices, and the device would stop delivering after the first completion —
/// a hang after exactly one successful request, which is a memorable way to
/// spend an afternoon.
pub const fn msix_message_data(vector: u8) -> u32 {
    vector as u32
}

/// A device's MSI-X table, mapped.
#[derive(Debug, Clone, Copy)]
pub struct MsixTable {
    base: u64,
    entries: u16,
}

impl MsixTable {
    /// Names a mapped MSI-X table.
    ///
    /// `unsafe` on the *constructor*, not only on `program`: the invariant is a
    /// property of the value, and a safely-constructible `MsixTable` can be
    /// built pointing anywhere, copied, stored and passed around with nothing
    /// marking it as dangerous — leaving the one call that can cause harm to
    /// carry an obligation established far away.
    ///
    /// # Safety
    /// `base` must be a mapped, uncacheable MSI-X table with at least `entries`
    /// entries.
    pub const unsafe fn new(base: u64, entries: u16) -> Self {
        Self { base, entries }
    }
}

/// Bytes per MSI-X table entry: address low, address high, data, vector control.
pub const MSIX_ENTRY_BYTES: u64 = 16;

impl MsixTable {
    /// Programs one entry and unmasks it.
    ///
    /// The mask bit is cleared *last*. An entry unmasked while its address is
    /// half-written would deliver a message to whatever the two halves happen
    /// to spell — which, with no IOMMU, is a write to an address the kernel
    /// never chose.
    ///
    /// # Safety
    /// `base` must be a mapped, uncacheable MSI-X table with at least
    /// `entries` entries.
    pub unsafe fn program(&self, entry: u16, address: u64, data: u32) -> Result<(), MsixError> {
        // The entry count comes from the device's own capability. Trusting it
        // past its stated size writes into whatever follows the table in the
        // BAR, which is more device registers.
        if entry >= self.entries {
            return Err(MsixError::NoSuchEntry(entry));
        }
        let at = self.base + entry as u64 * MSIX_ENTRY_BYTES;
        // SAFETY: the caller guarantees the table is mapped and this entry is
        // inside it.
        unsafe {
            // Masked first, so a half-written address is never live.
            core::ptr::write_volatile((at + 12) as *mut u32, 1);
            core::ptr::write_volatile(at as *mut u32, address as u32);
            core::ptr::write_volatile((at + 4) as *mut u32, (address >> 32) as u32);
            core::ptr::write_volatile((at + 8) as *mut u32, data);
            core::ptr::write_volatile((at + 12) as *mut u32, 0);
        }
        Ok(())
    }
}

/// The MSI-X capability's fields, as read from configuration space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsixCapability {
    /// Entries in the table.
    pub entries: u16,
    /// Which BAR holds the table.
    pub bar: u8,
    /// Byte offset of the table within that BAR.
    pub offset: u32,
    /// Where the message-control register lives, for enabling the capability.
    pub control_offset: u8,
}

/// Parses the MSI-X capability from a configuration-space snapshot.
pub fn parse_msix(cfg: &[u8; 256], cap: Capability) -> Option<MsixCapability> {
    // Header, message control, table offset/BIR, PBA offset/BIR.
    let body = capability_body(cfg, cap, 12).ok()?;
    let control = u16::from_le_bytes([body[2], body[3]]);
    // The table size is stored as N-1, so a device with one entry reports zero.
    // Reading it as the count directly gives a table one short, and the last
    // entry -- the only one a single-vector device has -- is then refused.
    let entries = (control & 0x7ff) + 1;
    let table = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    Some(MsixCapability {
        bar: (table & 0b111) as u8,
        offset: table & !0b111,
        entries,
        control_offset: cap.offset + 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_address_targets_one_processor_and_stays_in_the_lapic_window() {
        // An MSI is a memory write to the local APIC's address, with the
        // destination processor in bits 19:12. Getting the shift wrong sends
        // every completion to processor 0 -- which works on a one-processor
        // guest and silently stops working the moment the waiting thread is
        // elsewhere.
        let addr = msix_message_address(0xfee0_0000, 3);
        assert_eq!(addr & 0xfff0_0000, 0xfee0_0000, "the message left the LAPIC window");
        assert_eq!((addr >> 12) & 0xff, 3, "the destination processor is wrong");
        assert_ne!(
            msix_message_address(0xfee0_0000, 0),
            addr,
            "every processor got the same message address"
        );
    }

    #[test]
    fn a_message_data_word_carries_the_vector_and_nothing_else() {
        // Delivery mode must be Fixed (000) and the trigger edge: a
        // level-triggered MSI needs an end-of-interrupt protocol this kernel
        // does not implement for devices, and the device would stop delivering
        // after the first completion.
        let data = msix_message_data(35);
        assert_eq!(data & 0xff, 35, "the vector is not in the low byte");
        assert_eq!((data >> 8) & 0b111, 0, "delivery mode is not Fixed");
        assert_eq!((data >> 15) & 1, 0, "the message is level-triggered");
    }

    #[test]
    fn a_msix_table_size_of_one_is_read_as_one_entry() {
        // The register stores N-1, so a single-vector device reports zero.
        // Reading it as the count gives a table one short, and the only entry
        // such a device has is then refused -- which presents as "this device
        // has no MSI-X" for a device that does.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0x60;
        cfg[0x60] = CAP_ID_MSIX;
        cfg[0x61] = 0;
        cfg[0x62] = 0; // message control: table size - 1 == 0
        cfg[0x63] = 0;
        cfg[0x64] = 0x04; // table in BAR 4, offset 0
        let cap = Capability { id: CAP_ID_MSIX, offset: 0x60 };
        let msix = parse_msix(&cfg, cap).expect("the capability was not parsed");
        assert_eq!(msix.entries, 1, "a one-entry table was read as {}", msix.entries);
        assert_eq!(msix.bar, 4, "the table BAR index is wrong");
        assert_eq!(msix.offset, 0, "the table offset is wrong");

        // The negative direction of a device-controlled parse, which is the
        // direction this project keeps losing: a capability whose 12-byte body
        // runs past the window is refused rather than assembled from whatever
        // the snapshot buffer held.
        assert!(
            parse_msix(&cfg, Capability { id: CAP_ID_MSIX, offset: 0xfc }).is_none(),
            "a capability body past the window was parsed"
        );
    }

    #[test]
    fn the_msix_table_offset_excludes_the_bar_index_bits() {
        // The low three bits of the register are the BAR index, not part of the
        // offset. Leaving them in shifts the whole table by up to seven bytes,
        // so every entry straddles two entries' worth of registers.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0x60;
        cfg[0x60] = CAP_ID_MSIX;
        cfg[0x62] = 3; // four entries
        cfg[0x64..0x68].copy_from_slice(&0x0000_3002u32.to_le_bytes());
        let msix = parse_msix(&cfg, Capability { id: CAP_ID_MSIX, offset: 0x60 })
            .expect("the capability was not parsed");
        assert_eq!(msix.bar, 2, "the BAR index is wrong");
        assert_eq!(msix.offset, 0x3000, "the BAR index bits leaked into the offset");
        assert_eq!(msix.entries, 4);
    }

    #[test]
    fn programming_an_entry_past_the_table_is_refused() {
        // The entry count comes from the device's own capability. Trusting it
        // past its stated size writes into whatever follows the table in the
        // BAR -- which is more device registers.
        let mut backing = [0u32; 8];
        // SAFETY: `backing` is two entries of four dwords, which is exactly
        // what `entries: 2` claims.
        let table = unsafe { MsixTable::new(backing.as_mut_ptr() as u64, 2) };
        // SAFETY: `backing` is two entries of four dwords, which is exactly
        // what `entries: 2` claims.
        unsafe {
            assert_eq!(table.program(2, 0, 0), Err(MsixError::NoSuchEntry(2)));
            assert_eq!(table.program(1, 0xfee0_1000, 35), Ok(()));
        }
        // The second entry's four dwords, and the mask cleared last.
        assert_eq!(backing[4], 0xfee0_1000, "the address low half was not written");
        assert_eq!(backing[5], 0, "the address high half was not written");
        assert_eq!(backing[6], 35, "the data word was not written");
        assert_eq!(backing[7], 0, "the entry was left masked");
    }

    #[test]
    fn a_slot_whose_first_function_is_absent_is_skipped_entirely() {
        // The multi-function break. A device must implement function 0, so an
        // absent one means the whole slot is absent -- but if the break fired
        // on *any* absent function instead, the scan would stop at the first
        // gap and miss every device after it.
        use core::cell::RefCell;
        let probed = RefCell::new(alloc::vec::Vec::new());
        let found = scan_for(0x1af4, 0x1042, |bdf| {
            probed.borrow_mut().push((bdf.device(), bdf.function()));
            // Slot 0 absent entirely; the device sits at slot 5, function 0.
            if bdf.device() == 5 && bdf.function() == 0 { 0x1042_1af4 } else { 0xffff_ffff }
        });
        assert_eq!(found, Bdf::new(0, 5, 0), "a device behind an absent slot was missed");
        let probed = probed.borrow();
        // Slot 0's absent function 0 must have ended that slot, not the scan.
        assert!(
            !probed.iter().any(|&(d, f)| d == 0 && f > 0),
            "an absent function 0 did not end its slot: {probed:?}"
        );
        assert!(probed.iter().any(|&(d, f)| d == 5 && f == 0), "slot 5 was never probed");
    }

    #[test]
    fn a_device_on_a_later_function_of_a_present_slot_is_found() {
        // The other half: function 0 present means the remaining functions are
        // worth probing, so a device at function 3 must be reached.
        let found = scan_for(0x1af4, 0x1042, |bdf| match (bdf.device(), bdf.function()) {
            (2, 0) => 0x0001_1af4,
            (2, 3) => 0x1042_1af4,
            _ => 0xffff_ffff,
        });
        assert_eq!(found, Bdf::new(0, 2, 3), "a device on a later function was missed");
    }

    #[test]
    fn an_empty_bus_yields_nothing() {
        assert_eq!(scan_for(0x1af4, 0x1042, |_| 0xffff_ffff), None);
    }

    #[test]
    fn a_config_address_sets_the_enable_bit_and_aligns_the_offset() {
        // Bit 31 is the enable bit; without it the write to 0xCF8 selects
        // nothing and the read from 0xCFC returns whatever was last latched --
        // which looks exactly like a device that answered all-ones, so the
        // whole bus would appear empty rather than the access appear broken.
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
        // masked by another field happening to be zero -- which is exactly what
        // a single combined assertion would allow.
        assert_eq!(config_address(Bdf::new(0xff, 0, 0).unwrap(), 0) >> 16 & 0xff, 0xff);
        assert_eq!(config_address(Bdf::new(0, 0x1f, 0).unwrap(), 0) >> 11 & 0x1f, 0x1f);
        assert_eq!(config_address(Bdf::new(0, 0, 7).unwrap(), 0) >> 8 & 0x7, 7);
        assert_eq!(config_address(Bdf::new(0, 0, 0).unwrap(), 0xfc) & 0xfc, 0xfc);
    }

    #[test]
    fn an_unaligned_offset_is_rounded_down_to_its_register() {
        // Asserted with an offset that is *not* already aligned, which the
        // other tests all were -- so the mask could be widened to `0xff` with
        // every one of them still green. A byte-sized field is read by naming
        // its own offset, and the register containing it is the one four bytes
        // below; leaving the low bits set selects a different register on real
        // hardware and is quietly ignored on some, which is worse.
        let bdf = Bdf::new(0, 0, 0).unwrap();
        assert_eq!(
            config_address(bdf, 0x11) & 0xff,
            0x10,
            "an unaligned offset was passed through instead of rounded to its register"
        );
        assert_eq!(config_address(bdf, 0x13) & 0xff, 0x10);
        // And the four bytes of one register all name it, which is what makes
        // "read the dword, index the byte" correct at the call site.
        for byte in 0..4u8 {
            assert_eq!(config_address(bdf, 0x10 + byte) & 0xff, 0x10);
        }
    }

    #[test]
    fn an_out_of_range_device_or_function_is_refused() {
        // The negative direction. A device number above 31 or a function above
        // 7 does not fit its field, and truncating silently addresses a
        // *different* device -- a configuration write then lands on a device
        // the caller never named.
        assert!(Bdf::new(0, 32, 0).is_none(), "device 32 does not fit five bits");
        assert!(Bdf::new(0, 0, 8).is_none(), "function 8 does not fit three bits");
        assert!(Bdf::new(255, 31, 7).is_some(), "the largest legal address was refused");
    }

    #[test]
    fn a_capability_pointer_that_loops_is_refused_rather_than_walked_forever() {
        // Capability lists are a linked list in device-controlled memory, and a
        // device that points a capability at itself is a hang in the kernel's
        // enumeration path. Bounded and refused, because "the device is
        // malformed" is something the caller can act on and a hang is not.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0x40;
        cfg[0x40] = 0x09; // vendor-specific
        cfg[0x41] = 0x40; // next -> itself
        assert_eq!(walk_capabilities(&cfg), Err(CapError::Cycle));
    }

    #[test]
    fn a_capability_at_the_very_end_of_the_window_is_still_walked() {
        // The legacy ports reach only the first 256 bytes, and 252 is the last
        // dword-aligned offset. Refusing it would be indistinguishable from a
        // device with fewer capabilities -- so the device would look like it
        // lacked the virtio capability it actually has, and probing would
        // report "not a modern device" for a device that is one.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0xfc;
        cfg[0xfc] = 0x09;
        cfg[0xfd] = 0x00;
        assert_eq!(
            walk_capabilities(&cfg),
            Ok(alloc::vec![Capability { id: 0x09, offset: 0xfc }]),
            "the last readable capability was refused"
        );
    }

    #[test]
    fn a_capability_body_running_past_the_window_is_refused() {
        // Where `OutOfRange` actually lives. The *header* of an aligned
        // capability always fits -- 252 plus two bytes is inside the window --
        // so the walk cannot raise it, and a check there would be unreachable
        // code that reads like a guard. A *body* is a different matter: a
        // virtio capability is 16 bytes, and one placed at 252 runs off the end
        // of everything the legacy ports can read.
        let cfg = [0u8; 256];
        let last = Capability { id: 0x09, offset: 0xfc };
        assert_eq!(capability_body(&cfg, last, 16), Err(CapError::OutOfRange(0xfc)));
        // And the largest body that does fit is accepted, so the bound cannot
        // drift downward and refuse a legal capability without anything
        // noticing.
        assert!(capability_body(&cfg, last, 4).is_ok(), "a body ending at the window was refused");
        assert!(
            capability_body(&cfg, Capability { id: 0x09, offset: 0xf0 }, 16).is_ok(),
            "a 16-byte body ending exactly at the window was refused"
        );
    }

    #[test]
    fn a_misaligned_capability_pointer_is_refused() {
        // PCI requires capability structures to be dword-aligned. A device
        // reporting otherwise is malformed, and following it reads a header
        // straddling two registers -- which yields a plausible id and a
        // plausible next-pointer, both wrong.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0x41;
        assert_eq!(walk_capabilities(&cfg), Err(CapError::Unaligned(0x41)));
    }

    #[test]
    fn an_empty_capability_pointer_yields_no_capabilities() {
        // Zero terminates the list and must not be followed. A walk that
        // treated 0 as an offset would read the vendor id as a capability id
        // and the device id as the next pointer.
        let cfg = [0u8; 256];
        assert_eq!(walk_capabilities(&cfg), Ok(alloc::vec![]));
    }

    #[test]
    fn the_longest_list_the_window_can_hold_is_walked_in_full() {
        // Pins the iteration bound from below, which is the half of it a test
        // can reach. That the walk *terminates* is not assertable -- a function
        // that never returns never returns to be asserted about, and removing
        // the bound hangs this suite rather than failing it. What is assertable
        // is that the bound is not too small: lowered, it would truncate a
        // legitimate list and report `Cycle` for a device that has none, which
        // is the same "looks like fewer capabilities" failure the window bound
        // exists to avoid.
        //
        // The longest acyclic list is one capability per dword-aligned offset
        // from 0x40 to 0xFC: 48 of them, each pointing at the next.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0x40;
        let offsets: alloc::vec::Vec<u8> = (0x40..=0xfcu8).step_by(4).collect();
        for (i, &off) in offsets.iter().enumerate() {
            cfg[off as usize] = 0x09;
            cfg[off as usize + 1] = offsets.get(i + 1).copied().unwrap_or(0);
        }
        let walked = walk_capabilities(&cfg).expect("the longest legal list was refused");
        assert_eq!(
            walked.len(),
            offsets.len(),
            "the walk stopped early; a device with a full capability list would look like one \
             with fewer"
        );
        assert_eq!(walked.last().unwrap().offset, 0xfc, "the last capability was dropped");
    }

    #[test]
    fn a_well_formed_list_is_walked_in_order() {
        // The positive direction, so a `walk_capabilities` that refused
        // everything would not pass the six tests above by accident.
        let mut cfg = [0u8; 256];
        cfg[CAP_POINTER] = 0x40;
        cfg[0x40] = 0x11; // MSI-X
        cfg[0x41] = 0x50;
        cfg[0x50] = 0x09; // vendor-specific
        cfg[0x51] = 0x00;
        assert_eq!(
            walk_capabilities(&cfg),
            Ok(alloc::vec![
                Capability { id: 0x11, offset: 0x40 },
                Capability { id: 0x09, offset: 0x50 },
            ])
        );
    }
}
