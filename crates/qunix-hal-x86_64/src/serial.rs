use crate::port::{inb, outb};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};
use qunix_sync::IrqSpinLock;

const COM1: u16 = 0x3F8;

/// Spin budget per FIFO refill before giving up on a wedged UART.
const TX_SPIN_BUDGET: u32 = 100_000;

/// Depth of the transmit FIFO on a genuine 16550A. When the line-status THRE
/// bit is set with the FIFO enabled (`init` writes FCR = 0xC7), the whole
/// holding FIFO is empty, so this many bytes may be written before polling
/// again — halving the port accesses per byte, each of which is a VM exit under
/// virtualisation. This only applies once `init` has confirmed the FIFO is
/// real; see the `Uart::fifo_depth` field, which is what write paths use.
const TX_FIFO_DEPTH_16550A: u32 = 16;

pub struct Uart {
    base: u16,
    /// Bytes believed to be free in the transmit FIFO. Zero forces a poll.
    fifo_free: u32,
    /// Confirmed FIFO depth: 16 on a real 16550A, 1 otherwise.
    fifo_depth: u32,
    /// Set once the port has burned a full spin budget without reporting ready.
    /// Sticky, because the budget is per refill: without it a 2 KiB panic dump
    /// against a dead UART spends 2000 budgets — minutes of apparent hang with
    /// interrupts masked — instead of one. Cleared only by `init`.
    wedged: bool,
    /// Bytes discarded because the port was wedged. Saturates rather than wraps
    /// so a reader cannot mistake a flood for a clean run.
    dropped: u32,
}

impl Uart {
    const fn new(base: u16) -> Self {
        Self { base, fifo_free: 0, fifo_depth: 1, wedged: false, dropped: 0 }
    }

    /// Bytes dropped because the UART stopped accepting output.
    pub fn dropped_bytes(&self) -> u32 {
        self.dropped
    }

    /// # Safety
    /// `self.base` must be the I/O port base of a real 16550-compatible UART
    /// that nothing else is concurrently driving. Re-initialising is harmless:
    /// it reprograms the divisor and clears the FIFO.
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
        // A 16450 (or a broken 16550) ignores the FCR write and has a 1-byte
        // holding register. Trusting 16 there would drop 15 of every 16 bytes,
        // so read IIR back and believe only what the hardware confirms.
        self.fifo_depth = if unsafe { inb(self.base + 2) } & 0xC0 == 0xC0 {
            TX_FIFO_DEPTH_16550A
        } else {
            1
        };
        self.fifo_free = 0;
        // Re-init is the only way back from a wedge: the FCR write above has
        // just reset the FIFO, so the port deserves a fresh chance.
        self.wedged = false;
    }

    /// Waits for room in the transmit FIFO and records how much is free.
    ///
    /// Returns false if the UART never reported ready, in which case it is
    /// marked wedged and the caller must drop the byte: a wedged UART must not
    /// hang the kernel, least of all while the console lock is held.
    fn await_fifo(&mut self) -> bool {
        // Bit 5 of the line-status register means "transmit holding empty".
        let mut budget = TX_SPIN_BUDGET;
        while unsafe { inb(self.base + 5) } & 0x20 == 0 {
            if budget == 0 {
                self.wedged = true;
                return false;
            }
            budget -= 1;
            core::hint::spin_loop();
        }
        self.fifo_free = self.fifo_depth;
        true
    }

    fn write_byte(&mut self, byte: u8) {
        if self.wedged {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        if self.fifo_free == 0 && !self.await_fifo() {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.fifo_free -= 1;
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

/// The console is reachable from interrupt and exception context — `idt.rs`
/// prints from its handlers, and the panic path prints from anywhere. A plain
/// `SpinLock` held with interrupts enabled would let a fault taken mid-print
/// re-enter `_print` on the same CPU and spin forever on a lock it already
/// holds, producing a silent hang with no diagnostic. `IrqSpinLock` masks
/// interrupts for the guard's lifetime, which closes that window.
pub static CONSOLE: IrqSpinLock<Uart, crate::Irq> = IrqSpinLock::new(Uart::new(COM1));

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Brings up COM1. Idempotent — re-initialising a 16550 just reprograms the
/// divisor and clears the FIFO, but the guard keeps the contract honest.
pub fn init() {
    if INITIALISED.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe { CONSOLE.lock().init() };
}

/// Releases the console lock unconditionally.
///
/// # Safety
/// Only for the panic path, and only once the caller has established that no
/// context holding the lock will resume. See [`qunix_sync::SpinLock::force_unlock`].
pub unsafe fn force_unlock() {
    // The interrupted frame may have had unspent FIFO credit, and the panic
    // path is about to write through a different handle. Leaving that credit
    // stale would let the first bytes of the panic message -- the ones that
    // matter most -- be written into a FIFO that is not actually empty, and
    // silently dropped.
    unsafe {
        CONSOLE.force_unlock();
        (*CONSOLE.data_ptr()).fifo_free = 0;
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    match CONSOLE.try_lock() {
        Some(mut uart) => {
            let _ = uart.write_fmt(args);
        }
        None => {
            // Reentered while the lock is held. `IrqSpinLock` masks interrupts,
            // so this cannot be a hardware IRQ — it is an exception, which is
            // not maskable, raised inside a print. The interrupted frame cannot
            // resume until this handler returns, so blocking here would
            // deadlock the CPU with no output at all. A 16550 is stateless
            // beyond its port number, so emit through a fresh handle and accept
            // interleaved bytes: garbled diagnostics beat a silent hang.
            let mut uart = Uart::new(COM1);
            let _ = uart.write_fmt(args);
        }
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

// NOTE: a `($fmt:literal)` fast path using `concat!($fmt, "\n")` was tried and
// reverted. It avoids the nested `format_args!` indirection, but `concat!`
// produces a macro call rather than a string literal, and `format_args!` only
// supports implicit captures (`{name}`) on a true literal — so every
// `println!("{x:#x}")` in the tree stopped compiling. Not worth the trade.

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
