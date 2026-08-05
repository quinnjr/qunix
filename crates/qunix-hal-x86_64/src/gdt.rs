use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Selectors for one CPU's table.
///
/// Not constants. The indices depend on the order descriptors are appended, and
/// `x86_64`'s builder assigns them, so hard-coding them would encode an
/// assumption the builder is free to break. M1's plan assumed `KERNEL_CODE` and
/// `USER_DATA` consts; see Execution Deviation D1.
#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub user_data: SegmentSelector,
    pub tss: SegmentSelector,
}

/// Builds this CPU's GDT into `gdt`, loads it, and loads `tss`.
///
/// Every CPU gets its own table and its own TSS: the TSS holds `rsp0` and the
/// IST pointers, both of which are per-CPU by definition, so a shared one would
/// have two CPUs faulting onto the same stack.
///
/// # Safety
/// `gdt` and `tss` must live for as long as this CPU runs — the CPU keeps
/// reading them via GDTR and TR long after this returns. They must not be moved
/// or dropped, which is why the caller stores them in the per-CPU block rather
/// than on a stack.
///
/// `ist_top` must be the top (highest address) of a stack reserved for this
/// CPU's double-fault handler.
pub unsafe fn build_and_load(
    gdt: &mut GlobalDescriptorTable,
    tss: &TaskStateSegment,
    ist_top: u64,
) -> Selectors {
    // The TSS is filled in through a raw pointer rather than `&mut` because the
    // descriptor below borrows it immutably for `'static`, and the two borrows
    // would otherwise overlap. Sound: this is the only writer, and it runs
    // before the descriptor is built.
    let tss_ptr = (tss as *const TaskStateSegment).cast_mut();
    unsafe {
        (*tss_ptr).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            VirtAddr::new(ist_top);
    }

    // Rebuilt rather than reused because `ltr` sets the busy bit in the TSS
    // descriptor, and `ltr` against an already-busy descriptor raises #GP, so
    // every call must present a fresh, available one.
    *gdt = GlobalDescriptorTable::new();
    let kernel_code = gdt.append(Descriptor::kernel_code_segment());
    let kernel_data = gdt.append(Descriptor::kernel_data_segment());
    // Order is dictated by `SYSRET`, not by taste. It loads CS from
    // `IA32_STAR[63:48] + 16` and SS from `+ 8`, so the user *data* descriptor
    // must sit immediately before the user *code* one. Appending them the other
    // way round compiles, boots, and then returns to ring 3 with a data
    // selector in CS -- a #GP on the first user instruction.
    let user_data = gdt.append(Descriptor::user_data_segment());
    let user_code = gdt.append(Descriptor::user_code_segment());
    // SAFETY: the caller guarantees `tss` outlives this CPU, which is what the
    // `'static` bound on `tss_segment` is really asking for.
    let tss_static: &'static TaskStateSegment = unsafe { &*(tss as *const TaskStateSegment) };
    let tss_sel = gdt.append(Descriptor::tss_segment(tss_static));

    // SAFETY: same lifetime argument -- the table lives in the per-CPU block.
    let gdt_static: &'static GlobalDescriptorTable =
        unsafe { &*(gdt as *const GlobalDescriptorTable) };
    gdt_static.load();
    unsafe {
        CS::set_reg(kernel_code);
        DS::set_reg(kernel_data);
        ES::set_reg(kernel_data);
        SS::set_reg(kernel_data);
        load_tss(tss_sel);
    }

    Selectors { kernel_code, kernel_data, user_code, user_data, tss: tss_sel }
}

/// Whether this CPU's double-fault stack canary survives: `Some(false)` means
/// it overflowed, `None` that this CPU has no per-CPU block yet.
pub fn ist_canary_intact() -> Option<bool> {
    if !crate::percpu::is_installed() {
        return None;
    }
    crate::percpu::current().ist_canary_intact()
}

pub fn kernel_code_selector() -> SegmentSelector {
    crate::percpu::current().selectors().kernel_code
}

pub fn kernel_data_selector() -> SegmentSelector {
    crate::percpu::current().selectors().kernel_data
}

pub fn tss_selector() -> SegmentSelector {
    crate::percpu::current().selectors().tss
}

pub fn user_code_selector() -> SegmentSelector {
    crate::percpu::current().selectors().user_code
}

pub fn user_data_selector() -> SegmentSelector {
    crate::percpu::current().selectors().user_data
}
