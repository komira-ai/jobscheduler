//! The fitted curves in memory, where admission reads them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

use js_types::sched::WorkKey;
use js_types::sched::usage::{Curve, CurveSource};

use crate::fit::Fit;
use crate::key::ProfileKey;

/// A key idle for more than this many days has no curve (the staleness rule of
/// `js_sizing::STALE_DAYS`, which a flat estimate follows too).
pub const STALE_DAYS: u64 = 60;

/// The mapping from a scheduler's action key to the name its profile is kept under;
/// `None` for a key that has no profile name (it is charged flat).
pub type KeyMap = Box<dyn Fn(&WorkKey) -> Option<ProfileKey> + Send + Sync>;

#[derive(Clone)]
struct Held {
    curve: Curve,
    wall_minutes_p90: u32,
    last_used_day: u64,
}

/// The curves of every key the profile store holds, as [`CurveSource`] for admission.
///
/// The store ([`crate::ProfileStore`]) fills it when it opens and when a run ends, so a
/// read is a map lookup and never touches the state store. A key that is cold (fewer
/// than three runs), idle for more than [`STALE_DAYS`] days, or without a profile name
/// has no curve, and the caller charges it flat.
pub struct ProfileCurves {
    map: KeyMap,
    today: AtomicU64,
    held: RwLock<BTreeMap<ProfileKey, Held>>,
}

impl ProfileCurves {
    /// An empty set of curves; `map` names the profile of an action key.
    #[must_use]
    pub fn new(map: KeyMap) -> Self {
        Self {
            map,
            today: AtomicU64::new(0),
            held: RwLock::default(),
        }
    }

    /// Tells the curves what day it is (days since the Unix epoch), for the staleness
    /// rule. The owner calls it when the day changes.
    pub fn set_today(&self, day: u64) {
        self.today.store(day, Ordering::Release);
    }

    pub(crate) fn put(&self, key: &ProfileKey, last_used_day: u64, fit: Option<&Fit>) {
        let held = fit.and_then(|f| {
            Some(Held {
                curve: Curve::try_from(f).ok()?,
                wall_minutes_p90: f.wall_minutes_p90,
                last_used_day,
            })
        });
        let mut map = self.held.write().unwrap_or_else(PoisonError::into_inner);
        match held {
            Some(h) => {
                map.insert(key.clone(), h);
            }
            None => {
                map.remove(key);
            }
        }
    }

    pub(crate) fn touch(&self, key: &ProfileKey, day: u64) {
        let mut map = self.held.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = map.get_mut(key) {
            h.last_used_day = h.last_used_day.max(day);
        }
    }

    pub(crate) fn remove(&self, key: &ProfileKey) {
        self.put(key, 0, None);
    }

    fn lookup(&self, key: &WorkKey) -> Option<Held> {
        let name = (self.map)(key)?;
        let stale_before = self
            .today
            .load(Ordering::Acquire)
            .saturating_sub(STALE_DAYS);
        let map = self.held.read().unwrap_or_else(PoisonError::into_inner);
        map.get(&name)
            .filter(|h| h.last_used_day >= stale_before)
            .cloned()
    }
}

impl CurveSource for ProfileCurves {
    fn curve(&self, key: &WorkKey) -> Option<Curve> {
        self.lookup(key).map(|h| h.curve)
    }

    fn wall_minutes_p90(&self, key: &WorkKey) -> Option<u32> {
        self.lookup(key).map(|h| h.wall_minutes_p90)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_types::sched::usage::MinutePoint;

    fn pk(s: &str) -> ProfileKey {
        ProfileKey::parse(s).unwrap()
    }

    fn wk(class: &str, dedup: &str) -> WorkKey {
        WorkKey::new(class, dedup)
    }

    fn curves() -> ProfileCurves {
        ProfileCurves::new(Box::new(|k| {
            // The test's mapping: only class "c" has a profile, named by its dedup text.
            (k.class == "c")
                .then(|| ProfileKey::parse(&k.dedup).ok())
                .flatten()
        }))
    }

    fn fit(cores: u32, life: u32) -> Fit {
        Fit {
            points: vec![MinutePoint {
                cores_milli: cores,
                working_set_bytes: 5,
            }],
            cores_learned: true,
            runs: 4,
            wall_minutes_p90: life,
        }
    }

    /// Catches: a curve or a life served for a key that was not put, for the wrong
    /// class, or for a name that is not a profile key.
    #[test]
    fn only_a_mapped_key_with_a_held_curve_has_one() {
        let c = curves();
        c.put(&pk("a"), 10, Some(&fit(700, 3)));
        let got = c.curve(&wk("c", "a")).unwrap();
        assert_eq!(got.points()[0].cores_milli, 700);
        assert_eq!(got.runs(), 4);
        assert_eq!(c.wall_minutes_p90(&wk("c", "a")), Some(3));
        assert!(c.curve(&wk("c", "b")).is_none(), "not held");
        assert!(
            c.curve(&wk("other", "a")).is_none(),
            "no profile name for the class"
        );
        assert!(c.curve(&wk("c", "A B")).is_none(), "not a profile key");
        assert_eq!(c.wall_minutes_p90(&wk("c", "b")), None);
    }

    /// Catches: the memory-only mark of a fit lost on the way to admission, which would
    /// charge the zero cores of a key whose cores were never learned.
    #[test]
    fn a_fit_without_cores_is_served_as_a_memory_only_curve() {
        let c = curves();
        c.put(&pk("a"), 10, Some(&fit(700, 3)));
        assert!(c.curve(&wk("c", "a")).unwrap().cores_known());
        let mut f = fit(0, 3);
        f.cores_learned = false;
        c.put(&pk("a"), 10, Some(&f));
        let got = c.curve(&wk("c", "a")).unwrap();
        assert!(!got.cores_known());
        assert_eq!(got.points()[0].working_set_bytes, 5);
    }

    /// Catches: staleness `>=` vs `>` (idle exactly 60 days is still fresh, 61 is not), a
    /// clock that never moves, and a touch that does not renew.
    #[test]
    fn a_key_idle_more_than_60_days_has_no_curve_until_touched() {
        let c = curves();
        c.put(&pk("a"), 100, Some(&fit(1, 1)));
        c.set_today(160);
        assert!(c.curve(&wk("c", "a")).is_some(), "idle 60 days");
        c.set_today(161);
        assert!(c.curve(&wk("c", "a")).is_none(), "idle 61 days");
        assert_eq!(c.wall_minutes_p90(&wk("c", "a")), None);
        c.touch(&pk("a"), 150);
        assert!(c.curve(&wk("c", "a")).is_some());
        c.touch(&pk("a"), 120);
        c.set_today(210);
        assert!(
            c.curve(&wk("c", "a")).is_some(),
            "an older touch does not lower the day"
        );
        c.touch(&pk("absent"), 999);
    }

    /// Catches: a key that lost its fit (or was evicted) still served.
    #[test]
    fn putting_no_fit_or_removing_drops_the_curve() {
        let c = curves();
        c.put(&pk("a"), 1, Some(&fit(1, 1)));
        c.put(&pk("a"), 1, None);
        assert!(c.curve(&wk("c", "a")).is_none());
        c.put(&pk("a"), 1, Some(&fit(1, 1)));
        c.remove(&pk("a"));
        assert!(c.curve(&wk("c", "a")).is_none());
    }

    #[test]
    fn a_fit_too_long_for_a_curve_is_not_held() {
        let c = curves();
        let mut long = fit(1, 1);
        long.points = vec![MinutePoint::default(); 65];
        c.put(&pk("a"), 1, Some(&long));
        assert!(c.curve(&wk("c", "a")).is_none());
    }
}
