//! A small deterministic PRNG.
//!
//! Deliberately not `rand`: every random choice this system makes (which candidate to
//! sample, how much jitter to add to a poll) needs to be replayable from the ledger, so
//! the generator is seeded and reproducible rather than good.

/// SplitMix64. Fast, seedable, adequate for scheduling jitter and sampling.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }

    /// Seed from a string, so a run keyed by an opportunity id is reproducible.
    pub fn from_key(key: &str) -> Self {
        let hex = crate::sha256_hex(key.as_bytes());
        let mut bytes = [0u8; 8];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
        }
        Self::new(u64::from_le_bytes(bytes))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`. Returns 0 for `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u64() % n
    }

    /// Uniform in `[0.0, 1.0)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Jitter a base interval by +/- `frac`, so many watchers do not stampede a venue
    /// on the same second.
    pub fn jitter_ms(&mut self, base_ms: u64, frac: f64) -> u64 {
        let f = frac.clamp(0.0, 1.0);
        let span = (base_ms as f64 * f) as i64;
        if span == 0 {
            return base_ms;
        }
        let delta = self.below((span * 2 + 1) as u64) as i64 - span;
        (base_ms as i64 + delta).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_from_same_seed() {
        let a: Vec<u64> = (0..8).map(|_| Rng::new(42).next_u64()).collect();
        let mut r = Rng::new(42);
        assert!(a.iter().all(|&x| x == a[0]));
        let seq: Vec<u64> = (0..8).map(|_| r.next_u64()).collect();
        let mut r2 = Rng::new(42);
        let seq2: Vec<u64> = (0..8).map(|_| r2.next_u64()).collect();
        assert_eq!(seq, seq2);
        assert!(seq.windows(2).any(|w| w[0] != w[1]), "must actually vary");
    }

    #[test]
    fn key_seeding_is_stable() {
        assert_eq!(Rng::from_key("o-1").next_u64(), Rng::from_key("o-1").next_u64());
        assert_ne!(Rng::from_key("o-1").next_u64(), Rng::from_key("o-2").next_u64());
    }

    #[test]
    fn jitter_stays_in_band() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            let v = r.jitter_ms(1000, 0.2);
            assert!((800..=1200).contains(&v), "jitter escaped band: {v}");
        }
    }

    #[test]
    fn unit_is_in_range() {
        let mut r = Rng::new(1);
        for _ in 0..1000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }
}
