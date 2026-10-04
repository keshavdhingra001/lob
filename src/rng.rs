//! A tiny seeded random number generator (SplitMix64) for tests and the order-flow
//! generator. Hand-written so a seed means the same sequence forever: a crate upgrade
//! can't silently change every generated workload.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (the modulo bias is negligible for small `n`).
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0);
        self.next_u64() % n
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    /// True with probability `percent / 100`.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn known_first_value() {
        // SplitMix64's published first output for seed 0. If this changes, every
        // recorded workload and replay digest changes with it.
        assert_eq!(Rng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn range_is_inclusive_and_bounded() {
        let mut rng = Rng::new(1);
        let mut seen = [false; 5];
        for _ in 0..1000 {
            let x = rng.range(-2, 2);
            assert!((-2..=2).contains(&x));
            seen[(x + 2) as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }
}
