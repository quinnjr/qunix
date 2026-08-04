use crate::port::{inb, outb};
use core::fmt::{self, Write};
use qunix_sync::SpinLock;

const COM1: u16 = 0x3F8;

pub struct Uart {
    base: u16,
}

impl Uart {
    const fn new(base: u16) -> Self {
        Self { base }
    }

    /// # Safety
    /// Must only be called once per physical UART.
    unsafe fn init(&mut self) {
        unsafe {
            outb(self.base + 1, 0x00); // disable interrupts
            outb(self.base + 3, 0x80); // enable DLAB
            outb(self.base, 0x03); // divisor lo: 38400 baud
            outb(self.base + 1, 0x00); // divisor hi
            outb(self.base + 3, 0x03); // 8N1, DLAB off
            outb(self.base + 2, 0xC7); // enable + clear FIFO, 14-byte threshold
            outb(self.base + 4, 0x0B); // DTR, RTS, OUT2
        }
    }

    fn write_byte(&mut self, byte: u8) {
        // Bit 5 of the line-status register means "transmit holding empty".
        while unsafe { inb(self.base + 5) } & 0x20 == 0 {
            core::hint::spin_loop();
        }
        unsafe { outb(self.base, byte) };
    }
}

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

pub static CONSOLE: SpinLock<Uart> = SpinLock::new(Uart::new(COM1));

pub fn init() {
    unsafe { CONSOLE.lock().init() };
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    let _ = CONSOLE.lock().write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
