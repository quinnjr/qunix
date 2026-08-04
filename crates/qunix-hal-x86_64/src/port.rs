use core::arch::asm;

/// # Safety
/// The caller must ensure `port` is an I/O port it is entitled to drive on this
/// machine, and that writing `value` cannot violate an invariant of whatever
/// owns that device.
///
/// Note `options(nomem)`: this asserts to the compiler that the instruction
/// touches no memory, so it may be reordered across surrounding loads and
/// stores. That matches the `x86_64` crate's own port implementations and is
/// correct for the register-poking this kernel does today, but a DMA-capable
/// device whose buffer must be visible before the port write would need an
/// explicit fence.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value,
             options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// The caller must ensure `port` is an I/O port it is entitled to read on this
/// machine; reads can have side effects on the device. See [`outb`] for the
/// `options(nomem)` reordering caveat.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port,
             options(nomem, nostack, preserves_flags))
    };
    value
}

/// # Safety
/// As [`outb`], for a 32-bit write.
pub unsafe fn outl(port: u16, value: u32) {
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value,
             options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// As [`inb`], for a 32-bit read.
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        asm!("in eax, dx", out("eax") value, in("dx") port,
             options(nomem, nostack, preserves_flags))
    };
    value
}
