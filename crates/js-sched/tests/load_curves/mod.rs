//! The curves a scheduler is given in a test of profile admission: a fake
//! `CurveSource` that tests fill by key and change while a simulation runs.

#![allow(dead_code, reason = "each test binary uses some of the curves")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use js_types::sched::WorkKey;
use js_types::sched::usage::{Curve, CurveSource, MinutePoint};

/// The curves the scheduler is given in a simulation, by key; they can be changed while
/// it runs.
#[derive(Debug, Default)]
pub struct Curves {
    class: &'static str,
    curves: Mutex<BTreeMap<WorkKey, Curve>>,
    lives: Mutex<BTreeMap<WorkKey, u32>>,
    all: Mutex<Option<Curve>>,
    life_all: Mutex<Option<u32>>,
}

impl Curves {
    /// No curves yet, for the keys of `class`.
    pub fn new(class: &'static str) -> Arc<Self> {
        Arc::new(Self {
            class,
            ..Self::default()
        })
    }

    /// The same curve for every key.
    pub fn set_all(&self, points: &[(u32, u64)], runs: u16) {
        *self.all.lock().expect("all") = Some(make(points, runs));
    }

    /// The curve of `key`, fitted from `runs` runs: minute `i` asks for `points[i]`
    /// millicores and bytes.
    pub fn set(&self, key: &str, points: &[(u32, u64)], runs: u16) {
        self.curves
            .lock()
            .expect("curves")
            .insert(WorkKey::new(self.class, key), make(points, runs));
    }

    pub fn remove(&self, key: &str) {
        self.curves
            .lock()
            .expect("curves")
            .remove(&WorkKey::new(self.class, key));
    }

    /// The same P90 of wall minutes for every key.
    pub fn set_life_all(&self, minutes: u32) {
        *self.life_all.lock().expect("life_all") = Some(minutes);
    }

    /// The P90 of the wall minutes the runs of `key` lasted.
    pub fn set_life(&self, key: &str, minutes: u32) {
        self.lives
            .lock()
            .expect("lives")
            .insert(WorkKey::new(self.class, key), minutes);
    }
}

impl CurveSource for Curves {
    fn curve(&self, key: &WorkKey) -> Option<Curve> {
        let own = self.curves.lock().expect("curves").get(key).cloned();
        own.or_else(|| self.all.lock().expect("all").clone())
    }

    fn wall_minutes_p90(&self, key: &WorkKey) -> Option<u32> {
        let own = self.lives.lock().expect("lives").get(key).copied();
        own.or_else(|| *self.life_all.lock().expect("life_all"))
    }
}

/// A curve of `points` (millicores, bytes) per minute, fitted from `runs` runs.
pub fn make(points: &[(u32, u64)], runs: u16) -> Curve {
    let points = points
        .iter()
        .map(|&(cores_milli, working_set_bytes)| MinutePoint {
            cores_milli,
            working_set_bytes,
        })
        .collect();
    Curve::new(points, runs).expect("a short curve")
}

/// `points` with each value the largest within `delta` minutes: the dilation of the fit.
pub fn dilated(points: &[(u32, u64)], delta: usize) -> Vec<(u32, u64)> {
    (0..points.len())
        .map(|i| {
            let from = i.saturating_sub(delta);
            let to = (i + delta).min(points.len() - 1);
            let window = &points[from..=to];
            (
                window.iter().map(|p| p.0).max().unwrap_or(0),
                window.iter().map(|p| p.1).max().unwrap_or(0),
            )
        })
        .collect()
}
