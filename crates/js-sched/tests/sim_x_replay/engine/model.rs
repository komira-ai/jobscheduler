//! The pool, the charges and the two seams the replay is written against.
//!
//! * [`Truth`] says what really happens to a run: the cores it burns each second and the
//!   memory it holds. The replay never reads a belief to decide that.
//! * [`Beliefs`] says what the scheduler thinks of a lease when it is asked to place it:
//!   a [`Belief`], a pure function of the trace and the seed (the same for every ranker).
//!
//! Both are keyed by the lease index, the position of the row in the slice handed to the
//! replay, so a hand trace in a test and the committed one go through the same code.
//!
//! A [`HostState`] keeps, for each host, the sums the admission predicate reads. They are
//! updated as leases come, go and age, and [`HostState::rebuild`] computes them again from
//! the residents so a test can hold the two to be equal.

use std::collections::{BTreeMap, BTreeSet};

/// Ticks in a window: the host's truth CPU is read every 5 s.
pub const WINDOW_TICKS: u64 = 5;

/// Ticks in a logical minute.
pub const MINUTE_TICKS: u64 = 60;

/// The predicate looks 30 minutes ahead.
pub const HORIZON: usize = 30;

/// The slot for everything after the horizon.
pub const H_PLUS: usize = HORIZON + 1;

/// Slots: 0 (now, the shipped formulas), 1 to 30 (minutes ahead) and `H+`.
pub const SLOTS: usize = HORIZON + 2;

/// What a scorer evaluates per candidate host: two resources over every slot.
pub const EVALUATIONS_PER_WORKER: u64 = 2 * SLOTS as u64;

/// A charge: cores and memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charge {
    /// Millicores.
    pub cpu_millis: u64,
    /// Memory, MiB.
    pub mem_mib: u64,
}

impl Charge {
    /// A charge of `cpu_millis` and `mem_mib`.
    pub const fn new(cpu_millis: u64, mem_mib: u64) -> Self {
        Self {
            cpu_millis,
            mem_mib,
        }
    }

    /// The sum of two charges.
    #[must_use]
    pub fn plus(self, other: Charge) -> Charge {
        Charge::new(
            self.cpu_millis + other.cpu_millis,
            self.mem_mib + other.mem_mib,
        )
    }

    /// The difference; `other` must be part of `self`.
    #[must_use]
    pub fn minus(self, other: Charge) -> Charge {
        Charge::new(
            self.cpu_millis
                .checked_sub(other.cpu_millis)
                .expect("a charge removed is a charge added"),
            self.mem_mib
                .checked_sub(other.mem_mib)
                .expect("a charge removed is a charge added"),
        )
    }

    /// This charge, never more than a host of `cap` has (as the shipped predicate clamps).
    #[must_use]
    pub fn clamped(self, cap: &Capacity) -> Charge {
        Charge::new(
            self.cpu_millis.min(cap.cpu_millis),
            self.mem_mib.min(cap.mem_mib),
        )
    }
}

/// The charge of a lease nothing is known about: 2 cores and 4 GiB at every slot.
pub const COLD_CHARGE: Charge = Charge::new(2_000, 4_096);

/// The slot's distance in minutes from now; the `H+` slot is read 31 minutes ahead.
pub fn slot_minute(slot: usize) -> u64 {
    slot as u64
}

/// What the scheduler believes about one lease when it is placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Belief {
    /// No (or too thin a) history: the flat cold charge applies and the cold cap counts it.
    pub cold: bool,
    /// Minutes of the life the fit looks at, 1 to 30.
    pub fit_minutes: u8,
    /// The flat charge: what slot 0 and the shipped formulas use.
    pub p_bar: Charge,
    /// The charge at minute 0, 1, ...; the last point is held beyond the end.
    curve: Vec<Charge>,
}

impl Belief {
    /// A belief with a curve. `fit_minutes` is brought into 1 to 30.
    pub fn new(cold: bool, fit_minutes: u8, p_bar: Charge, curve: Vec<Charge>) -> Self {
        Self {
            cold,
            fit_minutes: fit_minutes.clamp(1, HORIZON as u8),
            p_bar,
            curve,
        }
    }

    /// A lease charged `p_bar` at every minute and fitted over one.
    pub fn flat(p_bar: Charge) -> Self {
        Self::new(false, 1, p_bar, Vec::new())
    }

    /// The belief about a key with no history.
    pub fn cold() -> Self {
        Self::new(true, 1, COLD_CHARGE, Vec::new())
    }

    /// The charge `minute` minutes into the lease's life.
    pub fn at(&self, minute: u64) -> Charge {
        let last = self.curve.len().checked_sub(1);
        match last {
            None => self.p_bar,
            Some(last) => {
                let i = usize::try_from(minute).map_or(last, |m| m.min(last));
                self.curve[i]
            }
        }
    }

    /// The charge at `slot` of a lease that is `age` whole minutes old: slot 0 is the flat
    /// charge whatever the age, slot `s` the curve `s` minutes on.
    pub fn at_slot(&self, age: u64, slot: usize) -> Charge {
        if slot == 0 {
            self.p_bar
        } else {
            self.at(age + slot_minute(slot))
        }
    }
}

/// What really happens to a run.
pub trait Truth {
    /// Core-milliseconds the lease burns in the `age_tick`-th second of its life (0 is
    /// the first). The values over a lease's `ceil(wall / 1000)` ticks add up to its
    /// `cpu_ms` exactly.
    fn core_ms(&self, lease: usize, age_tick: u64) -> u64;

    /// MiB the lease holds in that second.
    fn working_set_mib(&self, lease: usize, age_tick: u64) -> u64;
}

/// What the scheduler believes.
pub trait Beliefs {
    /// The belief the scheduler has about `lease` when it considers it. A pure function of
    /// the trace and the seed: the same for every ranker and every wait.
    fn belief(&self, lease: usize) -> &Belief;
}

/// A host's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// `C_cpu`, millicores.
    pub cpu_millis: u64,
    /// `C_mem`, MiB.
    pub mem_mib: u64,
    /// The most leases the host holds.
    pub leases: u32,
}

/// The hosts of a run: all of one size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    /// How many hosts.
    pub hosts: usize,
    /// The size of each.
    pub cap: Capacity,
}

impl Pool {
    /// `hosts` hosts of `threads` threads and `mem_mib` MiB; a lease cap of 4 per thread.
    pub fn new(hosts: usize, threads: u64, mem_mib: u64) -> Self {
        Self {
            hosts,
            cap: Capacity {
                cpu_millis: threads * 1_000,
                mem_mib,
                leases: u32::try_from(threads * 4).expect("a small lease cap"),
            },
        }
    }

    /// The pool the test builds run on: 6 hosts of 16 threads and 45.6 GiB.
    pub fn ci() -> Self {
        Self::new(6, 16, 46_694)
    }

    /// A pool of 6 hosts, 88 threads and 251 GiB each.
    pub fn full() -> Self {
        Self::new(6, 88, 251 * 1_024)
    }
}

/// A lease on a host.
#[derive(Clone, Copy, Debug)]
pub struct Resident<'w> {
    /// The tick it started.
    pub start_tick: u64,
    /// Whole minutes old, as the sums last saw it.
    pub age: u64,
    /// What the scheduler believes of it.
    pub belief: &'w Belief,
}

/// The sums the predicate reads. `slots[0]` is the sum of the flat charges; `slots[s]` the
/// sum of the residents' charges `s` minutes ahead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sums {
    /// By slot.
    pub slots: [Charge; SLOTS],
    /// Flat charges of the residents that have been on the host for a full window.
    pub covered: Charge,
    /// Flat charges of the others.
    pub uncovered: Charge,
}

impl Default for Sums {
    fn default() -> Self {
        Self {
            slots: [Charge::default(); SLOTS],
            covered: Charge::default(),
            uncovered: Charge::default(),
        }
    }
}

/// One host: its residents, the sums over them and what its readings last said.
#[derive(Clone, Debug)]
pub struct HostState<'w> {
    /// Its size.
    pub cap: Capacity,
    /// Marked: no lease is placed on it and nothing is scored for it.
    pub excluded: bool,
    residents: BTreeMap<usize, Resident<'w>>,
    next_age: BTreeSet<(u64, usize)>,
    sums: Sums,
    /// The tick of the latest round: residents placed in it are uncovered.
    last_round: u64,
    /// Core-ms of the last two windows, newest first.
    window_core_ms: [u64; 2],
    /// MiB held, read at the end of the last window.
    working_set_mib: u64,
    cold_this_round: u32,
}

impl<'w> HostState<'w> {
    /// An empty host.
    pub fn new(cap: Capacity) -> Self {
        Self {
            cap,
            excluded: false,
            residents: BTreeMap::new(),
            next_age: BTreeSet::new(),
            sums: Sums::default(),
            last_round: 0,
            window_core_ms: [0; 2],
            working_set_mib: 0,
            cold_this_round: 0,
        }
    }

    /// The sums.
    pub fn sums(&self) -> &Sums {
        &self.sums
    }

    /// The leases on the host, by lease index.
    pub fn residents(&self) -> &BTreeMap<usize, Resident<'w>> {
        &self.residents
    }

    /// Whether the host holds `lease`.
    pub fn holds(&self, lease: usize) -> bool {
        self.residents.contains_key(&lease)
    }

    /// The larger of the last two windows' truth CPU, in millicores.
    pub fn measured_cpu_millis(&self) -> u64 {
        self.window_core_ms[0].max(self.window_core_ms[1]) / WINDOW_TICKS
    }

    /// MiB held at the last reading.
    pub fn working_set_mib(&self) -> u64 {
        self.working_set_mib
    }

    /// Cold leases placed in this round.
    pub fn cold_this_round(&self) -> u32 {
        self.cold_this_round
    }

    /// The tick of the latest round.
    pub fn last_round(&self) -> u64 {
        self.last_round
    }

    /// A round begins at `tick`: what was placed in the last one has now been read for a
    /// full window, and the cold count starts again.
    pub fn begin_round(&mut self, tick: u64) {
        self.sums.covered = self.sums.covered.plus(self.sums.uncovered);
        self.sums.uncovered = Charge::default();
        self.last_round = tick;
        self.cold_this_round = 0;
    }

    /// Records a reading: the window's truth core-ms and the MiB held at its end.
    pub fn read(&mut self, window_core_ms: u64, working_set_mib: u64) {
        self.window_core_ms = [window_core_ms, self.window_core_ms[0]];
        self.working_set_mib = working_set_mib;
    }

    /// Starts `lease` at `tick`, age 0.
    pub fn place(&mut self, lease: usize, tick: u64, belief: &'w Belief) {
        let was = self.residents.insert(
            lease,
            Resident {
                start_tick: tick,
                age: 0,
                belief,
            },
        );
        assert!(was.is_none(), "lease {lease} placed twice on a host");
        self.next_age.insert((tick + MINUTE_TICKS, lease));
        for slot in 0..SLOTS {
            let c = belief.at_slot(0, slot).clamped(&self.cap);
            self.sums.slots[slot] = self.sums.slots[slot].plus(c);
        }
        let flat = belief.p_bar.clamped(&self.cap);
        if tick == self.last_round {
            self.sums.uncovered = self.sums.uncovered.plus(flat);
        } else {
            self.sums.covered = self.sums.covered.plus(flat);
        }
        if belief.cold {
            self.cold_this_round += 1;
        }
    }

    /// Removes `lease` (it ended or was killed), subtracting what it is charged now.
    pub fn leave(&mut self, lease: usize) {
        let r = self
            .residents
            .remove(&lease)
            .expect("the lease is on the host");
        self.next_age
            .remove(&(r.start_tick + MINUTE_TICKS * (r.age + 1), lease));
        for slot in 0..SLOTS {
            let c = r.belief.at_slot(r.age, slot).clamped(&self.cap);
            self.sums.slots[slot] = self.sums.slots[slot].minus(c);
        }
        let flat = r.belief.p_bar.clamped(&self.cap);
        if r.start_tick == self.last_round {
            self.sums.uncovered = self.sums.uncovered.minus(flat);
        } else {
            self.sums.covered = self.sums.covered.minus(flat);
        }
    }

    /// Ages every resident that has turned a minute older by `now`.
    pub fn tick(&mut self, now: u64) {
        while let Some(&(due, lease)) = self.next_age.first() {
            if due > now {
                break;
            }
            self.next_age.pop_first();
            let r = self.residents.get_mut(&lease).expect("an ageing resident");
            for slot in 1..SLOTS {
                let old = r.belief.at_slot(r.age, slot).clamped(&self.cap);
                let new = r.belief.at_slot(r.age + 1, slot).clamped(&self.cap);
                self.sums.slots[slot] = self.sums.slots[slot].minus(old).plus(new);
            }
            r.age += 1;
            self.next_age.insert((due + MINUTE_TICKS, lease));
        }
    }

    /// The sums computed again from the residents at `now`, each at the age its start
    /// says. Equal to [`HostState::sums`] after any sequence of the operations above.
    pub fn rebuild(&self, now: u64) -> Sums {
        let mut sums = Sums::default();
        for r in self.residents.values() {
            let age = (now - r.start_tick) / MINUTE_TICKS;
            for slot in 0..SLOTS {
                let c = r.belief.at_slot(age, slot).clamped(&self.cap);
                sums.slots[slot] = sums.slots[slot].plus(c);
            }
            let flat = r.belief.p_bar.clamped(&self.cap);
            if r.start_tick == self.last_round {
                sums.uncovered = sums.uncovered.plus(flat);
            } else {
                sums.covered = sums.covered.plus(flat);
            }
        }
        sums
    }
}
