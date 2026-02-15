//! # Inter-Process Communication (IPC)
//!
//! Implements capability-gated message passing between isolated protection domains.
//!
//! ## Design (seL4-inspired)
//!
//! IPC in this kernel uses **Endpoints** — named rendezvous points where
//! a sender and receiver meet to exchange messages.
//!
//! ```text
//!   Process A              Kernel               Process B
//!   ─────────              ──────               ─────────
//!       │                                           │
//!       │── send(endpoint_cap, msg) ──►│            │
//!       │                              │            │
//!       │                   Checks cap │            │
//!       │                   Queues msg │            │
//!       │                              │            │
//!       │                              │◄── recv(endpoint_cap) ──│
//!       │                              │            │
//!       │                              │── deliver msg ──────►  │
//!       │                                           │
//! ```
//!
//! ## Key Concepts
//! - **Endpoint**: A kernel object where messages are buffered.
//! - **Capability**: A process must hold an `EndpointCap` to send/receive.
//! - **Message**: A fixed-size payload (registers + optional data buffer).
//! - **Synchronous**: In seL4, IPC is synchronous (sender blocks until receiver
//!   picks up). We start with an async queue for simplicity.
//!
//! ## Security
//! Every `send()` and `receive()` operation requires the caller to present
//! a valid capability with the correct permissions. Without the right key,
//! a process cannot even know an endpoint exists.

use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

// ─── Message ─────────────────────────────────────────────────────────────────

/// Maximum number of data words in a single IPC message.
///
/// seL4 uses register-sized words for zero-copy fast-path IPC.
/// We use 8 words (64 bytes on x86_64) which fits in a cache line.
pub const MAX_MESSAGE_WORDS: usize = 8;

/// An IPC message — the unit of communication between processes.
///
/// Contains a label (message type) and a fixed-size data buffer.
/// For larger payloads, processes would use shared memory capabilities.
#[derive(Debug, Clone)]
pub struct Message {
    /// Application-defined message type/label.
    /// Convention: label 0 = "no message" / null.
    pub label: u64,

    /// Message payload — up to 8 machine words (64 bytes on x86_64).
    /// Unused words should be zeroed.
    pub data: [u64; MAX_MESSAGE_WORDS],

    /// Number of valid words in `data` (0..=MAX_MESSAGE_WORDS).
    pub length: usize,

    /// Sender's endpoint ID (filled in by the kernel, not the sender).
    /// Allows the receiver to identify who sent the message.
    pub sender_id: u64,
}

impl Message {
    /// Create an empty message with the given label.
    pub const fn new(label: u64) -> Self {
        Message {
            label,
            data: [0; MAX_MESSAGE_WORDS],
            length: 0,
            sender_id: 0,
        }
    }

    /// Create a message with a label and one data word.
    pub const fn with_data1(label: u64, word0: u64) -> Self {
        let mut msg = Message::new(label);
        msg.data[0] = word0;
        msg.length = 1;
        msg
    }

    /// Create a message with a label and two data words.
    pub const fn with_data2(label: u64, word0: u64, word1: u64) -> Self {
        let mut msg = Message::new(label);
        msg.data[0] = word0;
        msg.data[1] = word1;
        msg.length = 2;
        msg
    }
}

// ─── Endpoint ────────────────────────────────────────────────────────────────

/// Maximum number of messages that can be queued in an endpoint.
const ENDPOINT_QUEUE_SIZE: usize = 16;

/// Global counter for generating unique endpoint IDs.
static NEXT_ENDPOINT_ID: AtomicU64 = AtomicU64::new(1);

/// An IPC Endpoint — a kernel object where messages are exchanged.
///
/// Each endpoint has a bounded message queue. Senders enqueue messages;
/// receivers dequeue them. If the queue is full, send fails (no blocking yet).
pub struct Endpoint {
    /// Unique identifier for this endpoint.
    pub id: u64,

    /// Bounded ring buffer of messages.
    queue: [Option<Message>; ENDPOINT_QUEUE_SIZE],

    /// Index of the next message to dequeue (read pointer).
    head: usize,

    /// Index of the next free slot to enqueue into (write pointer).
    tail: usize,

    /// Number of messages currently in the queue.
    count: usize,
}

impl Endpoint {
    /// Create a new endpoint with a unique ID and an empty queue.
    pub fn new() -> Self {
        const EMPTY: Option<Message> = None;
        Endpoint {
            id: NEXT_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed),
            queue: [EMPTY; ENDPOINT_QUEUE_SIZE],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Enqueue a message into this endpoint.
    ///
    /// Returns `Ok(())` if the message was queued successfully,
    /// or `Err(IpcError::QueueFull)` if the buffer is full.
    pub fn send(&mut self, msg: Message) -> Result<(), IpcError> {
        if self.count >= ENDPOINT_QUEUE_SIZE {
            return Err(IpcError::QueueFull);
        }

        self.queue[self.tail] = Some(msg);
        self.tail = (self.tail + 1) % ENDPOINT_QUEUE_SIZE;
        self.count += 1;
        Ok(())
    }

    /// Dequeue the next message from this endpoint.
    ///
    /// Returns `Ok(message)` if a message was available,
    /// or `Err(IpcError::QueueEmpty)` if there are no pending messages.
    pub fn receive(&mut self) -> Result<Message, IpcError> {
        if self.count == 0 {
            return Err(IpcError::QueueEmpty);
        }

        let msg = self.queue[self.head]
            .take()
            .expect("Queue count > 0 but slot was None — invariant violated");
        self.head = (self.head + 1) % ENDPOINT_QUEUE_SIZE;
        self.count -= 1;
        Ok(msg)
    }

    /// Returns the number of messages currently queued.
    pub fn pending_count(&self) -> usize {
        self.count
    }

    /// Returns true if the queue has no messages.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

// ─── IPC Errors ──────────────────────────────────────────────────────────────

/// Errors that can occur during IPC operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// The endpoint's message queue is full. Sender should retry later.
    QueueFull,
    /// The endpoint's message queue is empty. No messages to receive.
    QueueEmpty,
    /// The caller does not hold a valid capability for this operation.
    PermissionDenied,
    /// The specified endpoint does not exist.
    InvalidEndpoint,
}

// ─── IPC Manager ─────────────────────────────────────────────────────────────

/// Maximum number of endpoints the kernel can manage.
const MAX_ENDPOINTS: usize = 32;

/// The global IPC manager — owns all endpoints and mediates access.
///
/// All IPC operations go through this manager, which enforces
/// capability-based access control before touching any endpoint.
pub struct IpcManager {
    /// Array of all kernel-managed endpoints.
    endpoints: [Option<Mutex<Endpoint>>; MAX_ENDPOINTS],
    /// Number of endpoints currently active.
    count: usize,
}

impl IpcManager {
    /// Create a new IPC manager with no endpoints.
    pub const fn new() -> Self {
        const EMPTY: Option<Mutex<Endpoint>> = None;
        IpcManager {
            endpoints: [EMPTY; MAX_ENDPOINTS],
            count: 0,
        }
    }

    /// Create a new endpoint and return its slot index.
    ///
    /// The caller should create an `EndpointCap` capability pointing
    /// to this slot index and grant it to the appropriate processes.
    pub fn create_endpoint(&mut self) -> Result<usize, IpcError> {
        for (i, slot) in self.endpoints.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(Mutex::new(Endpoint::new()));
                self.count += 1;
                return Ok(i);
            }
        }
        Err(IpcError::InvalidEndpoint) // No free slots
    }

    /// Send a message to an endpoint by slot index.
    ///
    /// In a full implementation, this would also check the caller's
    /// capability. For now, capability checking is done at a higher level.
    pub fn send(&self, endpoint_slot: usize, msg: Message) -> Result<(), IpcError> {
        match self.endpoints.get(endpoint_slot) {
            Some(Some(endpoint)) => endpoint.lock().send(msg),
            _ => Err(IpcError::InvalidEndpoint),
        }
    }

    /// Receive a message from an endpoint by slot index.
    pub fn receive(&self, endpoint_slot: usize) -> Result<Message, IpcError> {
        match self.endpoints.get(endpoint_slot) {
            Some(Some(endpoint)) => endpoint.lock().receive(),
            _ => Err(IpcError::InvalidEndpoint),
        }
    }

    /// Get the number of pending messages in an endpoint.
    pub fn pending_count(&self, endpoint_slot: usize) -> Result<usize, IpcError> {
        match self.endpoints.get(endpoint_slot) {
            Some(Some(endpoint)) => Ok(endpoint.lock().pending_count()),
            _ => Err(IpcError::InvalidEndpoint),
        }
    }

    /// Returns the total number of active endpoints.
    pub fn endpoint_count(&self) -> usize {
        self.count
    }
}
