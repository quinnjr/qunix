use limine::memmap::MEMMAP_USABLE;
use limine::request::{HhdmRequest, MemmapRequest};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

// The bootloader locates the requests by finding these two markers and reading
// what lies between them. `kernel/linker.ld` is what puts them in that order --
// as orphan sections they were placed by name, which sorted the end marker
// *before* the start marker and left `.requests` outside the pair. Limine then
// fell back to scanning the whole image, which worked until the image grew and
// then failed as `base revision unsupported` in release only.
//
// There is deliberately no test asserting the ordering. Once the markers exist
// Limine honours them strictly and stops scanning, so *any* misplacement -- end
// before start, or the requests outside the pair -- fails `kmain`'s base
// revision assertion on the first boot. Both were tried. A test could only
// assert something already true of every image that boots at all.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

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

/// Modules the bootloader loaded alongside the kernel.
///
/// In the same `.requests` section as the other requests.
#[used]
#[unsafe(link_section = ".requests")]
static MODULES: limine::request::ModulesRequest = limine::request::ModulesRequest::new();

/// The module whose cmdline is `name`, if the bootloader loaded one.
///
/// Selected by cmdline rather than by index: `limine.conf` may gain another
/// module at any time, and positional lookup would silently start returning a
/// different file rather than failing.
pub fn module(name: &str) -> Option<&'static [u8]> {
    let response = MODULES.response()?;
    response
        .modules()
        .iter()
        .find(|file| file.cmdline() == name)
        .map(|file| file.data())
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

