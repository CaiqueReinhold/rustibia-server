use once_cell::sync::Lazy;
use rand::{RngExt, SeedableRng, rngs::Xoshiro256PlusPlus, seq::IndexedRandom};
use rand_distr::{
    Distribution, Normal,
    weighted::{Error, WeightedIndex},
};

use crate::constants::items::MAX_DROP_CHANCE;

static DAMAGE_CURVE: Lazy<Normal<f32>> = Lazy::new(|| Normal::new(0.5, 0.25).unwrap());

#[derive(Debug)]
pub struct Rolls {
    rng: Xoshiro256PlusPlus,
}

impl Rolls {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Xoshiro256PlusPlus::seed_from_u64(seed),
        }
    }

    /// An independent, reproducible stream for one (tick, entity) pair.
    pub fn stream(world_seed: u64, tick: u64, id: u64) -> Self {
        Self::new(mix64(mix64(world_seed ^ tick) ^ id))
    }

    pub fn damage_roll(&mut self, min: u32, max: u32) -> u32 {
        let (a, b) = (min.min(max), min.max(max));
        let v = loop {
            let v = DAMAGE_CURVE.sample(&mut self.rng);
            if (0.0..=1.0).contains(&v) {
                break v;
            }
        };
        a + (v * (b - a) as f32).round() as u32
    }

    pub fn uniform(&mut self, min: u32, max: u32) -> u32 {
        let (a, b) = (min.min(max), min.max(max));
        self.rng.random_range(a..=b)
    }

    pub fn category_roll<'a, T>(&mut self, choices: &'a [T]) -> Option<&'a T> {
        choices.choose(&mut self.rng)
    }

    pub fn chance(&mut self, chance: u32) -> bool {
        self.uniform(0, MAX_DROP_CHANCE - 1) < chance
    }

    // loot rate is expressed as integer percent value, e.g. 100 for regular 100% chance.
    pub fn drop_rolls(&mut self, loot_rate: u32) -> u32 {
        let scaled = loot_rate * MAX_DROP_CHANCE / 100;
        scaled / MAX_DROP_CHANCE
            + if self.uniform(0, MAX_DROP_CHANCE) < (scaled % MAX_DROP_CHANCE) {
                1
            } else {
                0
            }
    }

    pub fn weighted(&mut self, weights: &[u64]) -> Result<usize, Error> {
        let dist = WeightedIndex::new(weights)?;
        Ok(dist.sample(&mut self.rng))
    }
}

/// SplitMix64 finalizer: diffuses a low-entropy integer across all 64 bits.
const fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}
