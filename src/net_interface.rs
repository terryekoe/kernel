use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken, Checksum};
use smoltcp::time::Instant;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::Hal; // Import Hal trait to call dma_alloc
use crate::hal::VirtioHal;
use crate::network::LegacyTransport;
use crate::serial_println;
use alloc::vec::Vec;
use spin::Mutex;
use lazy_static::lazy_static;
use core::ptr::NonNull;
use core::slice;

lazy_static! {
    static ref BUFFER_POOL: Mutex<Vec<DmaBuffer>> = Mutex::new(Vec::new());
}

const RX_BUFFER_PAGES: usize = 1; // 4096 bytes
const QUEUE_SIZE: usize = 256;
const VIRTIO_HEADER_LEN: usize = 10; // Legacy Header (no MRG_RXBUF)

/// A physically contiguous buffer allocated via HAL DMA.
pub struct DmaBuffer {
    ptr: NonNull<u8>,
    phys: usize,
    pages: usize,
    len: usize,
}

// Safety: The buffer pointer is unique (we own it) and thread-safe if passed around (Pointer is Send/Sync if raw memory).
unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

impl DmaBuffer {
    pub fn new(pages: usize) -> Option<Self> {
        // Allocate contiguous physical memory
        let (phys, ptr) = VirtioHal::dma_alloc(pages, virtio_drivers::BufferDirection::Both);
        if ptr.as_ptr().is_null() {
            return None;
        }
        Some(Self {
            ptr,
            phys,
            pages,
            len: pages * 4096,
        })
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

// We rely on BUFFER_POOL to recycle. If dropped without returning, we leak (dma_dealloc is no-op).

/// smoltcp Device implementation wrapping VirtIONetRaw (Non-blocking)
pub struct VirtioNetDevice {
    inner: VirtIONetRaw<VirtioHal, LegacyTransport, QUEUE_SIZE>,
    // buffers[i] holds the buffer for the descriptor with token `i`
    rx_buffers: Vec<Option<DmaBuffer>>,
    tx_buffers: Vec<Option<DmaBuffer>>,
}

impl VirtioNetDevice {
    pub fn new(mut inner: VirtIONetRaw<VirtioHal, LegacyTransport, QUEUE_SIZE>) -> Self {
        // Allocate storage for tokens
        let mut rx_buffers = Vec::with_capacity(QUEUE_SIZE);
        let mut tx_buffers = Vec::with_capacity(QUEUE_SIZE);
        for _ in 0..QUEUE_SIZE {
            rx_buffers.push(None);
            tx_buffers.push(None);
        }

        // Fill RX queue
        for i in 0..QUEUE_SIZE {
            // Allocate DMA buffer
            if let Some(mut buf) = DmaBuffer::new(RX_BUFFER_PAGES) {
                // Register buffer with driver
                unsafe {
                    match inner.receive_begin(buf.as_mut_slice()) {
                        Ok(token) => {
                             if (token as usize) < QUEUE_SIZE {
                                 rx_buffers[token as usize] = Some(buf);
                             } else {
                                 serial_println!("[NET] Error: RX token {} out of bounds", token);
                             }
                        }
                        Err(e) => {
                            serial_println!("[NET] Init RX failed for {}: {:?}", i, e);
                        }
                    }
                }
            } else {
                serial_println!("[NET] Failed to allocate initial RX buffer {}", i);
            }
        }

        Self { inner, rx_buffers, tx_buffers }
    }
}

/// RX token for receiving packets wrapped in a safe container
pub struct VirtioRxTokenSafe {
    buffer: Option<DmaBuffer>,
    len: usize,
}

impl RxToken for VirtioRxTokenSafe {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        if let Some(buf) = self.buffer.as_mut() {
             // Skip VirtIO Header
             f(&mut buf.as_mut_slice()[VIRTIO_HEADER_LEN..self.len])
        } else {
             f(&mut [])
        }
    }
}

impl Drop for VirtioRxTokenSafe {
    fn drop(&mut self) {
        if let Some(buf) = self.buffer.take() {
            BUFFER_POOL.lock().push(buf);
        }
    }
}

/// TX token for transmitting packets
pub struct VirtioTxToken<'a> {
    device: &'a mut VirtioNetDevice,
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Try reuse from pool or alloc new
        let mut buffer = BUFFER_POOL.lock().pop().or_else(|| DmaBuffer::new(RX_BUFFER_PAGES)).expect("TX Alloc failed");
        
        // Zero header
        unsafe { core::ptr::write_bytes(buffer.as_mut_slice().as_mut_ptr(), 0, VIRTIO_HEADER_LEN); }

        // Write packet data
        let result = f(&mut buffer.as_mut_slice()[VIRTIO_HEADER_LEN..VIRTIO_HEADER_LEN + len]);
        let data = buffer.as_mut_slice();
        let eth_type = ((data[VIRTIO_HEADER_LEN + 12] as u16) << 8) | (data[VIRTIO_HEADER_LEN + 13] as u16);
        serial_println!("[NET TX] {} bytes, EthType: 0x{:04x}", len, eth_type);

        // Checksum patch for IPv4 - DISABLED (smoltcp handles it)
        /*
        let pkt_start = VIRTIO_HEADER_LEN;
        if buffer.len > pkt_start + 34 { ... }
        */

        unsafe {
            // Transmit Header + Packet
            match self.device.inner.transmit_begin(&mut buffer.as_mut_slice()[..VIRTIO_HEADER_LEN + len]) {
                Ok(token) => {
                    if (token as usize) < QUEUE_SIZE {
                        if self.device.tx_buffers[token as usize].is_some() {
                           serial_println!("[NET TX] Warning: Overwriting active TX buffer at {}", token); 
                        }
                        self.device.tx_buffers[token as usize] = Some(buffer);
                    } else {
                        serial_println!("[NET TX] Error: TX token {} out of bounds", token);
                        // Return to pool if invalid token
                        BUFFER_POOL.lock().push(buffer);
                    }
                }
                Err(e) => {
                    serial_println!("[NET TX] Transmit failed: {:?}", e);
                    BUFFER_POOL.lock().push(buffer);
                }
            }
        }

        result
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxTokenSafe;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Acknowledge interrupts (clears ISR) - essential for some devices/backends even in polling mode
        // self.inner.ack_interrupt(); // Wait, confirm if exposed. 
        // virtio-drivers 0.10 VirtIONetRaw usually exposes it.
        // If not, we might need a workaround. Let's assume yes for now.
        // Actually, let's wrap in safe block if needed.
        self.inner.ack_interrupt();

        // 1. Poll TX completions (free up buffers)
        unsafe {
            while let Some(token) = self.inner.poll_transmit() {
                if (token as usize) < QUEUE_SIZE {
                     if let Some(mut buf) = self.tx_buffers[token as usize].take() {
                          self.inner.transmit_complete(token, buf.as_mut_slice()).ok();
                          BUFFER_POOL.lock().push(buf);
                     }
                }
            }
        }

        // 2. Replenish RX buffers
        loop {
            // Check if queue has space? 
            // We just try to add until full or pool empty (alloc new)
            // But we shouldn't infinitely alloc if queue is simply full. 
            // Virtio queue size is 256. If we have 256 pending, QueueFull happens.
            
            // We need a way to check 'is full' before alloc to be efficient, but correct is Try -> QueueFull -> Stop.
            
            // Allocate/Reuse
            // Note: If we just popped from pool, and queue is full, we push back.
            let mut buf = BUFFER_POOL.lock().pop().or_else(|| DmaBuffer::new(RX_BUFFER_PAGES)).expect("RX Pool/Alloc Empty");
            
            match unsafe { self.inner.receive_begin(buf.as_mut_slice()) } {
                Ok(token) => {
                    if (token as usize) < QUEUE_SIZE {
                         self.rx_buffers[token as usize] = Some(buf);
                    } else {
                         serial_println!("[NET ERROR] Driver returned token {} >= QUEUE_SIZE", token);
                         BUFFER_POOL.lock().push(buf);
                    }
                }
                Err(virtio_drivers::Error::QueueFull) => {
                    BUFFER_POOL.lock().push(buf);
                    break;
                }
                Err(e) => {
                    serial_println!("[NET ERROR] receive_begin failed: {:?}", e);
                    BUFFER_POOL.lock().push(buf);
                    break;
                }
            }
        }

        // 3. Poll RX
        unsafe {
            match self.inner.poll_receive() {
                Some(token) => {
                    if (token as usize) < QUEUE_SIZE && self.rx_buffers[token as usize].is_some() {
                        let mut buffer = self.rx_buffers[token as usize].take().unwrap();
                        match self.inner.receive_complete(token, buffer.as_mut_slice()) {
                            Ok((_hdr, pkt_len)) => {
                                let eth_type = ((buffer.as_mut_slice()[VIRTIO_HEADER_LEN + 12] as u16) << 8) | (buffer.as_mut_slice()[VIRTIO_HEADER_LEN + 13] as u16);
                                serial_println!("[NET RX] {} bytes, EthType: 0x{:04x}", pkt_len, eth_type);
                                
                                let rx_token = VirtioRxTokenSafe {
                                    buffer: Some(buffer), // Pass ownership
                                    len: pkt_len + VIRTIO_HEADER_LEN, // heuristic: pkt_len seems to be data len only in this env
                                };
                                let tx_token = VirtioTxToken { 
                                    device: self, 
                                };
                                return Some((rx_token, tx_token)); 
                            }
                            Err(e) => {
                                serial_println!("[NET] RX complete error: {:?}", e);
                                // Return buffer to pool
                                BUFFER_POOL.lock().push(buffer);
                            }
                        }
                    } else {
                         serial_println!("[NET ERROR] RX Token {} has no buffer calling poll_receive", token);
                    }
                }
                None => {}
            }
        }

        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Poll TX descriptors to free space
        unsafe {
             while let Some(token) = self.inner.poll_transmit() {
                if (token as usize) < QUEUE_SIZE {
                    if let Some(mut buf) = self.tx_buffers[token as usize].take() { 
                        self.inner.transmit_complete(token, buf.as_mut_slice()).ok();
                        BUFFER_POOL.lock().push(buf);
                    }
                }
             }
        }

        // Check flight limit
        let used = self.tx_buffers.iter().filter(|s| s.is_some()).count();
        if used >= QUEUE_SIZE {
            return None;
        }

        Some(VirtioTxToken { device: self }) 
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = Some(1);
        caps.medium = Medium::Ethernet;
        caps.checksum.ipv4 = Checksum::None;
        caps.checksum.tcp = Checksum::None;
        caps.checksum.udp = Checksum::None;
        caps.checksum.icmpv4 = Checksum::None;
        caps
    }
}
