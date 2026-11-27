//! The memory curve of a class: what a job of the class would hold at each 2 s slot of its age,
//! with the usual memory margin, and what a running job of the class is still projected to hold
//! at each slot ahead of it. Nothing books it: the job service reads it in shadow, to say where a
//! booking that follows it would have placed a job differently from the flat memory booking
//! (`docs/design/placement-profiles-live.md`, "Memory over time, in shadow").
//!
//! The curve is read from a memory [`Profile`] (MiB, each slot the largest second) and is in
//! bytes. All arithmetic is integer, so every architecture agrees.

use crate::MIB;
use crate::curve::{CURVE_SLOTS, TAIL_SLOT, age_slot};
use crate::profile::{CHECK_RUNS, MIN_SLOT_RUNS, Profile};
use crate::summary::sized;

/// What a class would hold at each slot of a job's age, and how many kept runs reached each slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCurve {
    booking: Vec<u64>,
    reach: Vec<usize>,
    flat_bytes: u64,
}

impl MemoryCurve {
    /// The memory curve of `profile` at `at_ms` for a class whose flat memory booking is
    /// `flat_bytes`; `None` while the profile has scored fewer than [`CHECK_RUNS`] runs (a class
    /// with too little history books flat). A start slot holds the load-weighted 90th percentile
    /// of its runs' largest second ([`Profile::slot_p90`]) and [`TAIL_SLOT`] that of their peaks
    /// after 128 s ([`Profile::tail_p90`]), each with the margin of the flat memory booking
    /// (`1.2 x p90 + 128 MiB`), while at least [`MIN_SLOT_RUNS`] kept runs reached it; else, and
    /// never above, `flat_bytes`.
    #[must_use]
    pub fn of(profile: &Profile, flat_bytes: u64, at_ms: u64) -> Option<Self> {
        if profile.window().runs < CHECK_RUNS {
            return None;
        }
        let held = |own: Option<u32>, runs: usize| {
            let own = own.filter(|_| runs >= MIN_SLOT_RUNS);
            own.map_or(flat_bytes, |mib| {
                sized(u64::from(mib) * MIB).min(flat_bytes)
            })
        };
        let mut booking = Vec::with_capacity(CURVE_SLOTS);
        let mut reach = Vec::with_capacity(CURVE_SLOTS);
        for (slot, own) in profile.slot_p90s(at_ms).into_iter().enumerate() {
            let runs = profile.slot_runs(slot);
            booking.push(held(own, runs));
            reach.push(runs);
        }
        let runs = profile.tail_runs();
        booking.push(held(profile.tail_p90(at_ms), runs));
        reach.push(runs);
        Some(Self {
            booking,
            reach,
            flat_bytes,
        })
    }

    /// What a job of the class would hold at `slot` of its age; a slot past [`TAIL_SLOT`] reads
    /// [`TAIL_SLOT`].
    #[must_use]
    pub fn booking(&self, slot: usize) -> u64 {
        self.booking[slot.min(TAIL_SLOT)]
    }

    /// What a running job of this curve, `age_ms` old, is projected to hold `k` slots ahead
    /// (`k == TAIL_SLOT` and more mean from 128 s on). A job in the tail holds the tail at every
    /// slot. A job whose own slot fewer than [`MIN_SLOT_RUNS`] kept runs reached has outlived the
    /// recorded runs and holds the flat booking at every slot. Otherwise it holds the booking of
    /// the slot it will be in, and nothing at a slot no kept run reached: every run of the class
    /// had ended by then. Unlike a CPU charge, it is not weighted by the share of runs still
    /// running: memory a job may still hold is held in full.
    #[must_use]
    pub fn charge(&self, age_ms: u64, k: usize) -> u64 {
        let own = age_slot(age_ms);
        if own == TAIL_SLOT {
            return self.booking(TAIL_SLOT);
        }
        if self.reach[own] < MIN_SLOT_RUNS {
            return self.flat_bytes;
        }
        let slot = own.saturating_add(k).min(TAIL_SLOT);
        if self.reach[slot] == 0 {
            0
        } else {
            self.booking(slot)
        }
    }
}
