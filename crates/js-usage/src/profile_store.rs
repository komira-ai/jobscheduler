//! The durable per-key profiles: one state-store row per action key, holding its ring of
//! runs, with a least-recently-used bound on the bytes of all rows.
//!
//! A row is written when a run ends, not per minute. The index of what is stored (each
//! key's size and last-used day) lives in memory and is rebuilt by one scan when the store
//! opens; a read of a key's use is a [`ProfileStore::touch`] in memory, and its day reaches
//! the store at most once a day per key, in [`ProfileStore::persist_touches`]. There is one
//! writer (the scheduler); a second one is caught by the version the row is written under.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_state::{Cond, Store, StoreError, Txn};
use thiserror::Error;

use crate::codec::{self, RowError};
use crate::curves::{ProfileCurves, STALE_DAYS};
use crate::fit::{Fit, fit};
use crate::key::ProfileKey;
use crate::ring::{Admitted, Run, RunSeries};

/// The state-store prefix of the profile rows; a row's key is the prefix and the
/// [`ProfileKey`].
pub const KEY_PREFIX: &str = "usage/key/";

fn store_key(key: &ProfileKey) -> String {
    format!("{KEY_PREFIX}{key}")
}

/// Why the profile store could not do what it was asked.
#[derive(Debug, Error)]
pub enum ProfileError {
    /// The state store failed.
    #[error("state store: {0}")]
    Store(#[from] StoreError),
    /// A row is not a profile row.
    #[error("profile row {key} is corrupt: {source}")]
    Corrupt {
        /// The key whose row it is.
        key: String,
        /// What is wrong with it.
        source: RowError,
    },
    /// Another writer changed the row between this one's read and write, twice.
    #[error("profile row {0} was changed by another writer")]
    Conflict(String),
    /// The key's row alone is larger than the whole budget.
    #[error("profile row {key} of {bytes} bytes cannot fit a budget of {budget} bytes")]
    TooLarge {
        /// The key.
        key: String,
        /// The size of its row.
        bytes: u64,
        /// The budget.
        budget: u64,
    },
}

/// What the store knows of one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyProfile {
    /// How many runs the key's ring holds.
    pub runs: u16,
    /// The key's fit; `None` below three runs.
    pub fit: Option<Fit>,
    /// The day the key was last used (a run ended, or [`ProfileStore::touch`]).
    pub last_used_day: u64,
    /// The size of the stored row, in bytes.
    pub bytes: u32,
}

impl KeyProfile {
    /// Whether the key has been idle for more than [`STALE_DAYS`] days on `now_day`: it
    /// gets no curve.
    #[must_use]
    pub fn is_stale(&self, now_day: u64) -> bool {
        self.last_used_day < now_day.saturating_sub(STALE_DAYS)
    }
}

#[derive(Clone, Copy)]
struct Entry {
    bytes: u64,
    used_day: u64,
    /// Orders keys used on the same day; in memory only, ascending with each use.
    used_tick: u64,
    /// The last-used day the stored row holds.
    persisted_day: u64,
}

/// The profile rows in a state store, bounded to a budget of bytes.
pub struct ProfileStore {
    store: Arc<dyn Store>,
    curves: Arc<ProfileCurves>,
    budget_bytes: u64,
    index: BTreeMap<ProfileKey, Entry>,
    /// Keys by `(used_day, used_tick, key)`: the first is the next to evict.
    lru: BTreeSet<(u64, u64, ProfileKey)>,
    total: u64,
    tick: u64,
    corrupt: usize,
}

impl ProfileStore {
    /// Opens the rows in `store`, with one scan: builds the index and fills `curves`.
    /// A row that is not a profile row (or whose name is not a [`ProfileKey`]) is left
    /// alone and counted in [`ProfileStore::corrupt_rows`]; the next run of that key
    /// replaces it.
    ///
    /// # Errors
    /// [`ProfileError::Store`] when the scan fails.
    pub fn open(
        store: Arc<dyn Store>,
        curves: Arc<ProfileCurves>,
        budget_bytes: u64,
        now_day: u64,
    ) -> Result<Self, ProfileError> {
        curves.set_today(now_day);
        let mut opened = Self {
            store,
            curves,
            budget_bytes,
            index: BTreeMap::new(),
            lru: BTreeSet::new(),
            total: 0,
            tick: 0,
            corrupt: 0,
        };
        for (name, held) in opened.store.scan_prefix(KEY_PREFIX)? {
            let parsed = ProfileKey::parse(&name[KEY_PREFIX.len()..]).ok();
            let row = codec::decode(&held.value).ok();
            let Some((key, row)) = parsed.zip(row) else {
                opened.corrupt += 1;
                continue;
            };
            let bytes = held.value.len() as u64;
            opened.set_entry(&key, bytes, row.last_used_day, row.last_used_day);
            opened
                .curves
                .put(&key, row.last_used_day, fit(&row.ring).as_ref());
        }
        Ok(opened)
    }

    /// The curves this store keeps current.
    #[must_use]
    pub fn curves(&self) -> &Arc<ProfileCurves> {
        &self.curves
    }

    /// How many keys the store holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether the store holds no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The bytes of every row held (at most the budget after a write).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// How many rows [`ProfileStore::open`] could not read.
    #[must_use]
    pub fn corrupt_rows(&self) -> usize {
        self.corrupt
    }

    /// Adds the run `series` of `key`, which ended on its own on `now_day`, to the key's
    /// ring and writes the row. Rows of the least recently used other keys are deleted in
    /// the same transaction while the bytes of all rows exceed the budget, so a failure
    /// leaves nothing half done. A row another writer changed meanwhile is read again and
    /// the write tried once more.
    ///
    /// # Errors
    /// [`ProfileError::Store`] when the state store fails, [`ProfileError::Conflict`] when
    /// the row was changed again, and [`ProfileError::TooLarge`] when the row alone does
    /// not fit the budget. A run that does not enter the ring is `Ok` with the reason, and
    /// writes nothing.
    pub fn run_ended(
        &mut self,
        key: &ProfileKey,
        series: &RunSeries,
        now_day: u64,
    ) -> Result<Admitted, ProfileError> {
        let run = match Run::from_series(series) {
            Ok(run) => run,
            Err(refused) => return Ok(refused),
        };
        let name = store_key(key);
        let mut retried = false;
        loop {
            let held = self.store.get(&name)?;
            // A row that cannot be read is replaced by a ring of this run.
            let mut ring = held
                .as_ref()
                .and_then(|v| codec::decode(&v.value).ok())
                .map(|row| row.ring)
                .unwrap_or_default();
            ring.push_run(run.clone());
            let bytes = codec::encode(now_day, &ring);
            let size = bytes.len() as u64;
            let (victims, fits) = self.victims(key, size);
            if !fits {
                return Err(ProfileError::TooLarge {
                    key: key.to_string(),
                    bytes: size,
                    budget: self.budget_bytes,
                });
            }
            let cond = held.map_or(Cond::Absent, |v| Cond::Version(v.version));
            let mut txn = Txn::new().put(name.clone(), bytes, cond);
            for victim in &victims {
                txn = txn.delete(store_key(victim), Cond::Any);
            }
            match self.store.txn(txn)? {
                Ok(_) => {
                    for victim in &victims {
                        self.unlink(victim);
                        self.curves.remove(victim);
                    }
                    self.set_entry(key, size, now_day, now_day);
                    self.curves.put(key, now_day, fit(&ring).as_ref());
                    return Ok(Admitted::Yes);
                }
                Err(_) if !retried => retried = true,
                Err(_) => return Err(ProfileError::Conflict(key.to_string())),
            }
        }
    }

    /// The keys to delete, least recently used first, so that a row of `size` bytes for
    /// `key` fits the budget, and whether it then does.
    fn victims(&self, key: &ProfileKey, size: u64) -> (Vec<ProfileKey>, bool) {
        let own = self.index.get(key).map_or(0, |e| e.bytes);
        let mut total = self.total - own + size;
        let mut victims = Vec::new();
        for (_, _, name) in &self.lru {
            if total <= self.budget_bytes {
                break;
            }
            if name != key {
                total -= self.index[name].bytes;
                victims.push(name.clone());
            }
        }
        (victims, total <= self.budget_bytes)
    }

    /// What is stored for `key`, read from the state store; `None` for a key it has no row
    /// for.
    ///
    /// # Errors
    /// [`ProfileError::Store`] when the read fails, [`ProfileError::Corrupt`] when the row
    /// is not a profile row.
    pub fn profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, ProfileError> {
        let Some(held) = self.store.get(&store_key(key))? else {
            return Ok(None);
        };
        let row = codec::decode(&held.value).map_err(|source| ProfileError::Corrupt {
            key: key.to_string(),
            source,
        })?;
        let used = self
            .index
            .get(key)
            .map_or(row.last_used_day, |e| e.used_day);
        Ok(Some(KeyProfile {
            runs: u16::try_from(row.ring.len()).unwrap_or(u16::MAX),
            fit: fit(&row.ring),
            last_used_day: used,
            bytes: u32::try_from(held.value.len()).unwrap_or(u32::MAX),
        }))
    }

    /// Records that `key` was used on `day`. In memory only: nothing is written (a read
    /// must not cost a write); [`ProfileStore::persist_touches`] writes the day later. A
    /// key the store does not hold is ignored.
    pub fn touch(&mut self, key: &ProfileKey, day: u64) {
        let Some(entry) = self.index.get(key).copied() else {
            return;
        };
        let used_day = entry.used_day.max(day);
        self.unlink(key);
        self.set_entry(key, entry.bytes, used_day, entry.persisted_day);
        self.curves.touch(key, day);
    }

    /// Writes the last-used day of every key used since its row was last written, one
    /// write for each such key, and returns how many. The owner calls it about once a day,
    /// so a key costs at most one write a day for being read.
    ///
    /// # Errors
    /// [`ProfileError::Store`] when a read or write fails, [`ProfileError::Corrupt`] when
    /// a row is not a profile row, [`ProfileError::Conflict`] when another writer changed
    /// a row. A row that no longer exists is dropped from the index.
    pub fn persist_touches(&mut self) -> Result<usize, ProfileError> {
        let due: Vec<(ProfileKey, u64)> = self
            .index
            .iter()
            .filter(|(_, e)| e.used_day > e.persisted_day)
            .map(|(k, e)| (k.clone(), e.used_day))
            .collect();
        let mut written = 0;
        for (key, day) in due {
            let name = store_key(&key);
            let Some(held) = self.store.get(&name)? else {
                self.unlink(&key);
                self.curves.remove(&key);
                continue;
            };
            let row = codec::decode(&held.value).map_err(|source| ProfileError::Corrupt {
                key: key.to_string(),
                source,
            })?;
            let bytes = codec::encode(day, &row.ring);
            let size = bytes.len() as u64;
            let txn = Txn::new().put(name, bytes, Cond::Version(held.version));
            if self.store.txn(txn)?.is_err() {
                return Err(ProfileError::Conflict(key.to_string()));
            }
            let before = self.index[&key];
            self.unlink(&key);
            self.place(
                &key,
                Entry {
                    bytes: size,
                    persisted_day: day,
                    ..before
                },
            );
            written += 1;
        }
        Ok(written)
    }

    /// Takes `key` out of the index and the recency order (a key that is not there is
    /// left alone).
    fn unlink(&mut self, key: &ProfileKey) {
        if let Some(old) = self.index.remove(key) {
            self.total -= old.bytes;
            self.lru.remove(&(old.used_day, old.used_tick, key.clone()));
        }
    }

    /// Puts `key` in the index as the most recently used of its day.
    fn set_entry(&mut self, key: &ProfileKey, bytes: u64, used_day: u64, persisted_day: u64) {
        self.unlink(key);
        self.tick += 1;
        let entry = Entry {
            bytes,
            used_day,
            used_tick: self.tick,
            persisted_day,
        };
        self.place(key, entry);
    }

    /// Puts `key` in the index and the recency order as `entry` says (it is not in
    /// either).
    fn place(&mut self, key: &ProfileKey, entry: Entry) {
        self.total += entry.bytes;
        self.lru
            .insert((entry.used_day, entry.used_tick, key.clone()));
        self.index.insert(key.clone(), entry);
    }
}

#[cfg(test)]
mod tests {
    use js_types::sched::WorkKey;
    use js_types::sched::usage::CurveSource;

    use super::*;
    use crate::ring::tests::series;
    use crate::testkit::TestStore;

    fn pk(s: &str) -> ProfileKey {
        ProfileKey::parse(s).unwrap()
    }

    fn wk(name: &str) -> WorkKey {
        WorkKey::new("c", name)
    }

    fn curves() -> Arc<ProfileCurves> {
        Arc::new(ProfileCurves::new(Box::new(|k| {
            ProfileKey::parse(&k.dedup).ok()
        })))
    }

    fn open(store: &Arc<TestStore>, budget: u64, day: u64) -> ProfileStore {
        ProfileStore::open(store.clone(), curves(), budget, day).unwrap()
    }

    fn run() -> RunSeries {
        series(&[(100, 10), (200, 20)])
    }

    /// Three runs of a key, on `day`: enough for a curve.
    fn learn(ps: &mut ProfileStore, key: &str, day: u64) {
        for _ in 0..3 {
            assert_eq!(ps.run_ended(&pk(key), &run(), day).unwrap(), Admitted::Yes);
        }
    }

    fn stored(store: &TestStore) -> Vec<String> {
        store
            .fake
            .scan_prefix(KEY_PREFIX)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k[KEY_PREFIX.len()..].to_owned())
            .collect()
    }

    fn row_bytes(store: &TestStore, key: &str) -> u64 {
        store
            .fake
            .get(&format!("{KEY_PREFIX}{key}"))
            .unwrap()
            .unwrap()
            .value
            .len() as u64
    }

    /// Catches: a curve handed out after two runs, none after three, a fit that is not
    /// what the stored ring gives, and a first run that is not stored.
    #[test]
    fn a_key_has_a_curve_from_its_third_run() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 100);
        assert!(ps.is_empty());
        for n in 1..=3 {
            assert_eq!(ps.run_ended(&pk("a"), &run(), 100).unwrap(), Admitted::Yes);
            let has = ps.curves().curve(&wk("a")).is_some();
            assert_eq!(has, n == 3, "after {n} runs");
        }
        let p = ps.profile(&pk("a")).unwrap().unwrap();
        assert_eq!(p.runs, 3);
        assert_eq!(p.fit.as_ref().unwrap().points.len(), 2);
        assert_eq!(u64::from(p.bytes), row_bytes(&store, "a"));
        assert_eq!(ps.total_bytes(), u64::from(p.bytes));
        assert_eq!(ps.len(), 1);
        let curve = ps.curves().curve(&wk("a")).unwrap();
        assert_eq!(curve.points()[1].cores_milli, 200);
        assert_eq!(ps.curves().wall_minutes_p90(&wk("a")), Some(2));
    }

    /// Catches: a refused run (empty, or too many minutes missing) leaving a row or a
    /// write behind.
    #[test]
    fn a_run_that_does_not_enter_writes_nothing() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 100);
        assert_eq!(
            ps.run_ended(&pk("a"), &RunSeries::default(), 100).unwrap(),
            Admitted::Empty
        );
        let mut holey = series(&[(1, 1); 10]);
        holey.minutes.retain(|m| m.minute == 0 || m.minute == 9);
        assert_eq!(
            ps.run_ended(&pk("a"), &holey, 100).unwrap(),
            Admitted::MissingMinutes
        );
        assert_eq!(store.txn_count(), 0);
        assert!(stored(&store).is_empty());
        assert_eq!(ps.profile(&pk("a")).unwrap(), None);
    }

    /// Catches: an index or cache rebuilt differently from the one that wrote the rows,
    /// and an open that leaves the curves empty.
    #[test]
    fn reopening_rebuilds_the_same_index_and_curves() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 100);
        learn(&mut ps, "a", 100);
        learn(&mut ps, "b", 101);
        let (len, total) = (ps.len(), ps.total_bytes());
        let curve = ps.curves().curve(&wk("a"));
        drop(ps);

        let again = open(&store, 1 << 20, 101);
        assert_eq!((again.len(), again.total_bytes()), (len, total));
        assert_eq!(again.curves().curve(&wk("a")), curve);
        assert!(again.curves().curve(&wk("b")).is_some());
        assert_eq!(again.profile(&pk("a")).unwrap().unwrap().last_used_day, 100);
        assert_eq!(again.corrupt_rows(), 0);
    }

    /// Catches: eviction of the most recent instead of the least recent key, a use that
    /// does not count, an older day not evicted before a newer one, an evicted key still
    /// served from the cache, and eviction in a separate write.
    #[test]
    fn the_least_recently_used_key_is_evicted_in_the_same_write() {
        let store = TestStore::new();
        let mut probe = open(&TestStore::new(), 1 << 20, 10);
        learn(&mut probe, "x", 10);
        let one = probe.total_bytes();

        let mut ps = open(&store, 3 * one, 10);
        learn(&mut ps, "a", 10);
        learn(&mut ps, "b", 10);
        learn(&mut ps, "c", 10);
        assert_eq!(
            ps.total_bytes(),
            3 * one,
            "a budget met exactly evicts nothing"
        );
        assert_eq!(stored(&store), ["a", "b", "c"]);

        // `a` was used last of the three: `b` is next to go.
        ps.touch(&pk("a"), 10);
        let before = store.txn_count();
        learn_one(&mut ps, "d", 10);
        assert_eq!(
            stored(&store),
            ["a", "c", "d"],
            "b went, in the write of d's first run"
        );
        assert!(
            ps.curves().curve(&wk("b")).is_none(),
            "an evicted key is charged flat"
        );
        assert!(ps.curves().curve(&wk("a")).is_some());
        assert_eq!(
            store.txn_count(),
            before + 1,
            "put and delete in one transaction"
        );
        learn_one(&mut ps, "d", 10);
        learn_one(&mut ps, "d", 10);
        assert_eq!(stored(&store), ["a", "c", "d"]);
        assert!(ps.total_bytes() <= 3 * one);

        // A key last used on an older day goes before one used later in the day.
        ps.touch(&pk("c"), 11);
        ps.touch(&pk("d"), 11);
        learn_one(&mut ps, "e", 11);
        assert_eq!(
            stored(&store),
            ["c", "d", "e"],
            "a (day 10) went, and only a"
        );
    }

    fn learn_one(ps: &mut ProfileStore, key: &str, day: u64) {
        assert_eq!(ps.run_ended(&pk(key), &run(), day).unwrap(), Admitted::Yes);
    }

    /// Catches: eviction that deletes the key being written when it is the least recently
    /// used one (its own row would go with the write that replaces it), or that stops at
    /// it and leaves the budget exceeded.
    #[test]
    fn the_key_being_written_is_never_its_own_victim() {
        let size_after = |runs: usize| {
            let mut probe = open(&TestStore::new(), 1 << 20, 10);
            for _ in 0..runs {
                learn_one(&mut probe, "x", 10);
            }
            probe.total_bytes()
        };
        let (one, two) = (size_after(1), size_after(2));
        assert!(two > one);
        let store = TestStore::new();
        let mut ps = open(&store, one + two - 1, 10);
        learn_one(&mut ps, "a", 10);
        learn_one(&mut ps, "b", 10);
        assert_eq!(stored(&store), ["a", "b"], "both fit while a has one run");
        // `a` is the least recently used, and its second run no longer fits beside `b`.
        learn_one(&mut ps, "a", 10);
        assert_eq!(stored(&store), ["a"]);
        assert_eq!(ps.profile(&pk("a")).unwrap().unwrap().runs, 2);
        assert_eq!(ps.total_bytes(), two);
    }

    /// Catches: a budget one byte short being treated as met.
    #[test]
    fn one_byte_over_the_budget_evicts_one_key() {
        let store = TestStore::new();
        let mut probe = open(&TestStore::new(), 1 << 20, 10);
        learn_one(&mut probe, "x", 10);
        let one = probe.total_bytes();
        let mut ps = open(&store, 2 * one - 1, 10);
        learn_one(&mut ps, "a", 10);
        learn_one(&mut ps, "b", 10);
        assert_eq!(stored(&store), ["b"]);
    }

    /// Catches: keys at the same last-used day being evicted in an order that depends on
    /// the process: after a reopen they go in key order.
    #[test]
    fn keys_of_the_same_day_are_evicted_in_key_order_after_a_reopen() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 10);
        for k in ["c", "a", "b"] {
            learn_one(&mut ps, k, 10);
        }
        let one = row_bytes(&store, "a");
        drop(ps);
        let mut ps = open(&store, 3 * one, 10);
        learn_one(&mut ps, "d", 10);
        assert_eq!(
            stored(&store),
            ["b", "c", "d"],
            "a is the least in key order"
        );
    }

    /// Catches: a failed write leaving victims deleted, the index or the cache changed.
    #[test]
    fn a_failing_store_leaves_no_half_state() {
        let store = TestStore::new();
        let mut probe = open(&TestStore::new(), 1 << 20, 10);
        learn(&mut probe, "x", 10);
        let one = probe.total_bytes();
        let mut ps = open(&store, one, 10);
        learn(&mut ps, "a", 10);
        let (total, rows) = (ps.total_bytes(), stored(&store));

        TestStore::set(&store.fail_txn, true);
        let err = ps.run_ended(&pk("b"), &run(), 10).unwrap_err();
        assert!(matches!(err, ProfileError::Store(_)), "{err}");
        assert_eq!((ps.total_bytes(), ps.len()), (total, 1));
        assert_eq!(stored(&store), rows);
        assert!(
            ps.curves().curve(&wk("a")).is_some(),
            "the victim is still served"
        );

        TestStore::set(&store.fail_txn, false);
        TestStore::set(&store.fail_get, true);
        assert!(matches!(
            ps.run_ended(&pk("b"), &run(), 10),
            Err(ProfileError::Store(_))
        ));
        assert!(matches!(ps.profile(&pk("a")), Err(ProfileError::Store(_))));
        assert!(
            matches!(ps.persist_touches(), Ok(0)),
            "nothing due, nothing read"
        );
        ps.touch(&pk("a"), 11);
        assert!(matches!(ps.persist_touches(), Err(ProfileError::Store(_))));
        TestStore::set(&store.fail_get, false);
        assert_eq!(stored(&store), rows);
    }

    /// Catches: a row bigger than the whole budget evicting every other key and then
    /// failing, or being written over the budget.
    #[test]
    fn a_row_larger_than_the_budget_is_refused_and_evicts_nothing() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 10);
        learn_one(&mut ps, "a", 10);
        let small = ps.total_bytes();
        let mut tight = open(&store, small + 2, 10);
        let long = series(&[(1, 1); 40]);
        let err = tight.run_ended(&pk("b"), &long, 10).unwrap_err();
        assert!(matches!(err, ProfileError::TooLarge { .. }), "{err}");
        assert!(err.to_string().contains("cannot fit a budget"));
        assert_eq!(stored(&store), ["a"]);
        assert_eq!(tight.len(), 1);
    }

    /// Catches: a second writer's change overwritten (the write must carry the version it
    /// read), no retry, and a retry that loops forever.
    #[test]
    fn a_row_changed_between_read_and_write_is_read_again_once() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 10);
        learn_one(&mut ps, "a", 10);
        let name = format!("{KEY_PREFIX}a");
        // The other writer replaces the row once, with a ring of five runs.
        let mut five = crate::ring::Ring::new();
        for _ in 0..5 {
            five.push(&run());
        }
        let theirs = codec::encode(10, &five);
        let mut once = Some(theirs);
        store.on_get(move |fake| {
            if let Some(bytes) = once.take() {
                let _ = fake.txn(Txn::new().put(name.clone(), bytes, Cond::Any));
            }
        });
        assert_eq!(ps.run_ended(&pk("a"), &run(), 10).unwrap(), Admitted::Yes);
        assert_eq!(
            ps.profile(&pk("a")).unwrap().unwrap().runs,
            6,
            "their five and ours"
        );

        // A writer that changes the row on every read wins both tries.
        let name = format!("{KEY_PREFIX}a");
        let empty = codec::encode(10, &crate::ring::Ring::new());
        store.on_get(move |fake| {
            let _ = fake.txn(Txn::new().put(name.clone(), empty.clone(), Cond::Any));
        });
        let err = ps.run_ended(&pk("a"), &run(), 10).unwrap_err();
        assert_eq!(
            err.to_string(),
            "profile row a was changed by another writer"
        );
    }

    /// Catches: a read writing (the use of a key must cost no write), a write per touch, a
    /// day that does not reach the store, and a second write for a day already written.
    #[test]
    fn a_touch_writes_nothing_and_its_day_is_persisted_once() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 10);
        learn(&mut ps, "a", 10);
        learn(&mut ps, "b", 10);
        let before = store.txn_count();
        for _ in 0..5 {
            ps.touch(&pk("a"), 12);
        }
        ps.touch(&pk("absent"), 12);
        assert_eq!(store.txn_count(), before, "touch is in memory only");
        assert_eq!(ps.profile(&pk("a")).unwrap().unwrap().last_used_day, 12);
        assert_eq!(
            codec::decode(&store.fake.get("usage/key/a").unwrap().unwrap().value)
                .unwrap()
                .last_used_day,
            10,
            "the store still holds the old day"
        );

        assert_eq!(ps.persist_touches().unwrap(), 1, "only a was used");
        assert_eq!(store.txn_count(), before + 1);
        assert_eq!(ps.persist_touches().unwrap(), 0, "nothing new to write");
        assert_eq!(store.txn_count(), before + 1);

        // A touch with an older day does not move the day back.
        ps.touch(&pk("a"), 11);
        assert_eq!(ps.persist_touches().unwrap(), 0);
        assert_eq!(
            ps.total_bytes(),
            row_bytes(&store, "a") + row_bytes(&store, "b")
        );
        drop(ps);
        let again = open(&store, 1 << 20, 12);
        assert_eq!(again.profile(&pk("a")).unwrap().unwrap().last_used_day, 12);
    }

    /// Catches: a row that vanished, was changed, or is unreadable making the daily
    /// persist fail forever or write over another writer.
    #[test]
    fn persisting_a_touch_meets_a_vanished_changed_or_corrupt_row() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 10);
        learn_one(&mut ps, "a", 10);

        ps.touch(&pk("a"), 11);
        store
            .fake
            .txn(Txn::new().put("usage/key/a", vec![9u8], Cond::Any))
            .unwrap()
            .unwrap();
        let err = ps.persist_touches().unwrap_err();
        assert!(matches!(err, ProfileError::Corrupt { .. }), "{err}");
        assert!(err.to_string().contains("a is corrupt"), "{err}");

        // A write between the read and the write.
        let good = codec::encode(10, &crate::ring::Ring::new());
        store
            .fake
            .txn(Txn::new().put("usage/key/a", good.clone(), Cond::Any))
            .unwrap()
            .unwrap();
        store.on_get(move |fake| {
            let _ = fake.txn(Txn::new().put("usage/key/a", good.clone(), Cond::Any));
        });
        assert!(matches!(
            ps.persist_touches(),
            Err(ProfileError::Conflict(_))
        ));
        store.no_hook();

        TestStore::set(&store.fail_txn, true);
        assert!(matches!(ps.persist_touches(), Err(ProfileError::Store(_))));
        TestStore::set(&store.fail_txn, false);

        store
            .fake
            .txn(Txn::new().delete("usage/key/a", Cond::Any))
            .unwrap()
            .unwrap();
        assert_eq!(ps.persist_touches().unwrap(), 0);
        assert!(ps.is_empty(), "the vanished row left the index");
        assert_eq!(ps.total_bytes(), 0);
    }

    /// Catches: a key idle over 60 days getting a curve from the cache, the staleness
    /// edge off by one, and a run not counting as a use.
    #[test]
    fn a_key_idle_more_than_60_days_is_stale_and_has_no_curve() {
        let store = TestStore::new();
        let mut ps = open(&store, 1 << 20, 100);
        learn(&mut ps, "a", 100);
        let p = ps.profile(&pk("a")).unwrap().unwrap();
        assert_eq!(p.last_used_day, 100, "a run that ends is a use");
        assert!(!p.is_stale(160));
        assert!(p.is_stale(161));
        ps.curves().set_today(160);
        assert!(ps.curves().curve(&wk("a")).is_some());
        ps.curves().set_today(161);
        assert!(ps.curves().curve(&wk("a")).is_none());
        // Opening on a later day applies the rule from the start.
        let late = open(&store, 1 << 20, 161);
        assert!(late.curves().curve(&wk("a")).is_none());
        // A run later renews it.
        learn_one(&mut ps, "a", 170);
        ps.curves().set_today(170);
        assert!(ps.curves().curve(&wk("a")).is_some());
    }

    /// Catches: one unreadable row (or a name outside the key grammar) stopping the
    /// server from starting, and a key stuck on its bad row for good.
    #[test]
    fn an_unreadable_row_is_skipped_at_open_and_replaced_by_the_next_run() {
        let store = TestStore::new();
        let put = |key: &str, v: Vec<u8>| {
            store
                .fake
                .txn(Txn::new().put(key, v, Cond::Any))
                .unwrap()
                .unwrap();
        };
        put("usage/key/bad", vec![0xff]);
        put(
            "usage/key/Upper",
            codec::encode(1, &crate::ring::Ring::new()),
        );
        put("usage/key/ok", codec::encode(1, &crate::ring::Ring::new()));
        let mut ps = open(&store, 1 << 20, 10);
        assert_eq!(ps.corrupt_rows(), 2);
        assert_eq!(ps.len(), 1);
        assert!(matches!(
            ps.profile(&pk("bad")),
            Err(ProfileError::Corrupt { .. })
        ));
        assert_eq!(ps.profile(&pk("ok")).unwrap().unwrap().runs, 0);
        learn_one(&mut ps, "bad", 10);
        assert_eq!(ps.profile(&pk("bad")).unwrap().unwrap().runs, 1);
    }

    #[test]
    fn a_failing_scan_fails_the_open() {
        let store = TestStore::new();
        TestStore::set(&store.fail_scan, true);
        let err = ProfileStore::open(store.clone(), curves(), 1, 0)
            .err()
            .unwrap();
        assert!(matches!(err, ProfileError::Store(_)));
        assert!(err.to_string().contains("state store"));
    }
}
