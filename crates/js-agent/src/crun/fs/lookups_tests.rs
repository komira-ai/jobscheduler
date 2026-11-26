//! The width of the lookups' pool ([`LOOKUP_THREADS`]): the most host paths one start may
//! name are looked up at once, a lookup asked while every thread is held waits for one to
//! end and is refused at its own bound though no thread took it, and lookups that hang hold
//! no thread of the assemblies and the checks. Each lookup is held by the fake
//! ([`Paths::holding_lookups`]) until the test lets it go, and the lookups are stamped on a
//! clock the test sets, so a wait for a thread is read from the stamps, never from a duration.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent_wire::{JobMount, MountMode, limits, refusal};
use js_testkit::dir::TestDir;

use super::tests::{GUARD, Mounts, Paths, host_layer, mount, owners, scratch, until};
use super::{Fs, FsSettings, LOOKUP_MS, LOOKUP_THREADS, Opened, WallClock};
use crate::lifecycle::{FsPrepared, JobFs, LookupStamps, MountRoots, NativeRefusal};
use crate::pool::Poll;

/// A wall clock that reads what the test last set, 0 at first.
fn set_clock() -> (WallClock, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(0));
    let read = Arc::clone(&now);
    (Arc::new(move || read.load(Ordering::SeqCst)), now)
}

/// The file systems of a test over `paths` on `clock`: the mounts in `dir`, the root `data`
/// with the directories `tree` and `late`.
fn open(dir: &TestDir, paths: Paths, clock: WallClock) -> Fs {
    let data = dir.path().join("data");
    for tree in ["tree", "late"] {
        fs::create_dir_all(data.join(tree)).expect("a directory");
    }
    let roots = MountRoots::new(&[("data".to_owned(), data)]).expect("roots");
    let settings = FsSettings {
        dir: dir.path().join("mounts"),
        roots,
        calls: Arc::new(Mounts::under(dir.path())),
        paths: Arc::new(paths),
        image_allow: Vec::new(),
    };
    Fs::open(settings).expect("opened").with_clock(clock)
}

/// `n` read-only mounts of `tree`, at `/m0`, `/m1` and on.
fn mounts(tree: &Path, n: usize) -> Vec<JobMount> {
    (0..n)
        .map(|i| mount(tree, &format!("/m{i}"), MountMode::ReadOnly))
        .collect()
}

/// A start's file system of `mounts` alone.
fn job(mounts: Vec<JobMount>) -> JobFs {
    JobFs {
        layers: Vec::new(),
        mounts,
    }
}

/// Asks the lookups of the run `run` (lease `lease-<run>`) of `start` until they answer.
fn answered(fs_: &mut Fs, run: &str, start: &JobFs) -> FsPrepared {
    let lease = format!("lease-{run}");
    until(|| match fs_.resolve((run, &lease), "", start, 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    })
}

/// The stamps of every lookup of the run `run`, in the order of its host paths, each open.
fn stamps_of(fs_: &Fs, run: &str) -> Vec<LookupStamps> {
    let prep = fs_.preps.get(run).expect("the run is being prepared");
    let open = prep.lookups.iter().map(|lookup| match &lookup.state {
        Opened::Open(_, stamps) => *stamps,
        Opened::Asked(_) => panic!("the lookup of {} is not open", lookup.path),
    });
    open.collect()
}

/// Asserts that a start naming the most host paths a start may name (every layer and every
/// mount a host path) has every one of its lookups running at once: each lookup is held
/// once it began, and every one begins before any is let go; then each is stamped begun
/// when it was asked, a wait for a thread of 0 ms. Catches a lookups' pool narrower than one
/// start's host paths (four threads, as every pool had, began four and queued the rest
/// behind them, so a start's own lookups waited for one another).
#[test]
fn a_start_naming_the_most_host_paths_has_all_its_lookups_running_at_once() {
    let dir = scratch("fs-lookups-at-once");
    let (paths, began, go) = Paths::holding_lookups();
    let (clock, now) = set_clock();
    let mut fs_ = open(&dir, paths, clock);
    let tree = dir.path().join("data/tree");
    let most = limits::MAX_LAYERS + limits::MAX_MOUNTS;
    let start = JobFs {
        layers: (0..limits::MAX_LAYERS).map(|_| host_layer(&tree)).collect(),
        mounts: mounts(&tree, limits::MAX_MOUNTS),
    };
    assert_eq!(
        fs_.resolve(("r", "lease-r"), "", &start, 0),
        FsPrepared::Pending
    );
    for n in 1..=most {
        let waited = began.recv_timeout(GUARD);
        assert!(
            waited.is_ok(),
            "lookup {n} of {most} did not begin while the others were held"
        );
    }
    now.store(1, Ordering::SeqCst);
    drop(go);
    assert_eq!(answered(&mut fs_, "r", &start), FsPrepared::Ready);
    let at_once = LookupStamps {
        queued_at_ms: 0,
        started_at_ms: 0,
        ended_at_ms: 1,
    };
    assert_eq!(stamps_of(&fs_, "r"), vec![at_once; most]);
    assert!(
        stamps_of(&fs_, "r")
            .iter()
            .all(|stamps| stamps.queue_ms() == 0)
    );
}

/// Asserts that the lookups' pool has a bound: with every thread held by the lookups of one
/// start, a lookup of another start waits for a thread; it is waiting before its bound and
/// refused `input-unavailable` at it, counted from when it was asked though no thread took it,
/// as the held ones are at theirs; once one held lookup ends, its thread takes the waiting
/// one, stamped begun after that end. Catches a pool that makes threads on demand (a hung
/// provider would then hold a thread for every lookup ever asked), a bound counted from when a
/// thread took the lookup, and a lookup that blocks the caller.
#[test]
fn a_lookup_past_the_pools_threads_waits_for_one_to_end_and_its_bound() {
    let dir = scratch("fs-lookups-bound");
    let (paths, began, go) = Paths::holding_lookups();
    let (clock, now) = set_clock();
    let mut fs_ = open(&dir, paths, clock);
    let tree = dir.path().join("data/tree");
    let full = job(mounts(&tree, LOOKUP_THREADS));
    assert_eq!(
        fs_.resolve(("full", "lease-full"), "", &full, 1_000),
        FsPrepared::Pending
    );
    for n in 1..=LOOKUP_THREADS {
        let waited = began.recv_timeout(GUARD);
        assert!(
            waited.is_ok(),
            "held lookup {n} of {LOOKUP_THREADS} did not begin"
        );
    }
    let late_tree = dir.path().join("data/late");
    let late = job(vec![mount(&late_tree, "/late", MountMode::ReadOnly)]);
    assert_eq!(
        fs_.resolve(("late", "lease-late"), "", &late, 2_000),
        FsPrepared::Pending
    );
    let refused = |path: &Path| {
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!(
                "the lookup of {} did not answer in time (bound {LOOKUP_MS} ms)",
                path.display()
            ),
        })
    };
    let bound = 2_000 + LOOKUP_MS;
    assert_eq!(
        fs_.resolve(("late", "lease-late"), "", &late, bound - 1),
        FsPrepared::Pending
    );
    assert_eq!(
        fs_.resolve(("late", "lease-late"), "", &late, bound),
        refused(&late_tree)
    );
    let held_bound = 1_000 + LOOKUP_MS;
    assert_eq!(
        fs_.resolve(("full", "lease-full"), "", &full, held_bound - 1),
        FsPrepared::Pending
    );
    assert_eq!(
        fs_.resolve(("full", "lease-full"), "", &full, held_bound),
        refused(&tree)
    );
    assert!(
        began.try_recv().is_err(),
        "the late lookup waits for a thread"
    );
    // One held lookup ends at 1; the waiting one is begun by the thread it freed, at 1 too.
    now.store(1, Ordering::SeqCst);
    go.send(()).expect("let one held lookup go");
    assert!(began.recv_timeout(GUARD).is_ok(), "the late lookup began");
    drop(go);
    let prep = fs_.preps.get("late").expect("the late run");
    let Some(Opened::Asked(pending)) = prep.lookups.first().map(|lookup| &lookup.state) else {
        panic!("the late lookup is not asked");
    };
    let stamps = until(|| match pending.poll(0) {
        Poll::Answered((Ok(_), stamps)) => Some(stamps),
        Poll::Answered((Err(why), _)) => panic!("the late lookup refused: {why:?}"),
        Poll::Waiting | Poll::Unanswered(_) => None,
    });
    let after_one_ended = LookupStamps {
        queued_at_ms: 0,
        started_at_ms: 1,
        ended_at_ms: 1,
    };
    assert_eq!(stamps, after_one_ended);
}

/// Asserts that lookups that hang (every thread of the lookups' pool held and one more
/// waiting) hold no thread of the assemblies and the checks: another start's assembly is
/// made and a check of its sources answers while they hang, and the hung lookups answer
/// once let go. Catches the lookups put back on the assemblies' threads, or the two pools
/// sharing one.
#[test]
fn lookups_that_hang_hold_no_thread_of_the_assemblies_and_checks() {
    let dir = scratch("fs-lookups-hang");
    let (paths, began, go) = Paths::holding_lookups();
    let (clock, _) = set_clock();
    let mut fs_ = open(&dir, paths, clock);
    let tree = dir.path().join("data/tree");
    let hung = job(mounts(&tree, LOOKUP_THREADS + 1));
    assert_eq!(
        fs_.resolve(("hung", "lease-hung"), "", &hung, 0),
        FsPrepared::Pending
    );
    for n in 1..=LOOKUP_THREADS {
        let waited = began.recv_timeout(GUARD);
        assert!(
            waited.is_ok(),
            "hung lookup {n} of {LOOKUP_THREADS} did not begin"
        );
    }
    let none = JobFs::default();
    assert_eq!(answered(&mut fs_, "free", &none), FsPrepared::Ready);
    let lease = dir.path().join(".lease-free");
    fs::create_dir(&lease).expect("the lease directory");
    fs_.ask_assembly("free", (None, Vec::new()), lease, owners())
        .expect("asked");
    let made = until(|| match fs_.assembled("free", 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    assert_eq!(
        made,
        FsPrepared::Ready,
        "an assembly is not behind the lookups"
    );
    let check = fs_.check("lease-free", 1, 0);
    let checked = until(|| match check.poll(0) {
        Poll::Waiting => None,
        other => Some(other),
    });
    assert_eq!(
        checked,
        Poll::Answered(None),
        "a check is not behind the lookups"
    );
    assert!(
        began.try_recv().is_err(),
        "the lookup past the threads waits"
    );
    drop(go);
    assert_eq!(answered(&mut fs_, "hung", &hung), FsPrepared::Ready);
}
