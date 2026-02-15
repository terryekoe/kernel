use crate::serial_println;
use crate::p2p_transport;
use crate::p2p_kademlia::{self, NodeId, RoutingTable, PeerInfo};
use crate::EXECUTOR;
use crate::executor::Task;
use crate::net_stack::NETWORK_STACK;
use ed25519_dalek::SigningKey;
use alloc::vec::Vec;
use alloc::string::String;
use spin::Mutex;
use lazy_static::lazy_static;

pub struct P2PState {
    pub peer_id: String,
    pub node_id: NodeId,
    pub routing_table: RoutingTable,
}

lazy_static! {
    pub static ref P2P_STATE: Mutex<Option<P2PState>> = Mutex::new(None);
}

pub fn init() {
    serial_println!("[P2P] Initializing P2P Stack (Modified Kademlia)...");
    
    // 1. Generate Identity
    serial_println!("[P2P] Step 1: Getting Randomness...");
    let mut key_bytes = [0u8; 32];
    getrandom::getrandom(&mut key_bytes).expect("RNG failed");
    
    serial_println!("[P2P] Step 2: Generating Keypair...");
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let verifying_key = signing_key.verifying_key();
    
    // PeerId derivation
    serial_println!("[P2P] Step 3: Deriving PeerID...");
    let mut pub_key_proto = Vec::with_capacity(36);
    pub_key_proto.push(0x08); pub_key_proto.push(0x01);
    pub_key_proto.push(0x12); pub_key_proto.push(0x20);
    pub_key_proto.extend_from_slice(verifying_key.as_bytes());
    
    let mut multihash = Vec::with_capacity(2 + 36);
    multihash.push(0x00); multihash.push(36);
    multihash.extend_from_slice(&pub_key_proto);
    
    let peer_id_str = bs58::encode(&multihash).into_string();
    
    // Generate NodeID (SHA256 of PeerID/PublicKey)
    serial_println!("[P2P] Step 4: Generating NodeID (SHA256)...");
    let node_id = NodeId::from_data(&multihash);

    serial_println!("[P2P] Identity: {:?} NodeId: {:?}", peer_id_str, node_id);
    
    serial_println!("[P2P] Step 5: Initializing Global State...");
    *P2P_STATE.lock() = Some(P2PState { 
        peer_id: peer_id_str,
        node_id,
        routing_table: RoutingTable::new(node_id),
    });
    serial_println!("[P2P] State initialized.");
    
    // 2. Spawn P2P Listener Task
    serial_println!("[P2P] Step 6: Spawning Listener...");
    EXECUTOR.lock().spawn(Task::new(p2p_listen_task()));
}

use core::task::{Context, Poll};
use core::future::Future;

struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            // Wake immediately so we get polled again next cycle
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

pub fn yield_now() -> impl Future<Output = ()> {
    YieldNow { yielded: false }
}

async fn p2p_listen_task() {
    serial_println!("[P2P] Starting listener task...");
    
    loop {
        // serial_println!("[P2P] Listener loop tick");
        let mut handle_opt = None;
        {
            let mut stack = NETWORK_STACK.lock();
            if let Some(ref mut stack_inner) = *stack {
                let socket = stack_inner.sockets.get_mut::<smoltcp::socket::tcp::Socket>(stack_inner.p2p_handle);
                
                let state = socket.state();
                if state == smoltcp::socket::tcp::State::Established || state == smoltcp::socket::tcp::State::CloseWait {
                     serial_println!("[P2P] Socket active! State: {:?}", state);
                     handle_opt = Some(stack_inner.p2p_handle);
                } else if state == smoltcp::socket::tcp::State::Closed {
                    // serial_println!("[P2P] Socket closed, re-listening...");
                    socket.listen(40444).ok();
                }
            }
        }
        
        if let Some(handle) = handle_opt {
            serial_println!("[P2P] New connection detected! Exchanging handshakes...");
            match handshake(handle).await {
                Ok(_) => { serial_println!("[P2P] Handshake success!"); }
                Err(_) => { serial_println!("[P2P] Handshake failed or connection closed."); }
            }
            // After handshake, close or keep open. For now, we simple echo/close.
            {
                let mut stack = NETWORK_STACK.lock();
                if let Some(ref mut stack_inner) = *stack {
                    let socket = stack_inner.sockets.get_mut::<smoltcp::socket::tcp::Socket>(stack_inner.p2p_handle);
                    socket.close();
                }
            }
        }
        
        // Yield proper
        yield_now().await;
    }
}

async fn handshake(handle: smoltcp::iface::SocketHandle) -> Result<(), ()> {
    // 1. Send our PeerID and NodeID
    let (my_peer_id, my_node_id) = {
        let state = P2P_STATE.lock();
        let s = state.as_ref().unwrap();
        (s.peer_id.clone(), s.node_id.clone())
    };
    
    // Serialization: [PeerID Len (4)] [PeerID Bytes] [NodeID (32)]
    let peer_id_bytes: &[u8] = my_peer_id.as_bytes();
    let mut payload = Vec::with_capacity(4 + peer_id_bytes.len() + 32);
    payload.extend_from_slice(&(peer_id_bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(peer_id_bytes);
    payload.extend_from_slice(&my_node_id.0);
    
    p2p_transport::send_framed(handle, &payload).await?;
    serial_println!("[P2P] Sent Identity (PeerID + NodeID)");
    
    // 2. Recv their Identity
    let payload = p2p_transport::recv_framed(handle).await?;
    if payload.len() < 36 { return Err(()); } // Min 4(len) + 0(id) + 32(node)
    
    let len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if payload.len() < 4 + len + 32 { return Err(()); }
    
    let remote_peer_id = String::from_utf8_lossy(&payload[4..4+len]).into_owned();
    let mut node_id_bytes = [0u8; 32];
    node_id_bytes.copy_from_slice(&payload[4+len..4+len+32]);
    let remote_node_id = NodeId::new(node_id_bytes);
    
    serial_println!("[P2P] Handshake verified. Remote PeerID: {} NodeID: {:?}", remote_peer_id, remote_node_id);
    
    // 3. Add to Routing Table
    {
        let mut state_lock = P2P_STATE.lock();
        if let Some(state) = state_lock.as_mut() {
            let peer_info = PeerInfo {
                node_id: remote_node_id,
                peer_id_str: remote_peer_id,
            };
            state.routing_table.add_peer(peer_info);
            serial_println!("[P2P] Added peer to Kademlia Routing Table.");
        }
    }
    
    Ok(())
}
