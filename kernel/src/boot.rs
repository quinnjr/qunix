use limine::BaseRevision;
use limine::request::{HhdmRespData, MemmapRespData, Request};

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
pub static HHDM: Request<HhdmRespData> = Request::<HhdmRespData>::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MEMMAP: Request<MemmapRespData> = Request::<MemmapRespData>::new();

pub fn base_revision_supported() -> bool {
    BASE_REVISION.is_supported()
}
