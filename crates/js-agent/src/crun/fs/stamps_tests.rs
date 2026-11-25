//! The stamps of a start's lookups ([`LookupStamps`]): each lookup of a host path is stamped
//! on the clock [`Fs::with_clock`] gives it when it is asked (on the caller's thread), when a
//! thread of the pool begins it and when it ends (on that thread), and the run keeps the
//! stamps of the lookup that ended last, which [`Fs::take`] gives with the assembly. The
//! clock here is a script of readings, read in order, so every stamp is the test's.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use js_agent_wire::MountMode;
use js_testkit::dir::TestDir;

use super::tests::{Mounts, Paths, mount, owners, scratch, until};
use super::{Fs, FsSettings, WallClock};
use crate::lifecycle::{FsPrepared, JobFs, LookupStamps, MountRoots};

/// A clock that reads `readings` in order, one for each read on whatever thread, and has
/// no reading after the last: a read past the script fails the lookup that made it.
fn scripted(readings: &[u64]) -> WallClock {
    let left: Mutex<VecDeque<u64>> = Mutex::new(readings.iter().copied().collect());
    Arc::new(move || {
        let mut left = left.lock().expect("the readings");
        left.pop_front().expect("a reading the test scripted")
    })
}

/// The file systems of a test on `clock`: the mounts in `dir`, the root `data` with the
/// directories `a` and `b`.
fn open(dir: &TestDir, clock: WallClock) -> Fs {
    let data = dir.path().join("data");
    for tree in ["a", "b"] {
        fs::create_dir_all(data.join(tree)).expect("a tree");
    }
    let roots = MountRoots::new(&[("data".to_owned(), data)]).expect("roots");
    let settings = FsSettings {
        dir: dir.path().join("mounts"),
        roots,
        calls: Arc::new(Mounts::under(dir.path())),
        paths: Arc::new(Paths::default()),
        image_allow: Vec::new(),
    };
    Fs::open(settings).expect("opened").with_clock(clock)
}

/// A start's file system of a read-only mount of each of `trees` of the root `data`.
fn mounts_of(dir: &Path, trees: &[&str]) -> JobFs {
    let data = dir.join("data");
    let mounts = trees
        .iter()
        .map(|tree| mount(&data.join(tree), &format!("/{tree}"), MountMode::ReadOnly));
    JobFs {
        layers: Vec::new(),
        mounts: mounts.collect(),
    }
}

/// Asks the lookups of `job` for the run `r` until they are all open.
fn resolved(fs_: &mut Fs, job: &JobFs) {
    let ready = until(|| match fs_.resolve(("r", "lease"), "", job, 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    assert_eq!(ready, FsPrepared::Ready);
}

/// Assembles the run `r`, whose lookups are open, and takes what was made: the stamps the
/// run kept.
fn assembled(fs_: &mut Fs, dir: &Path) -> Option<LookupStamps> {
    let lease = dir.join(".lease-lease");
    fs::create_dir_all(&lease).expect("the lease directory");
    fs_.ask_assembly("r", (None, Vec::new()), lease, owners())
        .expect("asked");
    let made = until(|| match fs_.assembled("r", 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    assert_eq!(made, FsPrepared::Ready);
    let (_, stamps) = fs_.take("r").expect("made");
    stamps
}

/// Asserts that the one lookup of a start is stamped queued when it is asked and started
/// and ended on its thread, in that order, and that the spans are the differences of the
/// clock's readings: asked at 1000, begun at 1030 and ended at 1075 waited 30 in the queue
/// and took 45 to open. Catches a stamp read at the wrong place (`started` read when the
/// lookup is asked would make the wait 0), a stamp lost between the lookup and the
/// assembly, and spans that are not the stamps' differences.
#[test]
fn a_lookup_is_stamped_when_asked_begun_and_ended() {
    let dir = scratch("fs-stamps-one");
    let mut fs_ = open(&dir, scripted(&[1000, 1030, 1075]));
    resolved(&mut fs_, &mounts_of(dir.path(), &["a"]));
    let stamps = assembled(&mut fs_, dir.path()).expect("stamped");
    assert_eq!(
        stamps,
        LookupStamps {
            queued_at_ms: 1000,
            started_at_ms: 1030,
            ended_at_ms: 1075,
        }
    );
    assert_eq!((stamps.queue_ms(), stamps.open_ms()), (30, 45));
}

/// Asserts that of a start's two lookups, the run keeps the stamps of the one that ended
/// last, whichever was asked first, and of two that ended at once the one listed first: the
/// first is asked and ends (its three readings), then a second host path is added and its
/// lookup asked and ended (the next three), and the first is kept and not asked again.
/// Catches the first or the last lookup kept instead of the one the start waited on, and a
/// kept lookup asked again (which would read the clock past its script).
#[test]
fn the_run_keeps_the_lookup_that_ended_last() {
    let second = LookupStamps {
        queued_at_ms: 100,
        started_at_ms: 150,
        ended_at_ms: 400,
    };
    let cases = [
        ("later", [10, 20, 30], second),
        (
            "earlier",
            [10, 20, 900],
            LookupStamps {
                queued_at_ms: 10,
                started_at_ms: 20,
                ended_at_ms: 900,
            },
        ),
        (
            "tie",
            [10, 20, 400],
            LookupStamps {
                queued_at_ms: 10,
                started_at_ms: 20,
                ended_at_ms: 400,
            },
        ),
    ];
    for (case, first, kept) in cases {
        let dir = scratch(&format!("fs-stamps-{case}"));
        let readings = [first.as_slice(), &[100, 150, 400]].concat();
        let mut fs_ = open(&dir, scripted(&readings));
        resolved(&mut fs_, &mounts_of(dir.path(), &["a"]));
        resolved(&mut fs_, &mounts_of(dir.path(), &["a", "b"]));
        assert_eq!(assembled(&mut fs_, dir.path()), Some(kept), "{case}");
    }
}

/// Asserts that a start with no host path has no lookup and so no stamps, and reads no
/// clock (its script is empty). Catches stamps made up for a start that waited on no lookup.
#[test]
fn a_start_with_no_host_path_has_no_stamps() {
    let dir = scratch("fs-stamps-none");
    let mut fs_ = open(&dir, scripted(&[]));
    resolved(&mut fs_, &JobFs::default());
    assert_eq!(assembled(&mut fs_, dir.path()), None);
}
