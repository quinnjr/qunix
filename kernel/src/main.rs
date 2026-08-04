#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(crate::testing::runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

mod boot;
mod frames;
mod heap;
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

    frames::init();
    println!(
        "qunix: {} MiB of frames available",
        frames::free_bytes() / (1024 * 1024)
    );

    heap::init();
    println!("qunix: kernel heap online");

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

    #[test_case]
    fn frame_allocator_hands_out_usable_physical_memory() {
        crate::frames::init();
        let before = crate::frames::free_bytes();
        assert!(before > 64 * 1024 * 1024, "only {before} bytes of frames");

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        assert_eq!(pa % 4096, 0);

        // The frame must be readable and writable through the HHDM.
        let va = (crate::boot::hhdm_offset() + pa) as *mut u64;
        unsafe {
            va.write_volatile(0xdead_beef_cafe_f00d);
            assert_eq!(va.read_volatile(), 0xdead_beef_cafe_f00d);
        }

        unsafe { crate::frames::free(pa, 0) };
        assert_eq!(crate::frames::free_bytes(), before);
    }

    #[test_case]
    fn mapping_a_fresh_frame_makes_it_readable_and_writable() {
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        // A scratch virtual address in an unused part of the higher half.
        const TEST_VA: u64 = 0xffff_9000_0000_0000;

        assert!(space.translate(TEST_VA).is_none(), "test address already mapped");

        unsafe {
            space
                .map(TEST_VA, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut || {
                    crate::frames::alloc(0)
                })
                .expect("map failed");
        }

        assert_eq!(space.translate(TEST_VA), Some(pa));

        let ptr = TEST_VA as *mut u64;
        unsafe {
            ptr.write_volatile(0x1234_5678_9abc_def0);
            assert_eq!(ptr.read_volatile(), 0x1234_5678_9abc_def0);
        }

        let unmapped = unsafe { space.unmap(TEST_VA).expect("unmap failed") };
        assert_eq!(unmapped, pa);
        assert!(space.translate(TEST_VA).is_none());

        unsafe { crate::frames::free(pa, 0) };
    }

    #[test_case]
    fn kernel_heap_supports_box_and_vec() {
        use alloc::boxed::Box;
        use alloc::vec::Vec;

        crate::frames::init();
        crate::heap::init();

        let boxed = Box::new(0xfeedu32);
        assert_eq!(*boxed, 0xfeed);

        let mut v: Vec<u64> = Vec::new();
        for i in 0..2048 {
            v.push(i);
        }
        assert_eq!(v.len(), 2048);
        assert_eq!(v[2047], 2047);
        assert_eq!(v.iter().sum::<u64>(), (0..2048u64).sum::<u64>());
    }
}
