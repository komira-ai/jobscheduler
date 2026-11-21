//! One check at a time: the host starts quiet and clean, the baseline is taken, one thing
//! changes in the in-memory host, and the check after the job says what it found.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use js_agent::integrity::{
    FakeHost, HostView, Integrity, Mount, PROCESS_CAP, ProcHost, Process, Watched, code, input,
};
use js_agent::lifecycle::{CHECK_DETAIL_MAX, CheckFinding, CheckHook as _, CheckNote};

use super::{
    JOB_UID, JOB_USERS, after, beside, codes, mount, noted_after, process, process_named, quiet,
    quiet_beside, said, started,
};

/// The control group the fake host gives the agent, below which a job's leftover can be.
const AGENT_GROUP: &str = "0::/system.slice/agent.service";
use crate::support::Scratch;

/// Asserts that a process the baseline did not have, whose parent is `init`, is a note after
/// the job (the check passes), naming the pid, its owner and the job, and that a new process
/// of another user whose parent is the agent, one that ended, and one that was already there
/// are no note at all. It catches a daemonised child that the log never names, and a check
/// that notes every new process.
#[test]
fn daemonised_child() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(300, 1, 2000, 900, AGENT_GROUP));
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::DAEMONISED_CHILD,
            "pid 300 (stray, state S, cgroup 0::/system.slice/agent.service, owner 2000) is a new child of init after job r1"
        )]
    );
    assert!(quiet(&mut checks, "r2"), "the baseline took it");

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(301, 100, 2000, 901, "0::/user.slice"));
    host.start_process(process(302, 100, 2000, 902, "0::/user.slice"));
    host.end_process(302);
    assert!(
        quiet(&mut checks, "r1"),
        "a child of the agent, of another user, is not the job's"
    );
}

/// Asserts that a process of the user the containers run as, which the baseline did not
/// have, is a finding after the job (a taint) that names the pid, the owner and the job, that
/// one with the same pid and another start time (a pid reused) is a new process, that a
/// process of that user which was already there at the baseline is not a finding, and that a
/// process that both belongs to the job's user and has `init` as its parent is the finding
/// and, beside it, the `daemonised-child` note. It catches a job's leftover process missed,
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
        quiet(&mut checks, "r0"),
        "the process that was there is not new"
    );
    host.start_process(process(
        401,
        100,
        JOB_UID,
        901,
        "0::/machine.slice/libpod-9e1.scope",
    ));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 401 (stray, state S, cgroup 0::/machine.slice/libpod-9e1.scope, owner 65534) is running after job r1".to_owned()
        )
    );
    assert!(checks.notes().is_empty(), "nothing else changed");

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
    host.start_process(process(
        500,
        1,
        JOB_UID,
        960,
        "0::/system.slice/agent.service/x",
    ));
    let found = after(&mut checks, "r3").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert!(found.detail().starts_with("pid 500"), "{found:?}");
    let notes = checks.notes();
    assert_eq!(codes(&notes), [code::DAEMONISED_CHILD]);
    assert!(notes[0].detail.starts_with("pid 500"), "{notes:?}");
}

/// Asserts that a mount the baseline did not have is a note after the job, naming where,
/// what and from what, that the check passes, and that a mount that went away is no note.
/// It catches a job's mount that the log never names, and a taint for a mount.
#[test]
fn new_mount() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/mnt/x (ext4) from /dev/sdb1"
        )]
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.unmount("/proc");
    assert!(quiet(&mut checks, "r2"), "only a new mount counts");
}

/// Asserts that a line added to the user database, and one added to the group database,
/// are each a note under their own code naming the account as added, that a line removed
/// is `removed <name>`, that a line replaced is `changed <name>`, and that the check passes
/// for each. A package install adds accounts while jobs run, and a contained job cannot write
/// `/etc`. It catches a taint for an account change, and a note that does not say what
/// happened to the account.
#[test]
fn new_user() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(code::USER_CHANGED, "added mallory")]
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.add_group("ops:x:2000:");
    assert_eq!(
        noted_after(&mut checks, "r2"),
        [CheckNote::new(code::GROUP_CHANGED, "added ops")]
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.add_user("root:x:0:0:root:/root:/bin/evil");
    assert_eq!(
        noted_after(&mut checks, "r3"),
        [CheckNote::new(code::USER_CHANGED, "added root")],
        "a second line for a name"
    );
}

/// Asserts that a kernel module the baseline did not have is a note after the job, by name,
/// as loaded, and that the check passes. It catches a taint for a module, which the host's
/// own tools (`ss` loads `inet_diag`) and the kernel's autoload cause.
#[test]
fn new_kernel_module() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.load_module("evil");
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(code::MODULE_CHANGED, "loaded evil")]
    );
}

/// Asserts that a change in the digest of a watched directory is a note after the job,
/// naming the directory and only that one, that the check passes, and that an unchanged
/// digest is no note. It catches a file written to a watched directory that the log never
/// names, and a check that names the wrong one.
#[test]
fn file_changed_in_watch_dir() {
    let (a, b) = (PathBuf::from("/watch/a"), PathBuf::from("/watch/b"));
    let host = FakeHost::quiet();
    host.watch(&a, Watched::Digest("one".to_owned()));
    host.watch(&b, Watched::Digest("one".to_owned()));
    let mut checks = started(&host, vec![a.clone(), b.clone()]);
    assert!(quiet(&mut checks, "r0"));
    host.watch(&b, Watched::Digest("two".to_owned()));
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::WATCH_DIR_CHANGED,
            "/watch/b differs from the baseline"
        )]
    );
    assert!(quiet(&mut checks, "r2"), "the baseline took it");
}

/// Asserts that a note names the command line of a process, trimmed as the host reads it, and
/// the owner as `name (uid)` when the user database has a line for the uid and as the uid
/// alone when it has none, in both the finding of a job's process and the note of a daemonised
/// child; and that a process without a command line (a kernel thread, or one that ended) has
/// no `cmd` part. It catches a finding that names only a pid, and an owner or a command line
/// that is not the process's own.
#[test]
fn processes_are_named_with_their_command_line_and_owner() {
    let host = FakeHost::quiet();
    host.add_user("builder:x:65534:65534::/home/builder:/bin/sh");
    let mut checks = started(&host, vec![]);
    host.start_process(process_named(
        process(300, 100, JOB_UID, 900, "0::/machine.slice/libpod-9e1.scope"),
        "sleep",
        'S',
    ));
    host.set_cmdline(300, "sleep 3600");
    host.start_process(process(301, 1, 0, 901, AGENT_GROUP));
    host.set_cmdline(301, "/usr/bin/daemon --flag");
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 300 (sleep, state S, cgroup 0::/machine.slice/libpod-9e1.scope, owner builder (65534), cmd \"sleep 3600\") is running after job r1".to_owned()
        )
    );
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::DAEMONISED_CHILD,
            "pid 301 (stray, state S, cgroup 0::/system.slice/agent.service, owner root (0), cmd \"/usr/bin/daemon --flag\") is a new child of init after job r1"
        )]
    );

    host.start_process(process(302, 1, 2000, 902, AGENT_GROUP));
    host.set_cmdline(302, "");
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER, "still running");
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::DAEMONISED_CHILD,
            "pid 302 (stray, state S, cgroup 0::/system.slice/agent.service, owner 2000) is a new child of init after job r2"
        )]
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
/// is no note (and the check takes a new baseline), while a file created in the watched one,
/// one that grows, and one that is removed are each a `watch-dir-changed` note naming the
/// directory, with the check passing. It catches a check that digests more than it was told
/// to watch, and one that misses a change in a directory it watches.
#[test]
fn file_outside_watch_dir_is_not_noted() {
    let (_dir, watched, other) = two_dirs("integrity-outside");
    let mixed = Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system(),
    };
    let mut checks = Integrity::start(Box::new(mixed), JOB_USERS, vec![watched.clone()]).unwrap();
    fs::write(other.join("b"), "outside").unwrap();
    assert!(quiet(&mut checks, "r1"));
    fs::write(watched.join("b"), "inside").unwrap();
    let notes = noted_after(&mut checks, "r2");
    assert_eq!(
        notes,
        [CheckNote::new(
            code::WATCH_DIR_CHANGED,
            &format!("{} differs from the baseline", watched.display())
        )]
    );

    let (_dir, watched, _other) = two_dirs("integrity-outside-grows");
    let mixed = Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system(),
    };
    let mut checks = Integrity::start(Box::new(mixed), JOB_USERS, vec![watched.clone()]).unwrap();
    fs::write(watched.join("a"), "a much longer text than before").unwrap();
    assert_eq!(
        codes(&noted_after(&mut checks, "r3")),
        [code::WATCH_DIR_CHANGED],
        "a size changed"
    );
    let mixed = Mixed {
        fake: FakeHost::quiet(),
        real: ProcHost::system(),
    };
    let mut checks = Integrity::start(Box::new(mixed), JOB_USERS, vec![watched.clone()]).unwrap();
    fs::remove_file(watched.join("a")).unwrap();
    assert_eq!(
        codes(&noted_after(&mut checks, "r4")),
        [code::WATCH_DIR_CHANGED],
        "an entry removed"
    );
}

/// Asserts that a watched directory with more entries than the checks read cannot be
/// digested and is a `watch-over-bound` note (the check passes and takes the baseline's
/// digest of it, so nothing else is noted), both from the in-memory host and from the real
/// reader with a small cap, and that a directory that is over the bound when the agent starts
/// stops it from taking a baseline. It catches a bound that makes a large directory taint
/// the host, and one that passes unseen.
#[test]
fn watch_listing_over_bound_is_a_note() {
    let dir = PathBuf::from("/watch/big");
    let host = FakeHost::quiet();
    host.watch(&dir, Watched::Digest("d".to_owned()));
    let mut checks = started(&host, vec![dir.clone()]);
    host.watch(&dir, Watched::TooMany);
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::WATCH_OVER_BOUND,
            "/watch/big: more than 4096 entries"
        )]
    );
    let started_over = Integrity::start(Box::new(host.clone()), JOB_USERS, vec![dir]);
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
    let mut checks = Integrity::start(Box::new(small()), JOB_USERS, vec![watched.clone()]).unwrap();
    assert!(quiet(&mut checks, "r2"), "exactly the cap is read");
    fs::write(watched.join("c"), "c").unwrap();
    assert_eq!(
        codes(&noted_after(&mut checks, "r3")),
        [code::WATCH_OVER_BOUND],
        "one over the cap"
    );
}

/// Asserts that an unreadable process table is the one input whose failure is a finding (a
/// check that did not run, an unchecked taint naming the reason) and that every other input
/// that cannot be read (the mounts, the user and group databases, the modules and a watched
/// directory, one that is not there at all as well as one that fails) is a note named by its
/// own code with the reason, the check passing; that the agent cannot take a baseline
/// without any of them; and that once the input reads again the next check passes quietly. It
/// catches a check that skips the process table it cannot read and calls the host clean, and
/// a taint for an input of a check that no longer taints.
#[test]
fn an_unreadable_process_table_is_unchecked_and_any_other_input_is_a_note() {
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
        let reason = format!("{which}: cannot be read");
        match which {
            input::PROCESSES => {
                let found = after(&mut checks, "r1").unwrap_err();
                assert_eq!(said(&found), (false, expected, reason), "{which}");
                assert!(checks.notes().is_empty(), "{which}");
            }
            _ => assert_eq!(
                noted_after(&mut checks, "r1"),
                [CheckNote::new(expected, &reason)],
                "{which}"
            ),
        }
        let started_blind =
            Integrity::start(Box::new(host.clone()), JOB_USERS, vec![watched.clone()]);
        assert!(
            started_blind.err().unwrap().contains(expected),
            "{which}: no baseline without it"
        );
        host.heal(which);
        assert!(quiet(&mut checks, "r2"), "{which}: reads again");
    }

    let missing = FakeHost::quiet();
    let mut blind = Integrity::start(Box::new(missing.clone()), JOB_USERS, vec![]).unwrap();
    assert!(quiet(&mut blind, "r5"));
    let gone = Integrity::start(
        Box::new(missing),
        JOB_USERS,
        vec![PathBuf::from("/not/there")],
    );
    let why = gone.err().unwrap();
    assert!(
        why.contains("watch-unreadable: /not/there: no such directory"),
        "{why}"
    );
}

/// Asserts that an input that cannot be read takes the baseline's value, so the check sees no
/// change in it: a mount made while the mounts cannot be read is not noted then, and is noted
/// once they read again (the baseline never took it); a watched directory that cannot be
/// listed and one that holds too many entries are not `watch-dir-changed` either; and that an
/// unreadable process table leaves the baseline alone, so a change made meanwhile is noted by
/// the next check that can read. It catches an unreadable input read as empty, which would
/// note every mount, account or module as removed.
#[test]
fn an_unread_input_is_taken_as_unchanged() {
    let watched = PathBuf::from("/watch/a");
    let host = FakeHost::quiet();
    host.watch(&watched, Watched::Digest("d".to_owned()));
    let mut checks = started(&host, vec![watched.clone()]);
    host.fail(input::MOUNTS);
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    assert_eq!(
        codes(&noted_after(&mut checks, "r1")),
        [code::MOUNTS_UNREADABLE],
        "no mount is seen to have gone or come"
    );
    host.heal(input::MOUNTS);
    assert_eq!(
        noted_after(&mut checks, "r2"),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/mnt/x (ext4) from /dev/sdb1"
        )]
    );

    host.fail(input::USERS);
    host.fail(input::GROUPS);
    host.fail(input::MODULES);
    host.fail(input::WATCH);
    assert_eq!(
        codes(&noted_after(&mut checks, "r3")),
        [
            code::USERS_UNREADABLE,
            code::GROUPS_UNREADABLE,
            code::MODULES_UNREADABLE,
            code::WATCH_UNREADABLE
        ],
        "every other input is read as it was"
    );
    host.heal(input::WATCH);
    host.watch(&watched, Watched::TooMany);
    assert_eq!(
        codes(&noted_after(&mut checks, "r4")),
        [
            code::USERS_UNREADABLE,
            code::GROUPS_UNREADABLE,
            code::MODULES_UNREADABLE,
            code::WATCH_OVER_BOUND
        ]
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.fail(input::PROCESSES);
    host.mount(mount("/mnt/y", "/dev/sdc1"));
    assert!(after(&mut checks, "r5").is_err());
    host.heal(input::PROCESSES);
    assert_eq!(
        codes(&noted_after(&mut checks, "r6")),
        [code::NEW_MOUNT],
        "the unread check kept the baseline"
    );
}

/// Asserts that when several of a job's processes are left the finding is one finding with
/// every process in its detail, and a detail longer than the bound is cut; that several
/// changes of the host are one note for each code, in the order of the table, with every name
/// joined by a comma, and a note longer than the bound is cut. It catches a finding that
/// hides the second process, a note that hides the second kind of change, and a detail that
/// can fill the log.
#[test]
fn several_leftovers_are_one_finding_and_several_changes_are_a_note_each() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(700, 100, JOB_UID, 970, "0::/a"));
    host.start_process(process(701, 100, JOB_UID, 971, "0::/b"));
    host.load_module("evil");
    host.unload_module("ext4");
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    host.add_user("eve:x:1002:1002::/home/eve:/bin/sh");
    host.add_group("ops:x:2000:");
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    host.mount(mount("/mnt/y", "/dev/sdb2"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 700 (stray, state S, cgroup 0::/a, owner 65534) is running after job r1, pid 701 (stray, state S, cgroup 0::/b, owner 65534) is running after job r1".to_owned()
        )
    );
    assert_eq!(
        checks.notes(),
        [
            CheckNote::new(
                code::NEW_MOUNT,
                "/mnt/x (ext4) from /dev/sdb1, /mnt/y (ext4) from /dev/sdb2"
            ),
            CheckNote::new(code::USER_CHANGED, "added eve, added mallory"),
            CheckNote::new(code::GROUP_CHANGED, "added ops"),
            CheckNote::new(code::MODULE_CHANGED, "loaded evil, unloaded ext4"),
        ]
    );

    for n in 0..60 {
        host.mount(mount(&format!("/mnt/long-name-{n}"), "/dev/sdc1"));
        host.start_process(process(
            800 + n,
            100,
            JOB_UID,
            980 + u64::from(n),
            "0::/long",
        ));
    }
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.detail().len(), CHECK_DETAIL_MAX);
    assert!(matches!(found, CheckFinding::Residue { .. }));
    let notes = checks.notes();
    assert_eq!(codes(&notes), [code::NEW_MOUNT]);
    assert_eq!(notes[0].detail.len(), CHECK_DETAIL_MAX);
}

/// Asserts that a check that finds no leftover takes a new baseline, noted changes included:
/// a mount that went away without a note is a new mount, noted once, when it comes back, and
/// the check after that notes nothing. It catches a baseline that is never moved forward,
/// and a note repeated after every later check.
#[test]
fn the_baseline_takes_what_a_clean_check_notes() {
    let host = FakeHost::quiet();
    host.mount(mount("/mnt/data", "/dev/sdb1"));
    let mut checks = started(&host, vec![]);
    host.unmount("/mnt/data");
    assert!(quiet(&mut checks, "r1"));
    host.mount(mount("/mnt/data", "/dev/sdb1"));
    assert_eq!(
        codes(&noted_after(&mut checks, "r2")),
        [code::NEW_MOUNT],
        "the baseline no longer has it"
    );
    assert!(quiet(&mut checks, "r3"), "and the note moved it");
}

/// Asserts that a check that finds a leftover keeps the baseline's processes of the job users'
/// range, so the leftover is found again by every later check, and takes everything that only
/// the notes look at (mounts, accounts, modules, watched directories, and a process outside the
/// job users' range), so that each of those is noted once and not after every later check. It
/// catches a baseline moved over a leftover, which would clear it, and one left behind, which
/// would repeat every note for as long as the host is tainted.
#[test]
fn a_dirty_check_keeps_its_leftover_and_takes_its_notes() {
    let watched = PathBuf::from("/watch/a");
    let host = FakeHost::quiet();
    host.watch(&watched, Watched::Digest("one".to_owned()));
    let mut checks = started(&host, vec![watched.clone()]);
    host.start_process(process(700, 100, JOB_UID, 970, "0::/a"));
    host.start_process(process(300, 1, 0, 900, AGENT_GROUP));
    host.mount(mount("/mnt/x", "/dev/sdb1"));
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    host.add_group("ops:x:2000:");
    host.load_module("evil");
    host.watch(&watched, Watched::Digest("two".to_owned()));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert_eq!(
        codes(&checks.notes()),
        [
            code::DAEMONISED_CHILD,
            code::NEW_MOUNT,
            code::USER_CHANGED,
            code::GROUP_CHANGED,
            code::MODULE_CHANGED,
            code::WATCH_DIR_CHANGED
        ]
    );

    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER, "still running");
    assert!(found.detail().starts_with("pid 700"), "{found:?}");
    assert!(checks.notes().is_empty(), "each change is noted once");

    host.end_process(700);
    assert!(quiet(&mut checks, "r3"), "the leftover is gone: clean");
}

/// Asserts that the containers of the agent's other jobs are not counted against a job that
/// finished: a process in the control group of a live container, and a mount of one, are
/// neither a finding nor a note; they are not taken into the baseline either, so what is left
/// of them once the container is gone is found by the check of that job (the process) and
/// noted (the mount). It catches a job blamed for its neighbour, and a neighbour's leftovers
/// hidden by a baseline taken while it ran.
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
    assert!(quiet_beside(&mut checks, "r1", &["c2"]));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        found.code(),
        code::PROCESS_AS_JOB_USER,
        "once c2 is gone, what is left of it is found"
    );
    assert!(found.detail().starts_with("pid 600"), "{found:?}");
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/var/lib/docker/overlay2/c2/merged (overlay) from overlay"
        )]
    );

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
            quiet_beside(&mut checks, "r4", &["c2"]),
            "{root} {point} {source}"
        );
    }
}
