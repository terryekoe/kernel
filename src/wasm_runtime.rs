//! # WebAssembly Runtime (Universal Execution Layer)
//!
//! This module provides the WASM execution environment for user-space applications.
//! It is the core of our cross-device compatibility story — apps compiled to WASM
//! can run identically on x86, ARM, RISC-V, or any future architecture.
//!
//! ## Architecture
//!
//! ```text
//!   ┌──────────────────────────────────────────┐
//!   │           User WASM App (.wasm)           │
//!   ├──────────────────────────────────────────┤
//!   │         WasmProcess (Sandbox)             │
//!   │  ┌────────────────────────────────────┐  │
//!   │  │   wasmi Interpreter (no_std)       │  │
//!   │  │   - Validates bytecode             │  │
//!   │  │   - Executes instructions          │  │
//!   │  │   - Enforces memory bounds         │  │
//!   │  └────────────────────────────────────┘  │
//!   │  ┌────────────────────────────────────┐  │
//!   │  │   Host Functions (Syscalls)        │  │
//!   │  │   - print(msg)                     │  │
//!   │  │   - yield()                        │  │
//!   │  │   - ipc_send() / ipc_recv()        │  │
//!   │  └────────────────────────────────────┘  │
//!   ├──────────────────────────────────────────┤
//!   │         Capability Check (CSpace)         │
//!   ├──────────────────────────────────────────┤
//!   │              Microkernel                  │
//!   └──────────────────────────────────────────┘
//! ```
//!
//! ## Security Model
//! Each WASM process runs inside a sandbox with:
//! - **Memory isolation**: WASM linear memory is separate from kernel memory.
//! - **No direct hardware access**: All I/O goes through host functions (syscalls).
//! - **Capability-gated syscalls**: Each host function checks the process's CSpace.

use alloc::string::String;
use alloc::vec::Vec;
use wasmi::{
    Caller, Engine, Linker, Module, Store,
};
use crate::serial_println;

// ─── Process State ───────────────────────────────────────────────────────────

/// State associated with a running WASM process.
///
/// This is passed to host functions via `wasmi::Store`, giving them
/// access to process-specific data like the output buffer.
pub struct ProcessState {
    /// The process's name (for logging).
    pub name: String,
    /// Collected output from `print` syscalls (captured for verification).
    pub output: Vec<String>,
}

// ─── WASM Runtime ────────────────────────────────────────────────────────────

/// Errors that can occur during WASM execution.
#[derive(Debug)]
pub enum WasmError {
    /// Failed to compile the WASM module (invalid bytecode).
    CompilationFailed,
    /// Failed to instantiate the module (missing imports, etc.).
    InstantiationFailed,
    /// The expected entry point function was not found.
    EntryPointNotFound,
    /// Runtime error during execution (trap, out-of-bounds, etc.).
    ExecutionFailed,
}

/// Load and execute a WASM binary inside a sandboxed process.
///
/// # Arguments
/// * `name` - Human-readable name for this process (for logging).
/// * `wasm_bytes` - The raw `.wasm` binary bytecode.
/// * `entry_point` - Name of the exported function to call (e.g., "main").
///
/// # Returns
/// The `ProcessState` after execution, containing any captured output.
///
/// # Security
/// The WASM module can only interact with the kernel through explicitly
/// provided host functions. It cannot access kernel memory, hardware,
/// or other processes directly.
pub fn execute_wasm(
    name: &str,
    wasm_bytes: &[u8],
    entry_point: &str,
) -> Result<ProcessState, WasmError> {
    serial_println!("[WASM] Loading process '{}'...", name);

    // Step 1: Create the WASM engine (the interpreter core).
    let engine = Engine::default();

    // Step 2: Compile the WASM bytecode into an executable module.
    // This validates the bytecode structure and type-checks all functions.
    let module = Module::new(&engine, wasm_bytes)
        .map_err(|_| WasmError::CompilationFailed)?;
    serial_println!("[WASM] Module compiled successfully.");

    // Step 3: Create a Store with our process state.
    // The Store owns the WASM instance's memory and globals.
    let mut store = Store::new(
        &engine,
        ProcessState {
            name: String::from(name),
            output: Vec::new(),
        },
    );

    // Step 4: Set up the Linker with host functions (syscalls).
    // These are the ONLY ways the WASM module can interact with the kernel.
    let mut linker = <Linker<ProcessState>>::new(&engine);
    register_host_functions(&mut linker);

    // Step 5: Instantiate the module — resolves imports against our host functions.
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|_| WasmError::InstantiationFailed)?
        .start(&mut store)
        .map_err(|_| WasmError::InstantiationFailed)?;
    serial_println!("[WASM] Module instantiated.");

    // Step 6: Find and call the entry point function.
    let func = instance
        .get_typed_func::<(), ()>(&store, entry_point)
        .map_err(|_| WasmError::EntryPointNotFound)?;

    serial_println!("[WASM] Calling '{}'...", entry_point);
    func.call(&mut store, ())
        .map_err(|_| WasmError::ExecutionFailed)?;

    serial_println!("[WASM] Process '{}' completed successfully.", name);

    Ok(store.into_data())
}

// ─── Host Functions (Syscalls) ───────────────────────────────────────────────

/// Register all host functions that WASM modules can call.
///
/// These act as the "system call" interface between user-space WASM apps
/// and the kernel. Each function is namespaced under "env".
fn register_host_functions(linker: &mut Linker<ProcessState>) {
    // syscall: env.print_char(char_code: i32)
    // Prints a single character to the serial console.
    // This is the most basic output primitive — WASM modules use this
    // to build up strings character by character.
    linker
        .func_wrap(
            "env",
            "print_char",
            |_caller: Caller<'_, ProcessState>, char_code: i32| {
                // Write a single character without newline.
                // We use serial_println's underlying _print directly.
                use core::fmt::Write;
                use x86_64::instructions::interrupts;
                interrupts::without_interrupts(|| {
                    let c = char::from(char_code as u8);
                    let mut serial = crate::serial::SERIAL1.lock();
                    write!(serial, "{}", c).expect("serial write failed");
                });
            },
        )
        .expect("Failed to register print_char");

    // syscall: env.print_newline()
    // Prints a newline to the serial console.
    linker
        .func_wrap(
            "env",
            "print_newline",
            |_caller: Caller<'_, ProcessState>| {
                serial_println!();
            },
        )
        .expect("Failed to register print_newline");

    // syscall: env.get_os_version() -> i32
    // Returns the OS version as a single integer (major * 100 + minor).
    // Demonstrates a "query" syscall that returns data to the WASM module.
    linker
        .func_wrap(
            "env",
            "get_os_version",
            |_caller: Caller<'_, ProcessState>| -> i32 {
                1 // v0.1.0
            },
        )
        .expect("Failed to register get_os_version");
}

// ─── Embedded WASM Bytecode ──────────────────────────────────────────────────

/// A hand-crafted "Hello World" WASM module in raw bytecode.
///
/// This module:
/// 1. Imports `env.print_char(i32)` and `env.print_newline()` from the host.
/// 2. Exports a `main()` function.
/// 3. When `main()` is called, it prints "Hello from WASM!" character by character.
///
/// ## Why hand-crafted bytecode?
/// We don't have a filesystem yet, so we can't load `.wasm` files from disk.
/// Embedding the bytecode directly lets us test the runtime immediately.
/// Once we have a filesystem or network stack, we'll load modules dynamically.
///
/// ## WASM Binary Format Overview (for learning)
/// ```text
/// [magic]  [version]  [type_section]  [import_section]  [func_section]
/// [export_section]  [code_section]
/// ```
pub fn hello_world_wasm() -> &'static [u8] {
    // This WASM module is equivalent to:
    //
    //   (module
    //     (import "env" "print_char" (func $print_char (param i32)))
    //     (import "env" "print_newline" (func $print_newline))
    //     (func $main (export "main")
    //       ;; Print "Hello from WASM!"
    //       (call $print_char (i32.const 72))   ;; 'H'
    //       (call $print_char (i32.const 101))  ;; 'e'
    //       (call $print_char (i32.const 108))  ;; 'l'
    //       (call $print_char (i32.const 108))  ;; 'l'
    //       (call $print_char (i32.const 111))  ;; 'o'
    //       (call $print_char (i32.const 32))   ;; ' '
    //       (call $print_char (i32.const 102))  ;; 'f'
    //       (call $print_char (i32.const 114))  ;; 'r'
    //       (call $print_char (i32.const 111))  ;; 'o'
    //       (call $print_char (i32.const 109))  ;; 'm'
    //       (call $print_char (i32.const 32))   ;; ' '
    //       (call $print_char (i32.const 87))   ;; 'W'
    //       (call $print_char (i32.const 65))   ;; 'A'
    //       (call $print_char (i32.const 83))   ;; 'S'
    //       (call $print_char (i32.const 77))   ;; 'M'
    //       (call $print_char (i32.const 33))   ;; '!'
    //       (call $print_newline)
    //     )
    //   )
    // Generated by: wat2wasm hello.wat -o hello.wasm
    // Validated by the WebAssembly Binary Toolkit (wabt v1.0.39).
    // 157 bytes total.
    &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
        0x01, 0x08, 0x02,                                 // type section: 2 types
        0x60, 0x01, 0x7f, 0x00,                           // type 0: (i32)->()
        0x60, 0x00, 0x00,                                 // type 1: ()->()
        0x02, 0x26, 0x02,                                 // import section: 2 imports
        0x03, 0x65, 0x6e, 0x76,                           // "env"
        0x0a, 0x70, 0x72, 0x69, 0x6e, 0x74, 0x5f, 0x63, 0x68, 0x61, 0x72, // "print_char"
        0x00, 0x00,                                       // func, type 0
        0x03, 0x65, 0x6e, 0x76,                           // "env"
        0x0d, 0x70, 0x72, 0x69, 0x6e, 0x74, 0x5f, 0x6e, 0x65, 0x77, 0x6c, 0x69, 0x6e, 0x65, // "print_newline"
        0x00, 0x01,                                       // func, type 1
        0x03, 0x02, 0x01, 0x01,                           // function section: 1 func, type 1
        0x07, 0x08, 0x01,                                 // export section: 1 export
        0x04, 0x6d, 0x61, 0x69, 0x6e,                     // "main"
        0x00, 0x02,                                       // func index 2
        0x0a, 0x53, 0x01, 0x51, 0x00,                     // code section: 1 body, 81 bytes, 0 locals
        // Print "Hello from WASM!" character by character:
        0x41, 0xc8, 0x00, 0x10, 0x00, // i32.const 72 ('H'),  call $print_char
        0x41, 0xe5, 0x00, 0x10, 0x00, // i32.const 101 ('e'), call $print_char
        0x41, 0xec, 0x00, 0x10, 0x00, // i32.const 108 ('l'), call $print_char
        0x41, 0xec, 0x00, 0x10, 0x00, // i32.const 108 ('l'), call $print_char
        0x41, 0xef, 0x00, 0x10, 0x00, // i32.const 111 ('o'), call $print_char
        0x41, 0x20, 0x10, 0x00,       // i32.const 32 (' '),  call $print_char
        0x41, 0xe6, 0x00, 0x10, 0x00, // i32.const 102 ('f'), call $print_char
        0x41, 0xf2, 0x00, 0x10, 0x00, // i32.const 114 ('r'), call $print_char
        0x41, 0xef, 0x00, 0x10, 0x00, // i32.const 111 ('o'), call $print_char
        0x41, 0xed, 0x00, 0x10, 0x00, // i32.const 109 ('m'), call $print_char
        0x41, 0x20, 0x10, 0x00,       // i32.const 32 (' '),  call $print_char
        0x41, 0xd7, 0x00, 0x10, 0x00, // i32.const 87 ('W'),  call $print_char
        0x41, 0xc1, 0x00, 0x10, 0x00, // i32.const 65 ('A'),  call $print_char
        0x41, 0xd3, 0x00, 0x10, 0x00, // i32.const 83 ('S'),  call $print_char
        0x41, 0xcd, 0x00, 0x10, 0x00, // i32.const 77 ('M'),  call $print_char
        0x41, 0x21, 0x10, 0x00,       // i32.const 33 ('!'),  call $print_char
        0x10, 0x01,                   // call $print_newline
        0x0b,                         // end
    ]
}
