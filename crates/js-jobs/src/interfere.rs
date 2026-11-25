//! [`Interfere`]: a store that delegates to another and, when told, fails a commit or lets a
//! second writer commit first, holds a scan where a test can wait on it, and keeps the keys
//! read through it. Test support: the suite uses
//! it to show that an operation writes nothing when its commit fails and that it pins the
//! versions it read, and the server's tests to count what a pass reads. It also notes every
//! call made on it while a decision of [`Store::claim_decide`] runs, which is a call on the store
//! from inside an open transaction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_state::{
    Changes, Claim, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, TxnRead, Versioned,
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
    /// Whether the next head read fails ([`Interfere::fail_next_head`]).
    fail_head: Mutex<bool>,
    /// Whether every clock read fails ([`Interfere::fail_clock`]).
    fail_clock: AtomicBool,
    before: Mutex<Option<Hook>>,
    /// What runs before the next scan under a prefix, with that prefix
    /// ([`Interfere::before_next_scan`]).
    before_scan: Mutex<Option<(String, Hook)>>,
    /// The error a failed commit or read answers; [`StoreError::Failed`] with [`INJECTED`]
    /// until [`Interfere::fail_with`] says another.
    error: Mutex<StoreError>,
    /// What each read that reached the wrapped store asked for, in order, since the last
    /// [`Interfere::take_reads`].
    reads: Mutex<Vec<Read>>,
    /// What the decisions of [`Store::claim_decide`] read, in order, since the last
    /// [`Interfere::take_claim_reads`]; apart from `reads`, which the passes' own reads are.
    claim_reads: Arc<Mutex<Vec<Read>>>,
    /// Whether the next get a decision of [`Store::claim_decide`] makes fails
    /// ([`Interfere::fail_next_decision_get`]).
    fail_decision_get: AtomicBool,
    /// Whether a decision of [`Store::claim_decide`] is running.
    deciding: AtomicBool,
    /// The calls made on this store while a decision ran, by name, in order, since the last
    /// [`Interfere::take_calls_while_deciding`].
    while_deciding: Mutex<Vec<&'static str>>,
}

/// The reads of a claimed decision, logged as they are made; a get fails instead when
/// `fail_get` is set, which it clears.
struct LoggedRead<'a> {
    read: &'a mut dyn TxnRead,
    log: &'a Mutex<Vec<Read>>,
    fail_get: &'a AtomicBool,
    error: &'a Mutex<StoreError>,
}

impl TxnRead for LoggedRead<'_> {
    fn get(&mut self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.fail_get.swap(false, Ordering::SeqCst) {
            return Err(lock(self.error).clone());
        }
        lock(self.log).push(Read::Get(key.to_owned()));
        self.read.get(key)
    }

    fn scan_prefix(&mut self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        lock(self.log).push(Read::Scan(prefix.to_owned()));
        self.read.scan_prefix(prefix)
    }
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
            fail_head: Mutex::new(false),
            fail_clock: AtomicBool::new(false),
            before: Mutex::new(None),
            before_scan: Mutex::new(None),
            error: Mutex::new(StoreError::Failed(INJECTED.to_owned())),
            reads: Mutex::new(Vec::new()),
            claim_reads: Arc::new(Mutex::new(Vec::new())),
            fail_decision_get: AtomicBool::new(false),
            deciding: AtomicBool::new(false),
            while_deciding: Mutex::new(Vec::new()),
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

    /// Fails the next read of the store's head ([`Store::head`]) with [`StoreError::Failed`],
    /// or the error [`Interfere::fail_with`] set, before it reaches the wrapped store; the head
    /// reads after it pass. Head reads are not counted by [`Interfere::fail_read_after`].
    pub fn fail_next_head(&self) {
        *lock(&self.fail_head) = true;
    }

    /// Fails every read of the store's clock ([`Store::now`]) while `failing`, with
    /// [`StoreError::Failed`] or the error [`Interfere::fail_with`] set, before it reaches the
    /// wrapped store: a store whose database cannot be read. The other calls are not failed by
    /// it, so a test shows what a caller does with the clock alone gone.
    pub fn fail_clock(&self, failing: bool) {
        self.fail_clock.store(failing, Ordering::SeqCst);
    }

    /// Fails the next get that a decision of [`Store::claim_decide`] makes through the view it
    /// is given, with [`StoreError::Failed`] or the error [`Interfere::fail_with`] set: a
    /// database that fails inside a claimed transaction. Such gets are not counted by
    /// [`Interfere::fail_read_after`], and the gets after it pass.
    pub fn fail_next_decision_get(&self) {
        self.fail_decision_get.store(true, Ordering::SeqCst);
    }

    /// Fails the next commit ([`Interfere::fail_after`] of 0).
    pub fn fail_next(&self) {
        self.fail_after(0);
    }

    /// Makes every commit or read that fails from now on, as [`Interfere::fail_after`] and
    /// [`Interfere::fail_read_after`] say, answer `error` instead of [`StoreError::Failed`]:
    /// a test of how a caller reacts to a particular failure, such as [`StoreError::Disconnected`].
    pub fn fail_with(&self, error: StoreError) {
        *lock(&self.error) = error;
    }

    /// Runs `hook` just before the next commit reaches the wrapped store: after the
    /// operation's reads, before its transaction.
    pub fn before_next(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.before) = Some(Box::new(hook));
    }

    /// Runs `hook` just before the next scan (a prefix or a range scan) under exactly `prefix`
    /// reaches the wrapped store, on the thread that scans: a hook that waits holds the reader
    /// there, as a slow read would, until it returns. Scans under other prefixes pass by it.
    pub fn before_next_scan(&self, prefix: &str, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.before_scan) = Some((prefix.to_owned(), Box::new(hook)));
    }

    /// The reads that reached the wrapped store since the last call, in the order they came,
    /// taken: the next call answers only the reads made after this one. A read failed by
    /// [`Interfere::fail_read_after`] never reached it and is not logged.
    pub fn take_reads(&self) -> Vec<Read> {
        std::mem::take(&mut *lock(&self.reads))
    }

    /// What the decisions of claimed commits read since the last call, in order.
    pub fn take_claim_reads(&self) -> Vec<Read> {
        std::mem::take(&mut *lock(&self.claim_reads))
    }

    /// The calls made on this store (by the name of the [`Store`] method) while a decision of
    /// [`Store::claim_decide`] ran, since the last call, taken. A decision reads through the
    /// [`TxnRead`] it is given; a call on the store itself from inside it needs a second
    /// connection while the decision's transaction holds the first.
    pub fn take_calls_while_deciding(&self) -> Vec<&'static str> {
        std::mem::take(&mut *lock(&self.while_deciding))
    }

    /// Notes the call `name` when a decision is running.
    fn called(&self, name: &'static str) {
        if self.deciding.load(Ordering::SeqCst) {
            lock(&self.while_deciding).push(name);
        }
    }

    /// Runs the hook of [`Interfere::before_next_scan`] when it waits for a scan under `prefix`.
    fn scanning(&self, prefix: &str) {
        let hook = {
            let mut slot = lock(&self.before_scan);
            match slot.take() {
                Some((wanted, hook)) if wanted == prefix => Some(hook),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        if let Some(hook) = hook {
            hook();
        }
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
        self.called("apply_batch");
        // Taken out before it runs, so the hook's own commits pass straight through.
        let hook = lock(&self.before).take();
        if let Some(hook) = hook {
            hook();
        }
        self.countdown(&self.fail_at)?;
        self.inner.apply_batch(batch)
    }

    fn head(&self) -> Result<Option<Revision>, StoreError> {
        self.called("head");
        if std::mem::take(&mut *lock(&self.fail_head)) {
            return Err(lock(&self.error).clone());
        }
        self.inner.head()
    }

    fn claim_decide(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, StoreError> {
        self.called("claim_decide");
        // A decided commit is a commit: the hook and the failure countdown of commits apply.
        let hook = lock(&self.before).take();
        if let Some(hook) = hook {
            hook();
        }
        self.countdown(&self.fail_at)?;
        let log = &self.claim_reads;
        let deciding = &self.deciding;
        let (fail_get, error) = (&self.fail_decision_get, &self.error);
        self.inner.claim_decide(claim, &mut |read| {
            deciding.store(true, Ordering::SeqCst);
            let mut logged = LoggedRead {
                read,
                log,
                fail_get,
                error,
            };
            let decided = decide(&mut logged);
            deciding.store(false, Ordering::SeqCst);
            decided
        })
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.called("get");
        self.log(Read::Get(key.to_owned()))?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.called("scan_prefix");
        self.scanning(prefix);
        self.log(Read::Scan(prefix.to_owned()))?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.called("scan_range");
        self.scanning(scan.prefix);
        self.log(Read::Scan(scan.prefix.to_owned()))?;
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.called("changes");
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.called("trim_feed");
        self.inner.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.called("reap_expired");
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.called("commit_stats");
        self.inner.commit_stats()
    }

    fn now(&self) -> Result<StoreTime, StoreError> {
        self.called("now");
        if self.fail_clock.load(Ordering::SeqCst) {
            return Err(lock(&self.error).clone());
        }
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

    // Catches a head failure that is not the error set, fails more than the next head read, or
    // reaches the wrapped store's head anyway.
    #[test]
    fn the_next_head_read_fails_and_only_it() {
        let store = wrapped();
        put(&store, "a").expect("commits");
        assert_eq!(store.head(), Ok(Some(Revision(1))));
        store.fail_next_head();
        assert_eq!(store.head(), Err(StoreError::Failed(INJECTED.to_owned())));
        assert_eq!(
            store.head(),
            Ok(Some(Revision(1))),
            "only the next head read fails"
        );
        assert_eq!(
            store.take_reads(),
            Vec::<Read>::new(),
            "a head read is not a logged read"
        );
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

    // Catches a head that is not the inner store's: it moves with a commit, and a decision made
    // through the wrapper (which counts as a commit for the hook and the failure countdown) is
    // the inner store's.
    #[test]
    fn the_head_and_a_claimed_decision_are_the_inner_stores() {
        use js_state::Claim;
        let store = wrapped();
        let before = store.head().expect("answers");
        put(&store, "q").expect("commits");
        assert_ne!(
            store.head().expect("answers"),
            before,
            "a commit moves the head"
        );
        let claim = ["q".to_owned()];
        let placed = store
            .claim_decide(&claim, &mut |_| {
                Some(Txn::new().put("p", "1", Cond::Absent))
            })
            .expect("answers");
        assert!(matches!(placed, Claim::Committed(_)), "{placed:?}");
        store.fail_next();
        let failed = store.claim_decide(&claim, &mut |_| None);
        assert_eq!(failed, Err(StoreError::Failed(INJECTED.to_owned())));
    }

    // Catches a wrapper that does not see a call made on it from inside a claimed decision
    // (the suite counts on it to show a placement makes none), and one that counts calls made
    // outside a decision: only the call inside is noted, by name, and taking the notes empties
    // them.
    #[test]
    fn a_call_made_while_a_decision_runs_is_noted() {
        use js_state::Claim;

        let store = wrapped();
        put(&store, "p").expect("put");
        let claim = ["p".to_owned()];
        let decided = store
            .claim_decide(&claim, &mut |_| {
                let _ = store.now();
                Some(Txn::new().put("p", "2", Cond::Any))
            })
            .expect("answers");
        assert!(matches!(decided, Claim::Committed(_)), "{decided:?}");
        let _ = store.now();
        assert_eq!(store.take_calls_while_deciding(), vec!["now"]);
        assert!(store.take_calls_while_deciding().is_empty());
    }

    // Catches an injected error that is not the one told: a failed commit and a failed read
    // each answer the error `fail_with` set, and the default stays `Failed` until then.
    #[test]
    fn a_failed_call_answers_the_error_it_was_told() {
        let store = wrapped();
        store.fail_with(StoreError::Disconnected("gone".to_owned()));
        store.fail_next();
        assert_eq!(
            put(&store, "a"),
            Err(StoreError::Disconnected("gone".to_owned()))
        );
        store.fail_read_after(0);
        assert_eq!(
            store.get("a"),
            Err(StoreError::Disconnected("gone".to_owned()))
        );
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

    // Catches a clock switch that fails other calls too, answers a reading while it is on, or
    // stays on once turned off: while it is on the clock answers the error told and a read
    // and a commit still pass; off, the clock reads the wrapped store's again.
    #[test]
    fn a_failed_clock_answers_its_error_and_nothing_else_fails() {
        let store = wrapped();
        store.fail_with(StoreError::Disconnected("gone".to_owned()));
        store.fail_clock(true);
        assert_eq!(
            store.now(),
            Err(StoreError::Disconnected("gone".to_owned()))
        );
        put(&store, "a").expect("a commit passes");
        assert!(store.get("a").expect("a read passes").is_some());
        store.fail_clock(false);
        assert_eq!(store.now(), store.inner().now());
    }
}
