//! [`Interfere`]: a store that delegates to another and, when told, fails a commit or lets a
//! second writer commit first. Test support: the suite uses it to show that an operation
//! writes nothing when its commit fails and that it pins the versions it read.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_state::{
    Changes, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};

/// The text of the [`StoreError::Failed`] a failed commit answers.
pub const INJECTED: &str = "the commit failed (injected)";

/// What runs before the next commit.
type Hook = Box<dyn FnOnce() + Send>;

/// A store that passes every call to the store it wraps, except that the commit
/// [`Interfere::fail_after`] names fails without reaching it, and a hook
/// [`Interfere::before_next`] sets runs just before the next commit does.
pub struct Interfere<S> {
    inner: Arc<S>,
    /// How many commits pass before one fails; `None`: none fails.
    fail_at: Mutex<Option<usize>>,
    before: Mutex<Option<Hook>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<S: Store> Interfere<S> {
    /// A wrapper of `inner` that interferes with nothing yet.
    #[must_use]
    pub fn new(inner: S) -> Interfere<S> {
        Interfere {
            inner: Arc::new(inner),
            fail_at: Mutex::new(None),
            before: Mutex::new(None),
        }
    }

    /// The wrapped store, for a second writer that commits past the interference.
    #[must_use]
    pub fn inner(&self) -> Arc<S> {
        Arc::clone(&self.inner)
    }

    /// The wrapped store back, when nothing else holds it.
    #[must_use]
    pub fn into_inner(self) -> Option<S> {
        Arc::into_inner(self.inner)
    }

    /// Lets `commits` commits through, then fails the next one with
    /// [`StoreError::Failed`] before it reaches the wrapped store.
    pub fn fail_after(&self, commits: usize) {
        *lock(&self.fail_at) = Some(commits);
    }

    /// Fails the next commit ([`Interfere::fail_after`] of 0).
    pub fn fail_next(&self) {
        self.fail_after(0);
    }

    /// Runs `hook` just before the next commit reaches the wrapped store: after the
    /// operation's reads, before its transaction.
    pub fn before_next(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.before) = Some(Box::new(hook));
    }

    /// Whether this commit is the one to fail.
    fn fails_now(&self) -> bool {
        let mut fail_at = lock(&self.fail_at);
        match *fail_at {
            Some(0) => {
                *fail_at = None;
                true
            }
            Some(left) => {
                *fail_at = Some(left - 1);
                false
            }
            None => false,
        }
    }
}

impl<S: Store> Store for Interfere<S> {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        // Taken out before it runs, so the hook's own commits pass straight through.
        let hook = lock(&self.before).take();
        if let Some(hook) = hook {
            hook();
        }
        if self.fails_now() {
            return Err(StoreError::Failed(INJECTED.to_owned()));
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Revision, Store, StoreError, Txn};

    use super::{INJECTED, Interfere};

    fn wrapped() -> Interfere<Fake> {
        Interfere::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ))
    }

    fn put(store: &Interfere<Fake>, key: &str) -> Result<(), StoreError> {
        store
            .txn(Txn::new().put(key, "v", Cond::Any))
            .map(|committed| committed.expect("no precondition"))
            .map(drop)
    }

    // Catches a wrapper that fails the wrong commit, fails more than one, or lets the failed
    // one reach the store.
    #[test]
    fn the_named_commit_fails_and_only_it() {
        let store = wrapped();
        store.fail_after(1);
        put(&store, "a").expect("the first commit passes");
        assert_eq!(
            put(&store, "b"),
            Err(StoreError::Failed(INJECTED.to_owned()))
        );
        put(&store, "c").expect("the commit after passes");
        let keys: Vec<String> = store
            .scan_prefix("")
            .expect("reads")
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, ["a", "c"]);
    }

    // Catches a hook that runs after the commit, or more than once.
    #[test]
    fn the_hook_runs_once_before_the_next_commit() {
        let store = wrapped();
        let ran = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&ran);
        let inner = store.inner();
        store.before_next(move || {
            assert!(
                inner.get("a").expect("reads").is_none(),
                "before the commit"
            );
            seen.store(true, Ordering::SeqCst);
        });
        put(&store, "a").expect("commits");
        assert!(ran.load(Ordering::SeqCst));
        put(&store, "b").expect("commits with no hook");
    }

    // Catches a wrapper that answers a read, the feed, the reaper or the counters itself
    // instead of passing it on.
    #[test]
    fn every_other_call_reaches_the_wrapped_store() {
        let store = wrapped();
        put(&store, "a").expect("commits");
        assert_eq!(store.get("a").expect("reads").expect("present").value, b"v");
        let feed = store.changes(Revision::ZERO, 10).expect("reads the feed");
        assert_eq!(feed.rows.len(), 1);
        assert_eq!(store.reap_expired().expect("reaps"), 0);
        assert_eq!(store.commit_stats().txns, 1);
        assert_eq!(store.now(), store.inner().now());
        assert!(store.into_inner().is_some(), "nothing else holds the store");
    }
}
