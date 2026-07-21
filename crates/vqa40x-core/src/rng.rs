//! Dependency-free seeding helpers shared by the noise engine and
//! [`crate::options::random_serial`].

use std::sync::atomic::{AtomicU64, Ordering};

/// SplitMix64 finalizer: bijective, well-mixed, good enough to decorrelate
/// nearby seeds (0, 1, 2, …) into unrelated xorshift states.
pub fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A fresh seed per call: wall clock + PID + a process-global counter, mixed
/// through [`splitmix64`]. The counter keeps two calls in the same clock tick
/// (e.g. several devices starting streams at once) from colliding.
pub fn entropy_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32
        ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(24);
    splitmix64(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_seeds_are_distinct() {
        assert_ne!(entropy_seed(), entropy_seed());
    }

    #[test]
    fn splitmix64_decorrelates_and_maps_zero_away() {
        assert_ne!(splitmix64(0), 0);
        assert_ne!(splitmix64(0), splitmix64(1));
    }
}
