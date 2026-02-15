use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use sha2::{Sha256, Digest};
use core::fmt;

// Kademlia Configuration
pub const K_BUCKET_SIZE: usize = 20;
pub const ID_SIZE: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; ID_SIZE]);

impl NodeId {
    pub fn new(bytes: [u8; ID_SIZE]) -> Self {
        NodeId(bytes)
    }

    pub fn from_data(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        let mut bytes = [0u8; ID_SIZE];
        bytes.copy_from_slice(&result);
        NodeId(bytes)
    }

    pub fn distance(&self, other: &NodeId) -> NodeId {
        let mut res = [0u8; ID_SIZE];
        for i in 0..ID_SIZE {
            res[i] = self.0[i] ^ other.0[i];
        }
        NodeId(res)
    }

    pub fn leading_zeros(&self) -> u32 {
        let mut zeros = 0;
        for byte in self.0.iter() {
            if *byte == 0 {
                zeros += 8;
            } else {
                zeros += byte.leading_zeros();
                break;
            }
        }
        zeros
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId(")?;
        for b in &self.0[0..4] {
            write!(f, "{:02x}", b)?;
        }
        write!(f, "...)")
    }
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub node_id: NodeId,
    pub peer_id_str: String,
    // Add socket addr later if needed
}

pub struct KBucket {
    pub peers: Vec<PeerInfo>,
}

impl KBucket {
    pub fn new() -> Self {
        KBucket {
            peers: Vec::with_capacity(K_BUCKET_SIZE),
        }
    }

    pub fn add(&mut self, peer: PeerInfo) -> bool {
        if let Some(idx) = self.peers.iter().position(|p| p.node_id == peer.node_id) {
            // Move to tail (most recently seen)
            self.peers.remove(idx);
            self.peers.push(peer);
            true
        } else if self.peers.len() < K_BUCKET_SIZE {
            self.peers.push(peer);
            true
        } else {
            // Bucket full - ideally disable/ping least recently seen.
            // For now, minimal implementation: drop new peer
            false
        }
    }
}

pub struct RoutingTable {
    pub local_id: NodeId,
    pub buckets: Vec<KBucket>, // Index corresponds to common prefix length
}

impl RoutingTable {
    pub fn new(local_id: NodeId) -> Self {
        let mut buckets = Vec::with_capacity(ID_SIZE * 8);
        for _ in 0..(ID_SIZE * 8) {
            buckets.push(KBucket::new());
        }
        RoutingTable {
            local_id,
            buckets,
        }
    }

    pub fn add_peer(&mut self, peer: PeerInfo) {
        let dist = self.local_id.distance(&peer.node_id);
        let bucket_idx = self.get_bucket_index(&dist);
        
        if let Some(bucket) = self.buckets.get_mut(bucket_idx) {
            bucket.add(peer);
        }
    }
    
    fn get_bucket_index(&self, distance: &NodeId) -> usize {
        // Distance 0 (self) -> last bucket? or separate handling.
        // Kademlia: index = matches shared prefix length?
        // Actually XOR distance -> take leading zeros.
        // Distance 0 -> same node.
        if distance.0.iter().all(|&b| b == 0) {
            return 0; // Self
        }
        
        let zeros = distance.leading_zeros() as usize;
        // Cap strictly
        if zeros >= self.buckets.len() {
            self.buckets.len() - 1
        } else {
            zeros
        }
    }
    
    pub fn find_closest(&self, target: &NodeId, count: usize) -> Vec<PeerInfo> {
        let mut closest = Vec::new();
        // Naive iteration for now (no efficient bucket hopping yet)
        // Collect all peers and sort by distance
        for bucket in &self.buckets {
            for peer in &bucket.peers {
                closest.push(peer.clone());
            }
        }
        
        closest.sort_by(|a, b| {
            let dist_a = a.node_id.distance(target);
            let dist_b = b.node_id.distance(target);
            dist_a.cmp(&dist_b)
        });
        
        closest.truncate(count);
        closest
    }
}
