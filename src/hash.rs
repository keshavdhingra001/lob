//! A cheap hasher for the fast book's order-id index (D31).
//!
//! The standard `HashMap` uses SipHash-1-3 with a random key: strong against attackers
//! who choose keys to collide, but slow for a one-`u64` key. Here the key is one `u64`, so
//! MurmurHash3's 64-bit finalizer ("fmix64") is enough: three xor-shifts and two multiplies.
//! Every input bit affects every output bit. That matters because hashbrown picks the bucket
//! from the low bits and a per-slot tag from the top 7.
//!
//! A single 128-bit "folded" multiply was tried first and rejected: ids that differ only in
//! their high bits, such as multiples of 2^40, hit only 1,755 of 4,096 buckets, against
//! about 2,590 for a random hash. The tests check those patterns.
//!
//! The constants are fixed, so the table layout and its performance are the same on every
//! run. The output never depends on them either way, because the index is never iterated (D21).

use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

/// Plug into `HashMap<K, V, IdBuildHasher>`.
pub type IdBuildHasher = BuildHasherDefault<IdHasher>;

/// MurmurHash3's fmix64.
fn fmix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

impl Hasher for IdHasher {
    /// `OrderId` hashes as one `u64`, so this is the only path the index takes.
    fn write_u64(&mut self, x: u64) {
        self.0 = fmix64(self.0 ^ x);
    }

    /// Any other key type: mix it in 8 bytes at a time. Correct, not tuned.
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OrderId;
    use std::collections::HashSet;
    use std::hash::BuildHasher;

    fn hash(id: u64) -> u64 {
        IdBuildHasher::default().hash_one(OrderId(id))
    }

    /// How many of the ids land in distinct buckets of a 4096-bucket table, and how many
    /// distinct 7-bit tags they get. Random hashing puts 4096 keys in about 2,590 buckets.
    fn spread(ids: impl Iterator<Item = u64>) -> (usize, usize) {
        let hashes: Vec<u64> = ids.map(hash).collect();
        let buckets: HashSet<u64> = hashes.iter().map(|h| h & 4095).collect();
        let tags: HashSet<u64> = hashes.iter().map(|h| h >> 57).collect();
        (buckets.len(), tags.len())
    }

    type Pattern = (&'static str, fn(u64) -> u64);

    /// Sequential ids, strided ids, and ids that differ only in high bits (where a plain
    /// or folded multiply clusters) all spread like random.
    #[test]
    fn structured_ids_spread_like_random() {
        let patterns: [Pattern; 7] = [
            ("sequential", |i| i),
            ("stride 1000", |i| i * 1000),
            ("stride 4096 + 7", |i| i * 4096 + 7),
            ("i << 20", |i| i << 20),
            ("i << 32", |i| i << 32),
            ("i << 40", |i| i << 40),
            // Without fmix64's last xor-shift, bits above 44 never reach the bucket bits:
            // these 4096 ids would all share one bucket.
            ("i << 50", |i| i << 50),
        ];
        for (name, id) in patterns {
            let (buckets, tags) = spread((1..=4096).map(id));
            assert!(buckets > 2_450, "{name}: {buckets} buckets");
            assert_eq!(tags, 128, "{name}");
        }
    }

    /// Avalanche: flipping any one input bit flips each bucket bit (0..12) and tag bit
    /// (57..64) about half the time. Bucket counts alone can't see fmix64's last xor-shift
    /// (an odd multiply only permutes the low bits), but without it some output bit flips
    /// with an input bit 98% of the time.
    #[test]
    fn every_input_bit_avalanches_into_bucket_and_tag_bits() {
        let mut rng = crate::rng::Rng::new(1);
        let keys: Vec<u64> = (0..1000).map(|_| rng.next_u64()).collect();
        let mut worst: f64 = 0.5;
        for in_bit in 0..64 {
            for out_bit in (0..12).chain(57..64) {
                let flips = keys
                    .iter()
                    .filter(|&&k| (fmix64(k) ^ fmix64(k ^ (1 << in_bit))) >> out_bit & 1 == 1)
                    .count();
                let rate = flips as f64 / keys.len() as f64;
                if (rate - 0.5).abs() > (worst - 0.5).abs() {
                    worst = rate;
                }
            }
        }
        // 1,216 rates from 1,000 keys each: about 0.05 off is normal, 0.1 is not.
        assert!((0.4..=0.6).contains(&worst), "worst flip rate {worst}");
    }

    #[test]
    fn deterministic_and_order_sensitive() {
        assert_eq!(hash(42), hash(42));
        assert_ne!(hash(1), hash(2));
        let mut a = IdHasher::default();
        a.write(b"ab");
        let mut b = IdHasher::default();
        b.write(b"ba");
        assert_ne!(a.finish(), b.finish());
    }
}
