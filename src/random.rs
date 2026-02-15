use getrandom::{register_custom_getrandom, Error};
use core::sync::atomic::{AtomicU64, Ordering};

// Simple Xorshift RNG for PoC (NOT SECURE)
static RHS_SEED: AtomicU64 = AtomicU64::new(0xCAFEBABE);

fn next_u64() -> u64 {
    let mut x = RHS_SEED.load(Ordering::Relaxed);
    if x == 0 {
        x = 0xCAFEBABE; // Avoid zero seed lock
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RHS_SEED.store(x, Ordering::Relaxed);
    x
}

pub fn custom_getrandom(buf: &mut [u8]) -> Result<(), Error> {
    for chunk in buf.chunks_mut(8) {
        let rand = next_u64();
        let bytes = rand.to_le_bytes();
        let len = chunk.len();
        chunk.copy_from_slice(&bytes[..len]);
    }
    Ok(())
}

register_custom_getrandom!(custom_getrandom);
