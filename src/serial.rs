//! # Serial Port Driver
//!
//! Provides output to the host machine via the UART 16550 serial port.
//! In QEMU, serial output is redirected to the terminal via `-serial stdio`.
//!
//! ## Why Serial?
//! Before we have a framebuffer/graphics driver, the serial port is the
//! simplest and most reliable way to get text output from the kernel.
//! It works identically across QEMU, real hardware, and different architectures.
//!
//! ## Usage
//! Use the `serial_print!` and `serial_println!` macros anywhere in the kernel:
//! ```rust
//! serial_println!("Hello from the kernel!");
//! serial_println!("Value: {}", 42);
//! ```

use uart_16550::SerialPort;
use spin::Mutex;
use lazy_static::lazy_static;

/// The standard I/O port address for COM1 (first serial port).
const COM1_PORT: u16 = 0x3F8;

lazy_static! {
    /// Global serial port instance, protected by a spinlock.
    ///
    /// We use a spinlock (not a regular mutex) because:
    /// 1. We have no OS scheduler to block/wake threads.
    /// 2. Spinlocks are safe in interrupt handlers (critical for later phases).
    pub static ref SERIAL1: Mutex<SerialPort> = {
        // SAFETY: Port 0x3F8 is the standard COM1 address.
        // We only create one instance, so there's no aliasing.
        let mut serial_port = unsafe { SerialPort::new(COM1_PORT) };
        serial_port.init();
        Mutex::new(serial_port)
    };
}

/// Internal print function. Use `serial_print!` or `serial_println!` instead.
///
/// Disables interrupts while printing to prevent deadlocks:
/// if an interrupt handler tries to print while we hold the lock, it would
/// spin forever waiting for itself to release the lock.
#[doc(hidden)]
pub fn _print(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;

    interrupts::without_interrupts(|| {
        SERIAL1.lock().write_fmt(args).expect("Printing to serial failed");
    });
}

/// Print to the serial console (no newline).
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*));
    };
}

/// Print to the serial console with a trailing newline.
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($fmt:expr) => ($crate::serial_print!(concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => ($crate::serial_print!(
        concat!($fmt, "\n"), $($arg)*));
}
