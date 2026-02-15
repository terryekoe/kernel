use crate::hal::VirtioHal;
use crate::network::LegacyTransport;
use crate::serial_println;
use virtio_drivers::device::net::VirtIONet;
use alloc::vec;

pub fn test_virtio_net(transport: LegacyTransport) {
    serial_println!("[TEST] Initializing high-level VirtIONet...");
    // VirtIONet::new(transport, buf_len) - confirmed by error message
    match VirtIONet::<VirtioHal, LegacyTransport, 256>::new(transport, 2048) {
        Ok(mut net) => {
            serial_println!("[TEST] VirtIONet initialized. MAC: {:02x?}", net.mac_address());
            
            serial_println!("[TEST] Polling for packets (1000 attempts)...");
            
            for i in 0..1000 {
                 match net.receive() {
                     Ok(packet) => {
                         serial_println!("[TEST] Packet received! len={}", packet.packet_len());
                         return; // Success!
                     }
                     // Error::WouldBlock might be missing?
                     // Let's just catch all Err and print/continue.
                     Err(e) => {
                         // serial_println!("[TEST] Receive error: {:?}", e); // Floods log
                         // Just busy wait
                     }
                 }
                 for _ in 0..10000 { core::hint::spin_loop(); }
            }
            serial_println!("[TEST] No packets received after polling.");
        }
        Err(e) => {
            serial_println!("[TEST] Init failed: {:?}", e);
        }
    }
}
