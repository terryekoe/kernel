//! # Interrupt Descriptor Table (IDT)
//!
//! Configures the CPU's Interrupt Descriptor Table to handle:
//! - **CPU Exceptions**: Breakpoints, Double Faults, Page Faults, etc.
//! - **Hardware Interrupts**: Timer ticks, Keyboard input (via the 8259 PIC).
//!
//! ## How it works
//! When the CPU encounters an exception or receives a hardware interrupt signal,
//! it looks up the corresponding entry in the IDT and jumps to the registered
//! handler function. Without an IDT, any exception causes a Triple Fault (reboot).
//!
//! ## The `x86-interrupt` calling convention
//! Interrupt handlers use a special ABI that saves/restores all CPU registers
//! automatically. This is a nightly Rust feature enabled via
//! `#![feature(abi_x86_interrupt)]` in main.rs.

use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::instructions::port::Port;
use lazy_static::lazy_static;
use crate::serial_println;

use core::sync::atomic::{AtomicU64, Ordering};

// 8259 PIC ports
const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// PIC remaps IRQs to these interrupt vector offsets.
/// IRQ 0 (timer) -> vector 32, IRQ 1 (keyboard) -> vector 33, etc.
const PIC1_OFFSET: u8 = 32;
const PIC2_OFFSET: u8 = 40;

/// Timer interrupt vector number (IRQ 0 remapped to 32)
const TIMER_INTERRUPT: u8 = PIC1_OFFSET;

pub static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    /// The global IDT, initialized once at boot.
    ///
    /// We use `lazy_static` because the IDT must live for the entire lifetime
    /// of the kernel (`'static`), and Rust doesn't allow mutable statics
    /// without synchronization.
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        // CPU Exception handlers
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.double_fault.set_handler_fn(double_fault_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);

        // Hardware interrupt handlers
        idt[TIMER_INTERRUPT as usize].set_handler_fn(timer_interrupt_handler);

        idt
    };
}

/// Load the IDT into the CPU.
///
/// After this call, the CPU will use our handlers for exceptions.
/// Must be called early in kernel initialization.
pub fn init_idt() {
    IDT.load();
    init_pic();
    init_pit(100); // 100 Hz timer
    x86_64::instructions::interrupts::enable();
    serial_println!("[INIT] IDT loaded, PIC initialized, timer at 100Hz");
}

/// Initialize the 8259 PIC pair with ICW1-ICW4 sequence.
/// Remaps IRQ 0-7 to vectors 32-39 and IRQ 8-15 to vectors 40-47.
fn init_pic() {
    unsafe {
        let mut cmd1 = Port::<u8>::new(PIC1_COMMAND);
        let mut data1 = Port::<u8>::new(PIC1_DATA);
        let mut cmd2 = Port::<u8>::new(PIC2_COMMAND);
        let mut data2 = Port::<u8>::new(PIC2_DATA);

        // ICW1: start initialization, expect ICW4
        cmd1.write(0x11);
        io_wait();
        cmd2.write(0x11);
        io_wait();

        // ICW2: vector offsets
        data1.write(PIC1_OFFSET);
        io_wait();
        data2.write(PIC2_OFFSET);
        io_wait();

        // ICW3: tell PICs about each other
        data1.write(4); // PIC1: slave at IRQ2
        io_wait();
        data2.write(2); // PIC2: cascade identity
        io_wait();

        // ICW4: 8086 mode
        data1.write(0x01);
        io_wait();
        data2.write(0x01);
        io_wait();

        // Unmask IRQ 0 (timer) only, mask everything else
        data1.write(0xFE); // bit 0 = IRQ0 unmasked
        io_wait();
        data2.write(0xFF); // mask all on PIC2
        io_wait();
    }
}

/// Configure the PIT (channel 0) to fire at the given frequency in Hz.
fn init_pit(freq_hz: u32) {
    let divisor = 1193182u32 / freq_hz;
    unsafe {
        // Channel 0, lo/hi byte, rate generator (mode 2)
        Port::<u8>::new(0x43).write(0x34);
        io_wait();
        Port::<u8>::new(0x40).write((divisor & 0xFF) as u8);
        io_wait();
        Port::<u8>::new(0x40).write(((divisor >> 8) & 0xFF) as u8);
        io_wait();
    }
}

/// Small I/O delay using port 0x80 (unused/safe)
#[inline(always)]
fn io_wait() {
    unsafe { Port::<u8>::new(0x80).write(0); }
}

// ---------------------------------------------------------------------------
// Exception Handlers
// ---------------------------------------------------------------------------

/// Handles a **Breakpoint Exception** (INT 3).
///
/// A breakpoint is a software-generated exception, typically used by debuggers.
/// We log it and resume execution — the instruction pointer is already
/// advanced past the `int3` instruction.
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    serial_println!("[EXCEPTION] Breakpoint");
    serial_println!("  Instruction Pointer: {:?}", stack_frame.instruction_pointer);
    serial_println!("  Stack Pointer:       {:?}", stack_frame.stack_pointer);
}

/// Handles a **Double Fault** (exception during exception handling).
///
/// A double fault is catastrophic — it means the CPU failed to handle
/// a previous exception. We print diagnostics and halt permanently.
/// The `-> !` return type means this handler never returns.
extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("[FATAL] Double Fault!");
    serial_println!("  {:#?}", stack_frame);
    panic!("Double fault — system halted.");
}

/// Handles a **Page Fault** (access to unmapped/protected memory).
///
/// Page faults are common in OS development. They occur when code tries to:
/// - Read/write to an unmapped virtual address
/// - Write to a read-only page
/// - Access a kernel page from user-space
///
/// In a full OS, page faults drive demand paging and copy-on-write.
/// For now, we log the fault address and halt.
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;

    // The CR2 register contains the virtual address that caused the fault.
    serial_println!("[EXCEPTION] Page Fault");
    serial_println!("  Accessed Address: {:?}", Cr2::read());
    serial_println!("  Error Code:       {:?}", error_code);
    serial_println!("  {:#?}", stack_frame);
    panic!("Page fault — cannot continue without a page fault handler.");
}

// ---------------------------------------------------------------------------
// Hardware Interrupt Handlers
// ---------------------------------------------------------------------------

/// Timer interrupt handler (IRQ 0, vector 32).
/// Fires ~100 times/second, waking the CPU from `hlt` to poll the network stack.
extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    TICK_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Send End-Of-Interrupt to PIC1
    unsafe {
        Port::<u8>::new(PIC1_COMMAND).write(0x20);
    }
}

pub fn get_ticks() -> u64 {
    TICK_COUNTER.load(Ordering::Relaxed)
}
