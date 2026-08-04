#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(crate::testing::runner)]
#![reexport_test_harness_main = "test_main"]

mod boot;
mod testing;

use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    qunix_hal_x86_64::serial::init();
    assert!(boot::base_revision_supported(), "limine base revision unsupported");
    println!("qunix: booted");

    qunix_hal_x86_64::gdt::init();
    println!("qunix: gdt installed");

    qunix_hal_x86_64::idt::init();
    println!("qunix: idt installed");

    let usable: u64 = boot::usable_regions().map(|r| r.len).sum();
    println!(
        "qunix: hhdm at {:#x}, {} MiB usable",
        boot::hhdm_offset(),
        usable / (1024 * 1024)
    );

    #[cfg(test)]
    test_main();

    halt_forever();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("FAILED\nqunix: PANIC: {info}");
    testing::exit_qemu(testing::ExitCode::Failure);
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

#[cfg(test)]
mod tests {
    #[test_case]
    fn harness_runs_at_all() {
        assert_eq!(1 + 1, 2);
    }

    #[test_case]
    fn gdt_installs_expected_kernel_code_selector() {
        use x86_64::instructions::segmentation::{CS, Segment};
        qunix_hal_x86_64::gdt::init();
        // Entry 0 is the null descriptor, so kernel code lands at index 1 => 0x08.
        assert_eq!(CS::get_reg().0, 0x08);
    }

    #[test_case]
    fn breakpoint_exception_returns_to_caller() {
        qunix_hal_x86_64::gdt::init();
        qunix_hal_x86_64::idt::init();
        x86_64::instructions::interrupts::int3();
        // Reaching this line at all is the assertion: a broken IDT would
        // triple-fault instead of returning here.
        assert!(true);
    }

    #[test_case]
    fn hhdm_offset_is_in_the_higher_half() {
        let offset = crate::boot::hhdm_offset();
        assert!(offset >= 0xffff_8000_0000_0000, "hhdm offset {offset:#x} is not higher-half");
    }

    #[test_case]
    fn memory_map_reports_usable_memory() {
        let mut regions = 0usize;
        let mut total = 0u64;
        for region in crate::boot::usable_regions() {
            regions += 1;
            total += region.len;
            assert!(region.usable);
            assert!(region.len > 0);
        }
        assert!(regions > 0, "no usable memory regions reported");
        // QEMU is launched with 512 MiB; expect at least 256 MiB usable.
        assert!(total >= 256 * 1024 * 1024, "only {total} bytes usable");
    }
}
