use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
const IST_STACK_SIZE: usize = 4096 * 5;

static mut DOUBLE_FAULT_STACK: [u8; IST_STACK_SIZE] = [0; IST_STACK_SIZE];
static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();
static mut SELECTORS: Option<Selectors> = None;

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

/// Installs the GDT and TSS on the current CPU.
///
/// Idempotent, so tests may call it in any order. Uses `static mut` because
/// this runs before any allocator exists and, in M0, only ever from one CPU.
/// M1 replaces this with per-CPU storage.
pub fn init() {
    unsafe {
        if (*(&raw const SELECTORS)).is_some() {
            return;
        }

        let stack_start = VirtAddr::from_ptr(&raw const DOUBLE_FAULT_STACK);
        let tss = &mut *(&raw mut TSS);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_start + IST_STACK_SIZE as u64;

        let gdt = &mut *(&raw mut GDT);
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(&*(&raw const TSS)));

        gdt.load();
        CS::set_reg(code);
        DS::set_reg(data);
        ES::set_reg(data);
        SS::set_reg(data);
        load_tss(tss_sel);

        SELECTORS = Some(Selectors { code, data, tss: tss_sel });
    }
}

fn selectors() -> &'static Selectors {
    unsafe { (*(&raw const SELECTORS)).as_ref().expect("gdt not initialised") }
}

pub fn kernel_code_selector() -> SegmentSelector {
    selectors().code
}

pub fn kernel_data_selector() -> SegmentSelector {
    selectors().data
}

pub fn tss_selector() -> SegmentSelector {
    selectors().tss
}
