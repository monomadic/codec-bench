//! Timing summaries. The runner never judges: it summarises, and the report compares.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

impl Summary {
    pub fn of(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut s: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
        if s.is_empty() {
            return None;
        }
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
        Some(Self {
            count: s.len(),
            mean: s.iter().sum::<f64>() / s.len() as f64,
            min: s[0],
            p50: pct(0.5),
            p95: pct(0.95),
            max: s[s.len() - 1],
        })
    }
}

/// A small deterministic generator for seek targets (SplitMix64), so every backend seeks to the
/// same frames for the same seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A target in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_percentiles() {
        let s = Summary::of(&[5.0, 1.0, 3.0, 2.0, 4.0]).unwrap();
        assert_eq!((s.min, s.p50, s.max, s.count), (1.0, 3.0, 5.0, 5));
        assert!(Summary::of(&[]).is_none());
    }

    #[test]
    fn rng_is_deterministic() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert_eq!(a.below(1000), b.below(1000));
    }
}
