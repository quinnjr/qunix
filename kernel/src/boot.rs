use limine::BaseRevision;
use limine::memmap::MEMMAP_USABLE;
use limine::request::{HhdmRequest, MemmapRequest};

// NOTE: there is no linker script (see the plan's Execution Deviations), so
// these land in an orphan `.requests` section rather than a dedicated PHDR,
// and the start/end markers are omitted. Limine treats the markers as a scan
// optimisation, so without them it simply scans the whole loaded image for
// request magic.
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MEMMAP: MemmapRequest = MemmapRequest::new();

pub fn base_revision_supported() -> bool {
    BASE_REVISION.is_supported()
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub start: u64,
    pub len: u64,
    pub usable: bool,
}

/// Offset of the higher-half direct map installed by Limine.
pub fn hhdm_offset() -> u64 {
    HHDM.response().expect("limine provided no HHDM response").offset
}

/// Every region the bootloader reported, usable or not.
pub fn all_regions() -> impl Iterator<Item = MemoryRegion> {
    let response = MEMMAP.response().expect("limine provided no memory map");
    response.entries().iter().map(|entry| MemoryRegion {
        start: entry.base,
        len: entry.length,
        usable: entry.type_ == MEMMAP_USABLE,
    })
}

pub fn usable_regions() -> impl Iterator<Item = MemoryRegion> {
    all_regions().filter(|r| r.usable)
}
