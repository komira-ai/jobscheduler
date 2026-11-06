//! The bits the server's scaler draws its acquire ids from, and so the names of the
//! machines it launches: from the seed, so a seed launches the same machines under the
//! same names on every run.
//!
//! The scaler draws one value of 32 bytes per tick, launch or not, through the server's
//! seam (`Config::with_entropy`), from a [`SessionSource`]. The cell's is a counter mixed
//! by SplitMix64 from the seed, as the job store's ids are (module `server::ids`), and
//! apart from the kernel's random source, so drawing from it moves no other decision. The
//! server node keeps one source across a restart of the server, so a restarted scaler goes
//! on with it rather than naming its next machine as its first.

use std::sync::atomic::{AtomicU64, Ordering};

use js_registry::SessionSource;

/// The bytes of one value the scaler draws (the registry's secret length).
const SECRET_BYTES: usize = 32;

/// The scaler's entropy of a cell of one seed.
#[derive(Debug)]
pub struct SeededEntropy {
    state: AtomicU64,
}

impl SeededEntropy {
    /// The values a cell of `seed` draws, in the order it draws them.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }

    /// The next 64 bits.
    fn next(&self) -> u64 {
        let state = self.state.fetch_add(GAMMA, Ordering::SeqCst);
        mix(state.wrapping_add(GAMMA))
    }
}

impl SessionSource for SeededEntropy {
    fn fresh(&self) -> Option<[u8; SECRET_BYTES]> {
        let mut bytes = [0; SECRET_BYTES];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_be_bytes()[..chunk.len()]);
        }
        Some(bytes)
    }
}

/// SplitMix64's increment.
const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// SplitMix64's mix of one counter value.
fn mix(state: u64) -> u64 {
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: entropy that does not follow its seed (two seeds alike, or one seed unlike
    /// itself), and a source that repeats a value, which the scaler refuses as a repeated
    /// acquire id.
    #[test]
    fn the_scalers_entropy_is_a_function_of_the_seed() {
        let draw = |seed| {
            let source = SeededEntropy::new(seed);
            (0..64).filter_map(|_| source.fresh()).collect::<Vec<_>>()
        };
        let (one, again, two) = (draw(3), draw(3), draw(4));
        assert_eq!(one, again);
        assert_ne!(one[0], two[0]);
        let distinct: std::collections::BTreeSet<_> = one.iter().collect();
        assert_eq!(distinct.len(), one.len());
        assert_ne!(
            one[0][..8],
            one[0][8..16],
            "each word of a value is drawn anew"
        );
    }
}
