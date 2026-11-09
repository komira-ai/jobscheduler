//! [`Interfere`]: a store that delegates to another and, when told, fails a commit or lets a
//! second writer commit first, and keeps the keys read through it. Test support: the suite uses
//! it to show that an operation writes nothing when its commit fails and that it pins the
//! versions it read, and the server's tests to count what a pass reads.

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
/// [`Interfere::fail_after`] names, and the read [`Interfere::fail_read_after`] names, fail
/// without reaching it, and a hook [`Interfere::before_next`] sets runs just before the next
/// commit does. Every read that reaches the wrapped store is logged ([`Interfere::take_reads`]).
pub struct Interfere<S> {
    inner: Arc<S>,
    /// How many commits pass before one fails; `None`: none fails.
    fail_at: Mutex<Option<usize>>,
    /// How many reads (gets and scans) pass before one fails; `None`: none fails.
    fail_read_at: Mutex<Option<usize>>,
    before: Mutex<Option<Hook>>,
    /// The error a failed commit or read answers; [`StoreError::Failed`] with [`INJECTED`]
    /// until [`Interfere::fail_with`] says another.
    error: Mutex<StoreError>,
    /// What each read that reached the wrapped store asked for, in order, since the last
    /// [`Interfere::take_reads`].
    reads: Mutex<Vec<Read>>,
}

/// One read a store was asked for: the key of a get, or the prefix of a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// A get of the key.
    Get(String),
    /// A prefix or range scan under the prefix.
    Scan(String),
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
            fail_read_at: Mutex::new(None),
            before: Mutex::new(None),
            error: Mutex::new(StoreError::Failed(INJECTED.to_owned())),
            reads: Mutex::new(Vec::new()),
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

    /// Lets `reads` reads (a get, a prefix scan or a range scan each) through, then fails the
    /// next one with [`StoreError::Failed`] before it reaches the wrapped store.
    pub fn fail_read_after(&self, reads: usize) {
        *lock(&self.fail_read_at) = Some(reads);
    }

    /// Fails the next commit ([`Interfere::fail_after`] of 0).
    pub fn fail_next(&self) {
        self.fail_after(0);
    }

    /// Makes every commit or read that fails from now on, as [`Interfere::fail_after`] and
    /// [`Interfere::fail_read_after`] say, answer `error` instead of [`StoreError::Failed`]:
    /// a test of how a caller reacts to [`StoreError::Fenced`], which no retry answers.
    pub fn fail_with(&self, error: StoreError) {
        *lock(&self.error) = error;
    }

    /// Runs `hook` just before the next commit reaches the wrapped store: after the
    /// operation's reads, before its transaction.
    pub fn before_next(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.before) = Some(Box::new(hook));
    }

    /// The reads that reached the wrapped store since the last call, in the order they came,
    /// taken: the next call answers only the reads made after this one. A read failed by
    /// [`Interfere::fail_read_after`] never reached it and is not logged.
    pub fn take_reads(&self) -> Vec<Read> {
        std::mem::take(&mut *lock(&self.reads))
    }

    /// Logs `read` once it passed [`Interfere::fail_read_after`].
    fn log(&self, read: Read) -> Result<(), StoreError> {
        self.countdown(&self.fail_read_at)?;
        lock(&self.reads).push(read);
        Ok(())
    }

    /// The error of [`Interfere::fail_with`] when this call is the one `slot` counts down to.
    fn countdown(&self, slot: &Mutex<Option<usize>>) -> Result<(), StoreError> {
        let mut left = lock(slot);
        match *left {
            Some(0) => {
                *left = None;
                Err(lock(&self.error).clone())
            }
            Some(n) => {
                *left = Some(n - 1);
                Ok(())
            }
            None => Ok(()),
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
        self.countdown(&self.fail_at)?;
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.log(Read::Get(key.to_owned()))?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.log(Read::Scan(prefix.to_owned()))?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.log(Read::Scan(scan.prefix.to_owned()))?;
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
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

    use super::{INJECTED, Interfere, Read};

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

    // Catches a wrapper that does not pass a feed trim to the store it wraps: the rows go
    // from the wrapped store, and the count is the store's.
    #[test]
    fn a_feed_trim_reaches_the_wrapped_store() {
        let store = wrapped();
        put(&store, "a").expect("put");
        put(&store, "b").expect("put");
        assert_eq!(store.trim_feed(Revision(1)), Ok(1));
        assert_eq!(
            store.changes(Revision::ZERO, 10),
            Err(StoreError::StaleCursor {
                after: Revision::ZERO,
                floor: Revision(1),
            })
        );
        let left = store.changes(Revision(1), 10).map(|page| page.rows.len());
        assert_eq!(left, Ok(1));
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

    // Catches a read failure that hits the wrong read, more than one, or a commit.
    #[test]
    fn the_named_read_fails_and_only_it() {
        let store = wrapped();
        put(&store, "a").expect("commits");
        store.fail_read_after(1);
        assert!(store.get("a").expect("the first read passes").is_some());
        assert_eq!(
            store.scan_prefix(""),
            Err(StoreError::Failed(INJECTED.to_owned()))
        );
        assert_eq!(
            store.scan_prefix("").expect("the next read passes").len(),
            1
        );
        store.fail_read_after(0);
        let range = js_state::ScanRange::new("", std::num::NonZeroUsize::MIN);
        assert_eq!(
            store.scan_range(range),
            Err(StoreError::Failed(INJECTED.to_owned()))
        );
        store.fail_read_after(0);
        assert_eq!(store.get("a"), Err(StoreError::Failed(INJECTED.to_owned())));
        put(&store, "b").expect("a commit does not count as a read");
    }

    // Catches a read log that misses a kind of read, logs a read that failed before it reached
    // the store, or answers a read twice: each read is logged once, in order, and taken.
    #[test]
    fn each_read_that_reaches_the_store_is_logged_once() {
        let store = wrapped();
        put(&store, "a").expect("commits");
        assert_eq!(store.take_reads(), [], "a commit is no read");
        store.get("a").expect("reads");
        store.scan_prefix("p/").expect("reads");
        let range = js_state::ScanRange::new("r/", std::num::NonZeroUsize::MIN);
        store.scan_range(range).expect("reads");
        store.fail_read_after(0);
        assert!(store.get("b").is_err());
        let logged = [
            Read::Get("a".to_owned()),
            Read::Scan("p/".to_owned()),
            Read::Scan("r/".to_owned()),
        ];
        assert_eq!(store.take_reads(), logged);
        assert_eq!(store.take_reads(), []);
    }

    // Catches an injected error that is not the one told: a failed commit and a failed read
    // each answer the error `fail_with` set, and the default stays `Failed` until then.
    #[test]
    fn a_failed_call_answers_the_error_it_was_told() {
        let store = wrapped();
        store.fail_with(StoreError::Fenced);
        store.fail_next();
        assert_eq!(put(&store, "a"), Err(StoreError::Fenced));
        store.fail_read_after(0);
        assert_eq!(store.get("a"), Err(StoreError::Fenced));
        put(&store, "b").expect("nothing is failing now");
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
