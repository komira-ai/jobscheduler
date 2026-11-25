//! The booked CPU curve of a shaped class: what a job of the class books at each 2 s slot
//! of its age, the four checks that let a class book it, and what a running job of the
//! class is still charged at each slot ahead of it.
//!
//! The curve is read from a [`Profile`] in sixteenths of a core and booked in millicores.
//! All arithmetic is integer, so every architecture agrees.

use crate::profile::{CHECK_RUNS, MIN_SLOT_RUNS, Profile, SLOT_SECS, START_SLOTS};
use std::collections::VecDeque;

/// The slots of a curve: the start slots of 2 s, then the slot from 128 s on.
pub const CURVE_SLOTS: usize = START_SLOTS + 1;
/// The slot from 128 s of age on.
pub const TAIL_SLOT: usize = START_SLOTS;
/// Check 3: at most this many of the last 16 scored runs overran their predicted peak.
pub const MAX_OVERRUNS: usize = 3;
/// Check 4: at least this many of the last 16 runs carried a usable CPU series.
pub const MIN_USABLE_SERIES: usize = 12;

/// Milliseconds of one slot.
const SLOT_MS: u64 = SLOT_SECS as u64 * 1000;

/// The slot a job `age_ms` old is in: its age over 2 s, and [`TAIL_SLOT`] from 128 s on.
pub fn age_slot(age_ms: u64) -> usize {
    usize::try_from(age_ms / SLOT_MS).map_or(TAIL_SLOT, |s| s.min(TAIL_SLOT))
}

/// Whether each of a class's last [`CHECK_RUNS`] runs carried a usable CPU series (one
/// that was learned or that the profile chose not to learn, as opposed to one that was
/// missing or disagreed with the run's CPU time). It is check 4's window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesWindow {
    runs: VecDeque<bool>,
}

impl SeriesWindow {
    /// Records one run; the window keeps the last [`CHECK_RUNS`].
    pub fn push(&mut self, usable: bool) {
        self.runs.push_back(usable);
        if self.runs.len() > CHECK_RUNS {
            self.runs.pop_front();
        }
    }

    /// Runs in the window whose series was usable.
    pub fn usable(&self) -> usize {
        self.runs.iter().filter(|&&u| u).count()
    }

    /// Runs in the window.
    pub fn runs(&self) -> usize {
        self.runs.len()
    }
}

/// The four checks a class passes to book its curve; while any fails it books flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checks {
    /// Check 1: the profile scored [`CHECK_RUNS`] runs.
    pub scored: bool,
    /// Check 2: the profile's own shape rule holds (the curve's error is at most
    /// [`crate::FIT_PERMILLE`] per mille of flat's).
    pub shaped: bool,
    /// Check 3: at most [`MAX_OVERRUNS`] of the scored runs overran their predicted peak.
    pub calibrated: bool,
    /// Check 4: at least [`MIN_USABLE_SERIES`] of the last runs carried a usable series.
    pub usable: bool,
}

impl Checks {
    /// The four checks of `profile` and its `series` window.
    pub fn of(profile: &Profile, series: &SeriesWindow) -> Self {
        Self {
            scored: profile.window().runs == CHECK_RUNS,
            shaped: profile.shaped(),
            calibrated: profile.overruns() <= MAX_OVERRUNS,
            usable: series.usable() >= MIN_USABLE_SERIES,
        }
    }

    /// Whether all four hold.
    pub fn pass(self) -> bool {
        self.scored && self.shaped && self.calibrated && self.usable
    }
}

/// The bounds every slot of a curve is held to, in millicores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clamp {
    /// The least a slot books.
    pub least_millis: u64,
    /// The most a slot books (a share of the host).
    pub most_millis: u64,
}

impl Clamp {
    /// `min(max(x, least), most)`.
    fn hold(self, x: u64) -> u64 {
        x.max(self.least_millis).min(self.most_millis)
    }
}

/// Sixteenths of a core as millicores, rounded up: a booking never rounds below the use.
fn millicores(sixteenths: u32) -> u64 {
    (u64::from(sixteenths) * 1000).div_ceil(16)
}

/// What a class books at each slot of a job's age, and how many kept runs reached each
/// slot. A slot few runs reached books the class's flat number: a few runs say nothing
/// of a 90th percentile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curve {
    booking: Vec<u64>,
    reach: Vec<usize>,
}

impl Curve {
    /// The curve of `profile` at `at_ms`. A start slot books the load-weighted 90th
    /// percentile of its runs, and [`TAIL_SLOT`] the mean past 128 s, each held to
    /// `clamp`, while at least [`MIN_SLOT_RUNS`] kept runs reached it; else it books
    /// `flat_millis` held to `clamp`.
    pub fn of(profile: &Profile, flat_millis: u64, clamp: Clamp, at_ms: u64) -> Self {
        let flat = clamp.hold(flat_millis);
        let mut booking = Vec::with_capacity(CURVE_SLOTS);
        let mut reach = Vec::with_capacity(CURVE_SLOTS);
        for s in 0..START_SLOTS {
            let runs = profile.slot_runs(s);
            let own = profile.slot_p90(s, at_ms).filter(|_| runs >= MIN_SLOT_RUNS);
            booking.push(own.map_or(flat, |v| clamp.hold(millicores(v))));
            reach.push(runs);
        }
        let runs = profile.tail_runs();
        let own = profile.tail(at_ms).filter(|_| runs >= MIN_SLOT_RUNS);
        booking.push(own.map_or(flat, |v| clamp.hold(millicores(v))));
        reach.push(runs);
        Self { booking, reach }
    }

    /// What a job books at `slot`; a slot past [`TAIL_SLOT`] reads [`TAIL_SLOT`].
    pub fn booking(&self, slot: usize) -> u64 {
        self.booking[slot.min(TAIL_SLOT)]
    }

    /// Kept runs that reached `slot` (for [`TAIL_SLOT`], that lasted past 128 s); a slot
    /// past it reads [`TAIL_SLOT`].
    pub fn reach(&self, slot: usize) -> usize {
        self.reach[slot.min(TAIL_SLOT)]
    }

    /// The largest booking over all slots.
    pub fn peak(&self) -> u64 {
        self.booking.iter().copied().max().unwrap_or(0)
    }

    /// What a running job of this curve, `age_ms` old, is charged `k` slots ahead
    /// (`k == TAIL_SLOT` and more mean from 128 s on). A job in the tail books the tail. A
    /// job whose own slot fewer than [`MIN_SLOT_RUNS`] kept runs reached has outlived the
    /// recorded runs and books `flat_millis` throughout. Otherwise the booking of the slot
    /// it will be in is weighted by survival, the share of the runs that reached the
    /// job's slot that also reached that one, since the others had ended by then; a slot
    /// no run reached charges 0. Products are rounded up.
    pub fn charge(&self, age_ms: u64, k: usize, flat_millis: u64) -> u64 {
        let i = age_slot(age_ms);
        if i == TAIL_SLOT {
            return self.booking(TAIL_SLOT);
        }
        let own = self.reach(i);
        if own < MIN_SLOT_RUNS {
            return flat_millis;
        }
        let ahead = i + k;
        let slot = if k < TAIL_SLOT && ahead < TAIL_SLOT {
            ahead
        } else {
            TAIL_SLOT
        };
        let product = u128::from(self.booking(slot)) * self.reach(slot) as u128;
        u64::try_from(product.div_ceil(own as u128)).unwrap_or(u64::MAX)
    }
}
