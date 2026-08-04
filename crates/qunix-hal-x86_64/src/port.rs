use core::arch::asm;

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value,
             options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port,
             options(nomem, nostack, preserves_flags))
    };
    value
}

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn outl(port: u16, value: u32) {
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value,
             options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        asm!("in eax, dx", out("eax") value, in("dx") port,
             options(nomem, nostack, preserves_flags))
    };
    value
}
