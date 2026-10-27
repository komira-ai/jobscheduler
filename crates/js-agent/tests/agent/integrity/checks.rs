//! One check at a time: the host starts quiet and clean, the baseline is taken, one thing
//! changes in the in-memory host, and the check after the job says what it found.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use js_agent::integrity::{
    FakeHost, HostView, Integrity, Mount, PROCESS_CAP, ProcHost, Process, Watched, code, input,
};
use js_agent::lifecycle::CHECK_DETAIL_MAX;

use super::{JOB_UID, after, beside, mount, process, said, started};
use crate::support::Scratch;

/// Asserts that a process the baseline did not have, whose parent is `init`, is found after
/// the job as a daemonised child, naming the pid and the job, and that a new process of
/// another user whose parent is the agent, one that ended, and one that was already there
/// are not. It catches a child that outlives its job unseen, and a check that calls every
/// new process a finding.
#[test]
fn daemonised_child() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(300, 1, 2000, 900, "0::/user.slice"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::DAEMONISED_CHILD,
            "daemonised-child: pid 300 was left behind by job r1 (parent init)".to_owned()
        )
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(301, 100, 2000, 901, "0::/user.slice"));
    host.start_process(process(302, 100, 2000, 902, "0::/user.slice"));
    host.end_process(302);
    assert!(
        after(&mut checks, "r1").is_ok(),
        "a child of the agent, of another user, is not the job's"
    );
}

/// Asserts that a process of the user the containers run as, which the baseline did not
/// have, is found after the job, that one with the same pid and another start time (a pid
/// reused) is a new process, that a process of that user which was already there at the
/// baseline is not a finding, and that a process that both belongs to the job's user and
/// has `init` as its parent is both findings. It catches a job's leftover process missed,
/// and a pid reuse that hides one.
#[test]
fn process_as_job_user_after_end() {
    let host = FakeHost::quiet();
    host.start_process(process(
        400,
        100,
        JOB_UID,
        700,
        "0::/system.slice/old.service",
    ));
    let mut checks = started(&host, vec![]);
    assert!(
        after(&mut checks, "r0").is_ok(),
        "the process that was there is not new"
    );
    host.start_process(process(401, 100, JOB_UID, 901, "0::/user.slice"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "process-as-job-user: pid 401 of user 65534 is running after job r1".to_owned()
        )
    );

    let host = FakeHost::quiet();
    host.start_process(process(400, 100, JOB_UID, 700, "0::/old"));
    let mut checks = started(&host, vec![]);
    host.end_process(400);
    host.start_process(process(400, 100, JOB_UID, 950, "0::/new"));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        found.code(),
        code::PROCESS_AS_JOB_USER,
        "same pid, new process"
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(500, 1, JOB_UID, 960, "0::/x"));
    let found = after(&mut checks, "r3").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert!(
        found.detail().contains("daemonised-child: pid 500"),
        "{found:?}"
    );
}

/// Asserts that a mount the baseline did not have is found after the job, naming where, what
/// and from what, and that a mount that went away is not. It catches a job that mounts
/// something on the host unseen.
#[test]
fn new_mount() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::NEW_MOUNT,
            "new-mount: /mnt/x (ext4) from /dev/sdb1".to_owned()
        )
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.unmount("/proc");
    assert!(after(&mut checks, "r2").is_ok(), "only a new mount counts");
}

/// Asserts that a line added to the user database, and one added to the group database,
/// are each found after the job under their own code and naming the entry, and that a line
/// changed in place is found too. It catches a job that adds a user or a group, or
/// rewrites an account, unseen.
#[test]
fn new_user() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (true, code::USER_CHANGED, "user-changed: mallory".to_owned())
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.add_group("ops:x:2000:");
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (true, code::GROUP_CHANGED, "group-changed: ops".to_owned())
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.add_user("root:x:0:0:root:/root:/bin/evil");
    let found = after(&mut checks, "r3").unwrap_err();
    assert_eq!(found.code(), code::USER_CHANGED, "same name, another shell");
    assert_eq!(found.detail(), "user-changed: root");
}

/// Asserts that a kernel module the baseline did not have is found after the job, by name,
/// as loaded. It catches a job that loads a module unseen.
#[test]
fn new_kernel_module() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.load_module("evil");
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::MODULE_CHANGED,
            "kernel-module-changed: loaded evil".to_owned()
        )
    );
}

/// Asserts that a change in the digest of a watched directory is found after the job,
/// naming the directory and only that one, and that an unchanged digest is not. It catches
/// a file written to a watched directory unseen, and a check that names the wrong one.
#[test]
fn file_changed_in_watch_dir() {
    let (a, b) = (PathBuf::from("/watch/a"), PathBuf::from("/watch/b"));
    let host = FakeHost::quiet();
    host.watch(&a, Watched::Digest("one".to_owned()));
    host.watch(&b, Watched::Digest("one".to_owned()));
    let mut checks = started(&host, vec![a.clone(), b.clone()]);
    assert!(after(&mut checks, "r0").is_ok());
    host.watch(&b, Watched::Digest("two".to_owned()));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::WATCH_DIR_CHANGED,
            "watch-dir-changed: /watch/b differs from the baseline".to_owned()
        )
    );
}

/// A host whose watched directories are real and everything else is in memory.
struct Mixed {
    fake: FakeHost,
    real: ProcHost,
}

impl HostView for Mixed {
    fn processes(&self) -> Result<Vec<Process>, String> {
        self.fake.processes()
    }
    fn mounts(&self) -> Result<Vec<Mount>, String> {
        self.fake.mounts()
    }
    fn users(&self) -> Result<BTreeSet<String>, String> {
        self.fake.users()
    }
    fn groups(&self) -> Result<BTreeSet<String>, String> {
        self.fake.groups()
    }
    fn modules(&self) -> Result<BTreeSet<String>, String> {
        self.fake.modules()
    }
    fn watched(&self, dir: &Path) -> Result<Watched, String> {
        self.real.watched(dir)
    }
    fn link(&self, path: &str) -> Result<String, String> {
        self.fake.link(path)
    }
    fn file(&self, path: &str) -> Result<String, String> {
        self.fake.file(path)
    }
}

/// Real directories, the first watched, the second not.
fn two_dirs(name: &str) -> (Scratch, PathBuf, PathBuf) {
    let dir = Scratch::new(name);
    let (watched, other) = (dir.path("watched"), dir.path("other"));
    for path in [&watched, &other] {
        fs::create_dir(path).unwrap();
        fs::write(path.join("a"), "a").unwrap();
    }
    (dir, watched, other)
}

/// Asserts, over real directories, that a file created in a directory that is not watched
/// is not a finding (and the check that finds nothing takes a new baseline), while a file
/// created in the watched one, one that grows, and one that is removed are. It catches a
/// check that digests more than it was told to watch, and one that misses a change in a
/// directory it watches.
#[test]
fn file_outside_watch_dir_not_a_finding() {
    let (_dir, watched, other) = two_dirs("integrity-outside");
    let mixed = Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system(),
    };
    let mut checks = Integrity::start(Box::new(mixed), JOB_UID, vec![watched.clone()]).unwrap();
    fs::write(other.join("b"), "outside").unwrap();
    assert!(after(&mut checks, "r1").is_ok());
    fs::write(watched.join("b"), "inside").unwrap();
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::WATCH_DIR_CHANGED);
    assert!(
        found.detail().contains(watched.to_str().unwrap()),
        "{found:?}"
    );

    let (_dir, watched, _other) = two_dirs("integrity-outside-grows");
    let mixed = Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system(),
    };
    let mut checks = Integrity::start(Box::new(mixed), JOB_UID, vec![watched.clone()]).unwrap();
    fs::write(watched.join("a"), "a much longer text than before").unwrap();
    assert_eq!(
        after(&mut checks, "r3").unwrap_err().code(),
        code::WATCH_DIR_CHANGED,
        "a size changed"
    );
    let mixed = Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system(),
    };
    let mut checks = Integrity::start(Box::new(mixed), JOB_UID, vec![watched.clone()]).unwrap();
    fs::remove_file(watched.join("a")).unwrap();
    assert_eq!(
        after(&mut checks, "r4").unwrap_err().code(),
        code::WATCH_DIR_CHANGED,
        "an entry removed"
    );
}

/// Asserts that a watched directory with more entries than the checks read cannot be
/// checked and is a finding (a check that did not run), both from the in-memory host and
/// from the real reader with a small cap, and that a directory that is over the bound when
/// the agent starts stops it from taking a baseline. It catches a bound that makes a
/// large directory pass unchecked.
#[test]
fn watch_listing_over_bound_is_a_finding() {
    let dir = PathBuf::from("/watch/big");
    let host = FakeHost::quiet();
    host.watch(&dir, Watched::Digest("d".to_owned()));
    let mut checks = started(&host, vec![dir.clone()]);
    host.watch(&dir, Watched::TooMany);
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            false,
            code::WATCH_OVER_BOUND,
            "watch-over-bound: /watch/big: more than 4096 entries".to_owned()
        )
    );
    let started_over = Integrity::start(Box::new(host.clone()), JOB_UID, vec![dir]);
    let why = started_over.err().unwrap();
    assert!(
        why.starts_with("cannot take the baseline of the host: watch-over-bound"),
        "{why}"
    );

    let (_dir, watched, _) = two_dirs("integrity-bound");
    fs::write(watched.join("b"), "b").unwrap();
    let small = || Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system().with_caps(PROCESS_CAP, 2),
    };
    let mut checks = Integrity::start(Box::new(small()), JOB_UID, vec![watched.clone()]).unwrap();
    assert!(after(&mut checks, "r2").is_ok(), "exactly the cap is read");
    fs::write(watched.join("c"), "c").unwrap();
    let found = after(&mut checks, "r3").unwrap_err();
    assert_eq!(found.code(), code::WATCH_OVER_BOUND, "one over the cap");
}

/// Asserts that an input that cannot be read is a finding of a check that did not run, for
/// each input: the process table, the mounts, the user and group databases, the modules and
/// a watched directory (one that is not there at all, as well as one that fails); that the
/// finding names the reason; that the baseline is left as it was (a change made meanwhile is
/// still found once the input reads again); and that the agent cannot take a baseline
/// without it. It catches a check that skips what it cannot read and calls the host clean.
#[test]
fn unreadable_input_is_a_finding_not_a_skip() {
    let watched = PathBuf::from("/watch/a");
    let inputs = [
        (input::PROCESSES, code::PROCESSES_UNREADABLE),
        (input::MOUNTS, code::MOUNTS_UNREADABLE),
        (input::USERS, code::USERS_UNREADABLE),
        (input::GROUPS, code::GROUPS_UNREADABLE),
        (input::MODULES, code::MODULES_UNREADABLE),
        (input::WATCH, code::WATCH_UNREADABLE),
    ];
    for (which, expected) in inputs {
        let host = FakeHost::quiet();
        host.watch(&watched, Watched::Digest("d".to_owned()));
        let mut checks = started(&host, vec![watched.clone()]);
        host.fail(which);
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(
            said(&found),
            (
                false,
                expected,
                format!("{expected}: {which}: cannot be read")
            ),
            "{which}"
        );
        let started_blind =
            Integrity::start(Box::new(host.clone()), JOB_UID, vec![watched.clone()]);
        assert!(
            started_blind.err().unwrap().contains(expected),
            "{which}: no baseline without it"
        );
        host.heal(which);
        assert!(after(&mut checks, "r2").is_ok(), "{which}: reads again");
    }

    let host = FakeHost::quiet();
    host.watch(&watched, Watched::Digest("d".to_owned()));
    let mut checks = started(&host, vec![watched.clone()]);
    host.fail(input::MOUNTS);
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    assert!(after(&mut checks, "r3").is_err());
    host.heal(input::MOUNTS);
    let found = after(&mut checks, "r4").unwrap_err();
    assert_eq!(
        found.code(),
        code::NEW_MOUNT,
        "the unread check kept the baseline"
    );

    let missing = FakeHost::quiet();
    let mut blind = Integrity::start(Box::new(missing.clone()), JOB_UID, vec![]).unwrap();
    assert!(after(&mut blind, "r5").is_ok());
    let gone = Integrity::start(
        Box::new(missing),
        JOB_UID,
        vec![PathBuf::from("/not/there")],
    );
    let why = gone.err().unwrap();
    assert!(
        why.contains("watch-unreadable: /not/there: no such directory"),
        "{why}"
    );
}

/// Asserts that when several things are found the finding carries the code of the first in
/// the order of the checks and the detail of all of them, and that a detail longer than the
/// bound is cut. It catches a finding that hides the second cause, and one that can fill
/// the log.
#[test]
fn several_findings_are_one_finding_with_every_detail() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.load_module("evil");
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::NEW_MOUNT);
    assert_eq!(
        found.detail(),
        "new-mount: /mnt/x (ext4) from /dev/sdb1; user-changed: mallory; kernel-module-changed: loaded evil"
    );

    for n in 0..60 {
        host.mount(mount(&format!("/mnt/long-name-{n}"), "/dev/sdc1"));
    }
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.detail().len(), CHECK_DETAIL_MAX);
}

/// Asserts that a check that finds nothing takes a new baseline and one that finds something
/// does not: a mount that went away without a finding is a new mount when it comes back,
/// and a change found once is found again by the next check. It catches a baseline that is
/// never moved forward, and one that is moved forward over a finding (which would clear it).
#[test]
fn the_baseline_moves_forward_only_over_a_clean_check() {
    let host = FakeHost::quiet();
    host.mount(mount("/mnt/data", "/dev/sdb1"));
    let mut checks = started(&host, vec![]);
    host.unmount("/mnt/data");
    assert!(after(&mut checks, "r1").is_ok());
    host.mount(mount("/mnt/data", "/dev/sdb1"));
    assert_eq!(
        after(&mut checks, "r2").unwrap_err().code(),
        code::NEW_MOUNT,
        "the baseline no longer has it"
    );
    assert_eq!(
        after(&mut checks, "r3").unwrap_err().code(),
        code::NEW_MOUNT,
        "and the finding did not move it"
    );
}

/// Asserts that the containers of the agent's other jobs are not counted against a job that
/// finished: a process in the control group of a live container, and a mount of one, are no
/// finding; they are not taken into the baseline either, so what is left of them once the
/// container is gone is found by the check of that job. It catches a job blamed for its
/// neighbour, and a neighbour's leftovers hidden by a baseline taken while it ran.
#[test]
fn a_neighbours_container_is_not_the_jobs() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(600, 500, JOB_UID, 1000, "0::/docker/c2"));
    host.mount(Mount {
        root: "/".to_owned(),
        point: "/var/lib/docker/overlay2/c2/merged".to_owned(),
        fstype: "overlay".to_owned(),
        source: "overlay".to_owned(),
    });
    assert!(beside(&mut checks, "r1", &["c2"]).is_ok());
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        found.code(),
        code::PROCESS_AS_JOB_USER,
        "once c2 is gone, what is left of it is found"
    );
    assert!(found.detail().contains("pid 600"), "{found:?}");
    assert!(found.detail().contains("new-mount"), "{found:?}");

    let wrong = beside(&mut checks, "r3", &["c9"]).unwrap_err();
    assert_eq!(
        wrong.code(),
        code::PROCESS_AS_JOB_USER,
        "another container's id"
    );
    for (root, point, source) in [
        ("/c2/x", "/m", "src"),
        ("/", "/c2", "src"),
        ("/", "/m", "/dev/c2"),
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.mount(Mount {
            root: root.to_owned(),
            point: point.to_owned(),
            fstype: "ext4".to_owned(),
            source: source.to_owned(),
        });
        assert!(
            beside(&mut checks, "r4", &["c2"]).is_ok(),
            "{root} {point} {source}"
        );
    }
}
