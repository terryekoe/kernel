use smoltcp::iface::{Config, Interface, SocketSet, SocketHandle};
use smoltcp::socket::dhcpv4;
use smoltcp::socket::udp::{self, PacketMetadata as UdpPacketMetadata, Socket as UdpSocket};
use smoltcp::socket::tcp::{self, Socket as TcpSocket, SocketBuffer as TcpSocketBuffer};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr};
use crate::net_interface::VirtioNetDevice;
use crate::serial_println;
use spin::Mutex;
use lazy_static::lazy_static;
use alloc::vec::Vec;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};

lazy_static! {
    pub static ref NETWORK_STACK: Mutex<Option<NetworkStack>> = Mutex::new(None);
}

pub struct NetworkStack {
    pub iface: Interface,
    pub device: VirtioNetDevice,
    pub sockets: SocketSet<'static>,
    pub dhcp_handle: SocketHandle,
    pub udp_handle: SocketHandle,
    pub tcp_handle: SocketHandle,
    pub p2p_handle: SocketHandle,
}

impl NetworkStack {
    pub fn new(mut device: VirtioNetDevice, mac: [u8; 6]) -> Self {
        serial_println!("[NET STACK] Creating interface with MAC: {:02x?}", mac);

        // Create interface configuration
        let ethernet_addr = EthernetAddress(mac);
        let hw_addr = HardwareAddress::Ethernet(ethernet_addr);
        let config = Config::new(hw_addr);

        // Create interface (needs mutable ref to device)
        let mut iface = Interface::new(config, &mut device, Instant::ZERO);
        
        // Static IP Configuration (10.0.2.15)
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(smoltcp::wire::IpAddress::v4(10, 0, 2, 15), 24)).ok();
        });
        iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).ok();

        // Create socket set
        let mut sockets = SocketSet::new(Vec::new());

        // 1. DHCP Socket (Optional, kept for testing)
        let dhcp_socket = dhcpv4::Socket::new();
        let dhcp_handle = sockets.add(dhcp_socket);
        /* 
        let dhcp_handle = SocketHandle::default();
        */

        // 2. UDP Echo Socket (Port 6969)
        let udp_rx_buffer = udp::PacketBuffer::new(
            vec![UdpPacketMetadata::EMPTY; 4],
            vec![0; 1024]
        );
        let udp_tx_buffer = udp::PacketBuffer::new(
            vec![UdpPacketMetadata::EMPTY; 4],
            vec![0; 1024]
        );
        let mut udp_socket = UdpSocket::new(udp_rx_buffer, udp_tx_buffer);
        udp_socket.bind(6969).expect("Failed to bind UDP socket");
        let udp_handle = sockets.add(udp_socket);

        // 3. TCP Echo Socket (Port 80)
        let tcp_rx_buffer = TcpSocketBuffer::new(vec![0; 1024]);
        let tcp_tx_buffer = TcpSocketBuffer::new(vec![0; 1024]);
        let mut tcp_socket = TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);
        tcp_socket.listen(80).expect("Failed to listen on TCP socket");
        let tcp_handle = sockets.add(tcp_socket);

        // 4. P2P Socket (Port 40444)
        let mut p2p_rx_buffer = tcp::SocketBuffer::new(vec![0; 4096]);
        let mut p2p_tx_buffer = tcp::SocketBuffer::new(vec![0; 4096]);
        let mut p2p_socket = TcpSocket::new(p2p_rx_buffer, p2p_tx_buffer);
        p2p_socket.listen(40444).expect("Failed to listen on P2P port");
        let p2p_handle = sockets.add(p2p_socket);

        serial_println!("[NET STACK] Interface created.");
        serial_println!("[NET STACK] Services: DHCP, UDP Echo (6969), TCP Echo (80), P2P (40444)");

        Self {
            iface,
            device,
            sockets,
            dhcp_handle,
            udp_handle,
            tcp_handle,
            p2p_handle,
        }
    }

    pub fn poll(&mut self, timestamp: Instant) {
        static POLL_COUNT: AtomicU64 = AtomicU64::new(0);
        let count = POLL_COUNT.fetch_add(1, Ordering::Relaxed);

        if count % 500 == 0 {
             if let Some(cidr) = self.iface.ip_addrs().first() {
                 serial_println!("[NET STACK] Poll #{}: IP: {} Time: {}ms", count, cidr, timestamp.total_millis());
             } else {
                 serial_println!("[NET STACK] Poll #{}: No IP. Time: {}ms", count, timestamp.total_millis());
             }
        }

        // 1. Handle DHCP
        let socket = self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp_handle);
        let event = socket.poll();
        if event.is_some() {
             serial_println!("[NET STACK] DHCP Event: {:?}", event);
        }
        match event {
            Some(dhcpv4::Event::Configured(config)) => {
                serial_println!("[NET STACK] DHCP configuration received:");
                serial_println!("  IP Address: {}", config.address);
                if let Some(router) = config.router {
                    serial_println!("  Gateway: {}", router);
                }

                // Update interface IP addresses
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    addrs.push(IpCidr::Ipv4(config.address)).ok();
                });

                // Set default route via router
                if let Some(router) = config.router {
                    self.iface.routes_mut().add_default_ipv4_route(router).ok();
                }
            }
            Some(dhcpv4::Event::Deconfigured) => {
                serial_println!("[NET STACK] DHCP lease lost. Setting fallback IP 10.0.2.15");
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    addrs.push(IpCidr::new(smoltcp::wire::IpAddress::v4(10, 0, 2, 15), 24)).ok();
                });
                self.iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).ok();
            }
            None => {}
        }
        
        /*
        */

        // Poll the interface
        let changed = self.iface.poll(timestamp, &mut self.device, &mut self.sockets);

        // Log significant changes (optional, reduced verbosity)
        if changed && count < 50 {
            // serial_println!("[NET STACK] poll #{}: interface state changed!", count);
        }

        // 2. Handle UDP Echo
        let socket = self.sockets.get_mut::<UdpSocket>(self.udp_handle);
        if socket.can_recv() {
            let mut buf = [0u8; 1500];
            match socket.recv_slice(&mut buf) {
                Ok((len, endpoint)) => {
                    serial_println!("[UDP] Recv {} bytes from {}", len, endpoint);
                    // Echo back
                    if socket.can_send() {
                         socket.send_slice(&buf[..len], endpoint).ok();
                    }
                }
                Err(_) => {}
            }
        }

        // 3. Handle TCP Echo
        let socket = self.sockets.get_mut::<TcpSocket>(self.tcp_handle);
        if socket.is_active() && !socket.is_open() {
             // connection closed, re-listen
             // actually smoltcp tcp socket stays in state, we might need to check if we need to listen again?
             // listen() puts it in Listen state. If it was Active (connected) and then remote closed, it goes to CloseWait/LastAck/Closed.
             // We need to re-listen if it's Closed.
             // For now, simple echo:
        }
        
        if socket.may_recv() {
            // We can read data
            // Since we want to echo, we can just pipe recv to send?
            // But we need a buffer or loop.
            // Let's inspect the recv buffer.
            
            // Note: simple echo using recv_slice/send_slice
            // We need to dequeue data to free buffer space
            // socket.recv(|data| {
            //     if data.len() > 0 {
            //         serial_println!("[TCP] Recv {} bytes", data.len());
            //          // send queue might be full, so we can't always echo all.
            //          // For this simple demo, assume we can echo.
            //          let len = data.len();
            //          (len, Try to send data) -- HARD to do zero copy echo in one closure.
            //     }
            //     (0, ())
            // });
            
            // easier: peek, try send, if sent -> data received.
            // But TcpSocket doesn't allow easy "peek and remove conditionally on send".
            // We'll allocate a temp buffer.
            let mut buf = [0u8; 1024];
            match socket.recv_slice(&mut buf) {
                Ok(len) if len > 0 => {
                     serial_println!("[TCP] Recv {} bytes", len);
                     if socket.may_send() {
                         match socket.send_slice(&buf[..len]) {
                             Ok(_) => {},
                             Err(e) => { serial_println!("[TCP] Echo failed: {:?}", e); },
                         }
                     }
                }
                _ => {}
            }
        } else if socket.state() == tcp::State::Closed {
            // If closed, listen again
            socket.listen(80).ok();
        }

        // 4. Handle P2P Socket (Debug State)
        let socket = self.sockets.get_mut::<TcpSocket>(self.p2p_handle);
        let p2p_state = socket.state();
        if p2p_state != tcp::State::Listen && p2p_state != tcp::State::Closed {
             serial_println!("[NET STACK] P2P Socket State: {:?}", p2p_state);
        }

        // 5. Periodic Heartbeat to Gateway (helps SLIRP find us)
        static LAST_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
        let now_ms = timestamp.total_millis() as u64;
        let last = LAST_HEARTBEAT.load(Ordering::Relaxed);
        if now_ms > last && now_ms - last > 5000 {
            LAST_HEARTBEAT.store(now_ms, Ordering::Relaxed);
            
            // Only send if we have an IP
            if self.iface.ip_addrs().first().is_some() {
                let socket = self.sockets.get_mut::<UdpSocket>(self.udp_handle);
                if socket.can_send() {
                    let gateway = smoltcp::wire::IpEndpoint::new(smoltcp::wire::IpAddress::v4(10, 0, 2, 2), 12345);
                    serial_println!("[NET STACK] Sending Heartbeat to gateway 10.0.2.2...");
                    socket.send_slice(b"PING", gateway).ok();
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_ip(&self) -> Option<smoltcp::wire::Ipv4Address> {
        self.iface.ipv4_addr()
    }
}

pub fn init(device: VirtioNetDevice, mac: [u8; 6]) {
    let stack = NetworkStack::new(device, mac);
    *NETWORK_STACK.lock() = Some(stack);
    serial_println!("[NET STACK] Network stack initialized");
}

pub fn poll_network(timestamp: Instant) {
    let mut stack_lock = NETWORK_STACK.lock();
    if let Some(ref mut stack) = *stack_lock {
        stack.poll(timestamp);
    } else {
        static ONCE: AtomicU64 = AtomicU64::new(0);
        if ONCE.fetch_add(1, Ordering::Relaxed) % 1000 == 0 {
             serial_println!("[NET ERROR] poll_network called but NETWORK_STACK is None!");
        }
    }
}
