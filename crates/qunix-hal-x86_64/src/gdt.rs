use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
/// The double-fault handler's peak depth is one `println!` -- the 32 backtrace
/// lines are sequential, not nested -- so a few KiB suffices in principle.
/// 16 KiB keeps a ~5x margin anyway, because this stack has no guard page and
/// overflow grows *down* into whatever `.bss` precedes it, silently corrupting
/// the very structures fault handling depends on. Still 4 KiB cheaper than the
/// original 20 KiB, and it becomes per-CPU in M1.
const IST_STACK_SIZE: usize = 4096 * 4;
/// Written at the low end of the stack and checked on entry, so an overflow is
/// reported rather than silently scribbling.
const IST_CANARY: u64 = 0x5153_5441_434B_5F30;

/// The CPU aligns RSP to 16 bytes on an IST stack switch, so an align-1 array
/// merely wastes the slack. Aligning explicitly also keeps the stack from
/// sharing a cache line with the statics declared after it.
#[repr(align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

static mut DOUBLE_FAULT_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);
static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();
static mut SELECTORS: Option<Selectors> = None;

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

/// Installs the GDT and TSS on the current CPU. Idempotent per CPU: the `lgdt`,
/// the segment-register reloads and the `ltr` all run on every call, so every
/// CPU that calls this ends up actually using the table, not just the first
/// one. [`crate::idt::init`] depends on that — its double-fault gate names an
/// IST index, which is only meaningful on a CPU that has the TSS loaded, so a
/// CPU reaching `idt::init` without this having run there would triple-fault on
/// the first #DF.
///
/// Only the shared IST stack's one-time setup is skipped on repeat calls.
///
/// Uses `static mut` because this runs before any allocator exists.
pub fn init() {
    unsafe {
        // The IST stack is shared and is set up once. Rewriting the canary on a
        // later call would erase the evidence of an overflow an earlier caller
        // could still report; M1's per-CPU storage gives each CPU its own stack
        // and TSS, at which point this moves with them.
        if (*(&raw const SELECTORS)).is_none() {
            let stack_start = VirtAddr::from_ptr(&raw const DOUBLE_FAULT_STACK.0);
            let tss = &mut *(&raw mut TSS);
            // The canary sits at the lowest address, which is where a
            // descending overflow reaches first.
            (&raw mut DOUBLE_FAULT_STACK.0).cast::<u64>().write(IST_CANARY);
            tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
                stack_start + IST_STACK_SIZE as u64;
        }

        // The descriptors are rebuilt rather than reused because `ltr` sets the
        // busy bit in the TSS descriptor, and `ltr` against an already-busy
        // descriptor raises #GP -- so every call must present a fresh,
        // available one. The code and data entries are rewritten with the
        // byte-identical values they already held, and the TSS entry is not
        // consulted by anything but `ltr` (the CPU stack-switches from the
        // cached TR, not from the table), so the table the CPU is reading is
        // never observably inconsistent and this needs no interrupt masking.
        let gdt = &mut *(&raw mut GDT);
        *gdt = GlobalDescriptorTable::new();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(&*(&raw const TSS)));

        (*(&raw const GDT)).load();
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

/// Whether the double-fault stack's low-end canary survives: `Some(false)`
/// means it overflowed.
///
/// `None` before [`init`] has run, because the canary is written by `init` and
/// the stack is zeroed until then — a bare `false` there would report a fault
/// taken during early boot as a stack overflow it cannot have been.
pub fn ist_canary_intact() -> Option<bool> {
    unsafe {
        if (*(&raw const SELECTORS)).is_none() {
            return None;
        }
        Some((&raw const DOUBLE_FAULT_STACK.0).cast::<u64>().read() == IST_CANARY)
    }
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
