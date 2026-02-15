use x86_64::instructions::port::{Port, PortRead, PortWrite};
use virtio_drivers::{device::net::{VirtIONet, VirtIONetRaw}, transport::{Transport, DeviceType, DeviceStatus}, Error};
use crate::hal::VirtioHal;
use crate::serial_println;
use core::mem::size_of;
use zerocopy::{FromBytes, IntoBytes, Immutable};
use bitflags::Flags;

pub fn init() {
    serial_println!("[NET] Scanning PCI bus for VirtIO Network device...");
    
    // Simple PCI scan
    for bus in 0..255 {
        for device in 0..32 {
            if let Some(header) = unsafe { verify_device(bus, device) } {
                // Check if it's a network device (Device ID 0x1000 for legacy, Vendor ID 0x1af4)
                if header.device_id == 0x1000 && header.vendor_id == 0x1af4 {
                    serial_println!("[NET] Found VirtIO device at {:02x}:{:02x}, Vendor ID: 0x{:04x}, Device ID: 0x{:04x}", 
                        bus, device, header.vendor_id, header.device_id);
                    serial_println!("[NET] Detected Legacy VirtIO Network Device.");
                    
                    // Read BAR0 to get I/O base
                    let bar0 = unsafe { pci_read(bus, device, 0, 0x10) };
                    // If bit 0 is set, it's I/O.
                    if bar0 & 1 == 1 {
                        let io_base = (bar0 & !0x3) as u16;
                        serial_println!("[NET] I/O Base: 0x{:04x}", io_base);
                        
                        // IMPORTANT: Enable Bus Master (bit 2) in Command Register (Offset 4)
                        // IMPORTANT: Enable Bus Master (bit 2) and Memory Space (bit 1)
                        // Command Register is 16 bits at offset 4.
                        let command_reg = unsafe { pci_read(bus, device, 0, 0x04) } as u16;
                        let new_command = command_reg | 0x7; // Bit 0 (IO), Bit 1 (Mem), Bit 2 (Bus Master)
                        unsafe { pci_write_16(bus, device, 0, 0x04, new_command) };
                        serial_println!("[NET] PCI Bus Master + Mem Enabled");

                        let transport = LegacyTransport::new(io_base);

                        // Initialize VirtIONetRaw with 256 queue size (Legacy default)
                        match VirtIONetRaw::<VirtioHal, LegacyTransport, 256>::new(transport) {
                            Ok(net) => {
                                serial_println!("[NET] VirtIO Network Driver Initialized!");
                                let mac = net.mac_address();
                                serial_println!("[NET] MAC Address: {:02x?}", mac);

                                let device = crate::net_interface::VirtioNetDevice::new(net);
                                
                                // PROBE: Check if queues are active using a fresh transport handle
                                let mut probe_transport = LegacyTransport::new(io_base);
                                let rx_active = probe_transport.queue_used(0);
                                let tx_active = probe_transport.queue_used(1);
                                serial_println!("[NET] Queue PFN Probe: RX={}, TX={}", rx_active, tx_active);

                                crate::net_stack::init(device, mac);
                            }
                            Err(e) => {
                                serial_println!("[NET] Failed to initialize VirtioNet: {:?}", e);
                            }
                        }
                        return; // Found and initialized
                    } else {
                        serial_println!("[NET] BAR0 is not I/O space. Legacy VirtIO requires I/O.");
                    }
                }
            }
        }
    }
    serial_println!("[NET] No VirtIO Network device found.");
}

// Minimal PCI helpers
#[derive(Debug, Clone, Copy)]
struct PciHeader {
    vendor_id: u16,
    device_id: u16,
}

unsafe fn pci_read(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let address = 0x80000000 | ((bus as u32) << 16) | ((slot as u32) << 11) | ((func as u32) << 8) | ((offset as u32) & 0xfc);
    let mut command_port = Port::<u32>::new(0xCF8);
    let mut data_port = Port::<u32>::new(0xCFC);
    command_port.write(address);
    data_port.read()
}

unsafe fn pci_write(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let address = 0x80000000 | ((bus as u32) << 16) | ((slot as u32) << 11) | ((func as u32) << 8) | ((offset as u32) & 0xfc);
    let mut command_port = Port::<u32>::new(0xCF8);
    let mut data_port = Port::<u32>::new(0xCFC);
    command_port.write(address);
    data_port.write(value);
}

unsafe fn pci_write_16(bus: u8, slot: u8, func: u8, offset: u8, value: u16) {
    let address = 0x80000000 | ((bus as u32) << 16) | ((slot as u32) << 11) | ((func as u32) << 8) | ((offset as u32) & 0xfc);
    let mut command_port = Port::<u32>::new(0xCF8);
    // Data port for 16-bit access depends on offset alignment (0xCFC + (offset & 2))
    let mut data_port = Port::<u16>::new(0xCFC + (offset as u16 & 2));
    command_port.write(address);
    data_port.write(value);
}

unsafe fn verify_device(bus: u8, slot: u8) -> Option<PciHeader> {
    let id = pci_read(bus, slot, 0, 0);
    if id == 0xFFFFFFFF {
        return None;
    }
    Some(PciHeader {
        vendor_id: (id & 0xFFFF) as u16,
        device_id: ((id >> 16) & 0xFFFF) as u16,
    })
}

// Legacy Transport Implementation
pub struct LegacyTransport {
    io_base: u16,
}

impl LegacyTransport {
    pub fn new(io_base: u16) -> Self {
        Self { io_base }
    }
}

// Offsets
const HOST_FEATURES: u16 = 0;
const GUEST_FEATURES: u16 = 4;
const QUEUE_PFN: u16 = 8;
const QUEUE_SIZE: u16 = 12;
const QUEUE_SEL: u16 = 14;
const QUEUE_NOTIFY: u16 = 16;
const DEVICE_STATUS: u16 = 18;
const ISR_STATUS: u16 = 19;
// Config space starts at 20 for legacy
const CONFIG_OFFSET: u16 = 20; 

impl Transport for LegacyTransport {
    fn device_type(&self) -> DeviceType {
        DeviceType::Network // We assume it's network because we checked Device ID 0x1000
    }

    fn read_device_features(&mut self) -> u64 {
        unsafe {
            let mut port = Port::<u32>::new(self.io_base + HOST_FEATURES);
            port.read() as u64
        }
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        unsafe {
            let mut port = Port::<u32>::new(self.io_base + GUEST_FEATURES);
            port.write(driver_features as u32);
        }
    }

    fn begin_init<F: Flags<Bits = u64> + core::ops::BitAnd<Output = F> + core::fmt::Debug>(
        &mut self,
        supported_features: F,
    ) -> F {
        // 1. Reset
        self.set_status(DeviceStatus::empty());

        // 2. Set ACKNOWLEDGE | DRIVER
        self.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        // 3. Read features
        let device_features = F::from_bits_truncate(self.read_device_features());
        // 3. Read features
        let device_features = F::from_bits_truncate(self.read_device_features());

        // 4. Negotiate
        // Mask out INDIRECT_DESC (28) and EVENT_IDX (29) to use simple direct descriptors
        // 1<<28 = 0x10000000, 1<<29 = 0x20000000
        let mut negotiated_features = device_features & supported_features;
        let mask = F::from_bits_truncate(0x10000000 | 0x20000000); 
        negotiated_features.remove(mask);
        
        
        self.write_driver_features(negotiated_features.bits());

        // 5. Set FEATURES_OK (ignored by legacy but good practice/required by drivers crate?)
        // The default impl does this. Legacy ignores it.
        self.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);

        negotiated_features
    }

    fn finish_init(&mut self) {
        // 6. Set DRIVER_OK
        self.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        unsafe {
            let mut sel_port = Port::<u16>::new(self.io_base + QUEUE_SEL);
            sel_port.write(queue);
            let mut size_port = Port::<u16>::new(self.io_base + QUEUE_SIZE);
            size_port.read() as u32
        }
    }

    fn notify(&mut self, queue: u16) {
        unsafe {
            let mut port = Port::<u16>::new(self.io_base + QUEUE_NOTIFY);
            port.write(queue);
        }
    }

    fn get_status(&self) -> DeviceStatus {
        unsafe {
            let mut port = Port::<u8>::new(self.io_base + DEVICE_STATUS);
            let s = port.read();
            DeviceStatus::from_bits_truncate(s.into())
        }
    }

    fn set_status(&mut self, status: DeviceStatus) {
        unsafe {
            let mut port = Port::<u8>::new(self.io_base + DEVICE_STATUS);
            port.write(status.bits() as u8); // bits() returns u32
        }
    }

    fn set_guest_page_size(&mut self, _guest_page_size: u32) {
        // Legacy VirtIO uses fixed 4096 page size (implied).
    }

    fn requires_legacy_layout(&self) -> bool {
        true
    }

    fn queue_set(
        &mut self,
        queue: u16,
        _size: u32,
        descriptors: usize, // PhysAddr is alias for usize
        _driver_area: usize,
        _device_area: usize,
    ) {
        // Legacy VirtIO: write PFN to QUEUE_PFN.
        // PFN = physical_address >> 12.
        // We assume descriptors points to the start of the contiguous common area.
        let pfn = (descriptors as u32) >> 12;

        if descriptors > 0xFFFFFFFF {
            serial_println!("[VIRTIO] WARNING: descriptors address > 4GB, PFN will be truncated!");
        }
        unsafe {
            let mut sel_port = Port::<u16>::new(self.io_base + QUEUE_SEL);
            sel_port.write(queue);
            let mut pfn_port = Port::<u32>::new(self.io_base + QUEUE_PFN);
             pfn_port.write(pfn);
        }
    }

    fn queue_unset(&mut self, _queue: u16) {
        // Not easily supported in Legacy
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        unsafe {
            let mut sel_port = Port::<u16>::new(self.io_base + QUEUE_SEL);
            sel_port.write(queue);
            let mut pfn_port = Port::<u32>::new(self.io_base + QUEUE_PFN);
            pfn_port.read() != 0
        }
    }

    fn ack_interrupt(&mut self) -> bool {
        unsafe {
            let mut port = Port::<u8>::new(self.io_base + ISR_STATUS);
            // Reading ISR status resets it
            let status = port.read();
            status & 1 != 0
        }
    }

    fn read_config_generation(&self) -> u32 {
        0 // Legacy doesn't support config generation
    }

    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> Result<T, Error> {
        // T is generic. We read bytes.
        let type_size = size_of::<T>();
        let mut buffer = [0u8; 64]; // Enough for MAC (6) + Status (2) etc.
        if type_size > buffer.len() {
             return Err(Error::ConfigSpaceMissing); // Or equivalent
        }
        
        for i in 0..type_size {
             buffer[i] = unsafe { Port::<u8>::new(self.io_base + CONFIG_OFFSET + offset as u16 + i as u16).read() };
        }
        
        // Safety: T is FromBytes, so it can be created from bytes.
        let val = T::read_from(&buffer[..type_size]).ok_or(Error::IoError);
        val
    }

    fn write_config_space<T: IntoBytes + Immutable>(&mut self, offset: usize, value: T) -> Result<(), Error> {
         let bytes = value.as_bytes();
         for (i, &byte) in bytes.iter().enumerate() {
             unsafe { Port::<u8>::new(self.io_base + CONFIG_OFFSET + offset as u16 + i as u16).write(byte); }
         }
         Ok(())
    }
}
