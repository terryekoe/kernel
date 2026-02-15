//! # Next-Gen Microkernel - Entry Point
//!
//! This is the main entry point for the microkernel. It runs on bare metal
//! (no underlying OS) and is loaded by the `bootloader` crate.
//!
//! ## Architecture
//! - **Target**: x86_64 (custom JSON spec based on `x86_64-unknown-none`)
//! - **Security**: Capability-based (seL4-inspired)
//! - **No standard library**: `#![no_std]` — we ARE the operating system.
//! - **No main**: `#![no_main]` — the bootloader calls our entry point directly.
//!
//! ## Boot Flow
//! 1. BIOS/UEFI loads the bootloader from disk.
//! 2. The bootloader sets up 64-bit Long Mode, paging, and a stack.
//! 3. The bootloader jumps to `kernel_main` (registered via `entry_point!` macro).
//! 4. We initialize: IDT → Memory Manager → CSpace → idle loop.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)] // Required for interrupt handler calling convention

extern crate alloc;

// A simple bump allocator for the kernel heap.
// wasmi needs dynamic allocation (alloc) to run.
use alloc::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// A minimal bump allocator for kernel heap.
///
/// This allocates memory from a static buffer. It never frees memory.
/// Sufficient for our boot-time WASM demo. A proper allocator
/// (linked-list or slab) will replace this in a future phase.
const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB heap

#[repr(align(4096))]
struct AlignedHeap([u8; HEAP_SIZE]);

static mut HEAP: AlignedHeap = AlignedHeap([0; HEAP_SIZE]);

static HEAP_POS: AtomicUsize = AtomicUsize::new(0);

struct BumpAllocator;

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        crate::serial_println!("[ALLOC] size={}", layout.size());
        let size = layout.size();
        let align = layout.align();
        loop {
            let pos = HEAP_POS.load(Ordering::Relaxed);
            let aligned = (pos + align - 1) & !(align - 1);
            let new_pos = aligned + size;
            if new_pos > HEAP_SIZE {
                return core::ptr::null_mut();
            }
            if HEAP_POS.compare_exchange(pos, new_pos, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                return unsafe { HEAP.0.as_mut_ptr().add(aligned) };
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator does not support deallocation.
        // Memory is reclaimed when the kernel reboots.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator;

mod serial;
mod interrupts;
mod network;
pub mod net_interface;
pub mod net_stack;
mod executor;
mod p2p;
mod p2p_transport;
pub mod p2p_kademlia;
mod random;
mod ipc;
mod memory;
mod capability;
mod wasm_runtime;
mod hal;

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;
use capability::{CSpace, Capability, CapabilityId, CapabilityType, Permissions};
use ipc::{IpcManager, Message};
use x86_64::instructions::port::Port;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

pub fn exit_qemu(exit_code: QemuExitCode) -> ! {
    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

// Register `kernel_main` as the entry point called by the bootloader.
// Configure bootloader to map all physical memory (required for VirtIO DMA)
use bootloader_api::config::Mapping;
const BOOTLOADER_CONFIG: bootloader_api::BootloaderConfig = {
    let mut config = bootloader_api::BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.kernel_stack_size = 1024 * 1024; // 1 MiB stack
    config
};
entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

/// The kernel's main function, called by the bootloader after hardware setup.
///
/// # Arguments
/// * `boot_info` - Contains information from the bootloader:
///   - Memory map (which physical memory regions are usable)
///   - Framebuffer address (for future graphics)
///   - RSDP pointer (for ACPI hardware discovery)
///
/// # Initialization Order
/// The order matters — each subsystem depends on the previous:
/// 1. **IDT** — So we can catch exceptions instead of triple-faulting
/// 2. **Memory** — So we can allocate frames for page tables and stacks
/// 3. **CSpace** — Demonstrates the capability security model
use crate::executor::{Executor, Task};
use spin::Mutex;

lazy_static::lazy_static! {
    pub static ref EXECUTOR: Mutex<Executor> = Mutex::new(Executor::new());
}

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // ── Banner ──────────────────────────────────────────────────────
    serial_println!("====================================");
    serial_println!("  Next-Gen Microkernel v0.1.0");
    serial_println!("  Capability-Based | Rust-Native");
    serial_println!("====================================");
    serial_println!();

    // ── Step 1: Initialize Interrupt Descriptor Table ───────────────
    interrupts::init_idt();

    // ── Step 2: Initialize Memory Manager ──────────────────────────
    let mut frame_allocator = unsafe {
        memory::BootInfoFrameAllocator::init(&boot_info.memory_regions)
    };
    serial_println!("[INIT] Frame allocator initialized from boot memory map.");
    
    // Initialize regions for contiguous DMA usage
    memory::init_regions(&boot_info.memory_regions);

    // ── Step 3: Initialize HAL ──────────────────────────────────────
    if let Some(offset) = boot_info.physical_memory_offset.into_option() {
        hal::init(offset);
        serial_println!("[INIT] HAL initialized with physical memory offset: 0x{:x}", offset);
    } else {
        panic!("[INIT] Failed to get physical memory offset from bootloader!");
    }

    // ── Step 4: Initialize Networking ──
    serial_println!("[INIT] Initializing Networking...");
    network::init();
    p2p::init();
    serial_println!("[INIT] Network initialization complete.");

    // ── Step 5: Initialize Capability Space ─────────────────────────
    serial_println!("[INIT] Initializing Capability Space (CSpace)...");
    let mut cspace = CSpace::new();

    // Create a Root capability
    let root_cap = Capability {
        id: CapabilityId::new(),
        cap_type: CapabilityType::Memory,
        permissions: Permissions::all(),
        resource_id: 0, 
    };
    cspace.insert(root_cap).expect("Failed to insert root cap");
    serial_println!("[INIT] CSpace: Root capability created.");

    // ── Step 6: Initialize IPC Subsystem ────────────────────────────
    let mut ipc_manager = IpcManager::new();
    let ep_slot = ipc_manager.create_endpoint().expect("Failed to create endpoint");
    serial_println!("[INIT] IPC: Endpoint created at slot {}", ep_slot);

    // ── Step 7: WASM Runtime Demo ───────────────────────────────────
    serial_println!("[WASM] ── Phase 2: Universal Execution Layer ──");
    let wasm_bytes = wasm_runtime::hello_world_wasm();
    serial_println!("[WASM] Hello World module: {} bytes", wasm_bytes.len());
    
    // Execute WASM
    match wasm_runtime::execute_wasm("hello_world", wasm_bytes, "main") {
        Ok(state) => { serial_println!("[WASM] Process '{}' exited cleanly.", state.name); },
        Err(e) => { serial_println!("[WASM] Execution failed: {:?}", e); },
    }

    // ── Final Step: Idle Loop with Network Polling ─────────────────
    serial_println!();
    serial_println!("[SUCCESS] Kernel initialized successfully.");
    serial_println!("[IDLE] Entering network polling loop...");

    loop {
        // Halt CPU until next interrupt (Timer fires at 100Hz)
        x86_64::instructions::hlt();

        // Calculate time from ticks (100Hz = 10ms per tick)
        // COMPENSATION: Timer seems to run at ~10kHz instead of 100Hz in QEMU/HVF?
        // Divide by 100 to get roughly real time.
        let ticks = interrupts::get_ticks();
        let time_ms = (ticks / 100) * 10;
        
        // Log heartbeat rarely (every 100*100 ticks = 1s maybe?)
        if ticks % 10000 == 0 {
             // serial_println!("[MAIN] Tick: {} Time: {}ms", ticks, time_ms);
        }

        // Poll the network stack
        let timestamp = smoltcp::time::Instant::from_millis(time_ms as i64);
        net_stack::poll_network(timestamp);
        
        // Poll the async executor
        EXECUTOR.lock().poll();
    }
}

/// Panic handler — called when the kernel hits an unrecoverable error.
///
/// In a real OS, this might trigger a kernel dump or reboot.
/// For now, we print the error to the serial console and halt (exit QEMU).
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!();
    serial_println!("!!! KERNEL PANIC !!!");
    serial_println!("{}", info);
    exit_qemu(QemuExitCode::Failed);
}
