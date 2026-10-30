//! Duty 3: the post-job checks list the `--watch-dir` directories (`SUGGESTED_WATCH_DIRS`
//! names `/root/.ssh`, `/etc/sudoers.d` and others that only root can read). A directory the
//! agent cannot read must make the agent refuse to start and, once running, taint the host
//! as unchecked: never a silent skip.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::cli_runtime::JobUsers;
use js_agent::integrity::{HostView, Integrity, ProcHost, Watched};
use js_agent::lifecycle::{CheckFinding, CheckHook, Finished};

use crate::harness::{
    AGENT, Kv, arg, cap, kv, make_dir, require_root, run_agent, verdict, work_dir,
};

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "watch_read" => {
            let dir = arg("DIR");
            let read = ProcHost::system().watched(&dir);
            let digest = match &read {
                Ok(Watched::Digest(digest)) => digest.clone(),
                _ => String::new(),
            };
            Some(vec![kv("watched", verdict(&read)), kv("digest", digest)])
        }
        "watch_real_dirs" => {
            let host = ProcHost::system();
            let read = host.watched(Path::new("/root"));
            Some(vec![kv("root_home", verdict(&read))])
        }
        "watch_start" => {
            let dir = arg("DIR");
            let started = Integrity::start(
                Box::new(ProcHost::system()),
                JobUsers::Uid(65534),
                vec![dir],
            );
            Some(vec![kv("start", verdict(&started.map(|_| ())))])
        }
        "watch_post_job" => {
            let watched = arg("DIR").join("watched");
            fs::create_dir(&watched).unwrap();
            fs::write(watched.join("a"), "x").unwrap();
            let started = Integrity::start(
                Box::new(ProcHost::system()),
                JobUsers::Uid(65534),
                vec![watched.clone()],
            );
            let mut checks = match started {
                Ok(checks) => checks,
                Err(why) => return Some(vec![kv("start", format!("err:{why}"))]),
            };
            // The job (or an operator) closes the directory after the baseline was taken.
            fs::set_permissions(&watched, fs::Permissions::from_mode(0o0)).unwrap();
            let found = checks.run(&Finished {
                run_id: "r1",
                live: &[],
                slot: None,
                live_slots: &[],
            });
            fs::set_permissions(&watched, fs::Permissions::from_mode(0o700)).unwrap();
            let (kind, code, detail) = match found {
                Ok(()) => ("none", "", String::new()),
                Err(CheckFinding::Unchecked { code, detail }) => ("unchecked", code, detail),
                Err(CheckFinding::Residue { code, detail }) => ("residue", code, detail),
            };
            Some(vec![
                kv("start", "ok"),
                kv("finding_kind", kind),
                kv("finding_code", code),
                kv("finding_detail", detail),
            ])
        }
        _ => None,
    }
}

/// A root-owned directory only root may enter, as `/root/.ssh` is on a stock host.
fn root_only(parent: &Path) -> std::path::PathBuf {
    let dir = parent.join("root-only");
    make_dir(&dir, 0o700, 0);
    fs::write(dir.join("authorized_keys"), "key").unwrap();
    dir
}

/// A: without `CAP_DAC_READ_SEARCH` the agent cannot list a directory only root may enter,
/// and the reader says so as an error (`Permission denied`), never an empty listing. It
/// catches a reader that turns a failed listing into "nothing there".
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_without_a_capability_gets_an_error_for_a_root_only_watch_dir() {
    let work = work_dir("priv-watch-read");
    let dir = root_only(work.path());
    let report = run_agent(&[], "watch_read", &[("DIR", &dir)]);
    report.expect_prefix("watched", "err:");
    assert!(
        report.get("watched").contains("Permission denied"),
        "{}",
        report.get("watched")
    );
}

/// A: `CAP_DAC_READ_SEARCH` alone lists a root-only directory and its entries' metadata, so
/// the watch of `/root/.ssh` and `/etc/sudoers.d` works for a non-root agent holding it
/// (and holding nothing else for this duty). `CAP_FOWNER` and `CAP_CHOWN` do not.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn cap_dac_read_search_alone_reads_a_root_only_watch_dir() {
    let work = work_dir("priv-watch-cap");
    let dir = root_only(work.path());
    let report = run_agent(&[cap::DAC_READ_SEARCH], "watch_read", &[("DIR", &dir)]);
    report.expect("watched", "ok");
    let report = run_agent(&[cap::FOWNER, cap::CHOWN], "watch_read", &[("DIR", &dir)]);
    report.expect_prefix("watched", "err:");
}

/// A: on this host the real `/root` (the parent of `/root/.ssh`) is closed to an agent
/// that is not root and holds nothing, and open to one with `CAP_DAC_READ_SEARCH`. It ties
/// the two results above to the path the design names.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn the_real_root_home_needs_cap_dac_read_search_for_a_non_root_agent() {
    let none = run_agent(&[], "watch_real_dirs", &[]);
    none.expect_prefix("root_home", "err:");
    let with = run_agent(&[cap::DAC_READ_SEARCH], "watch_real_dirs", &[]);
    with.expect("root_home", "ok");
}

/// A: an agent that cannot read a watched directory does not start: `Integrity::start`
/// takes the baseline, the unreadable directory is a `watch-unreadable` problem, and the
/// start fails naming it. The agent holds `CAP_SYS_PTRACE` (so the namespace check passes)
/// and nothing for the directory. It catches a baseline that skips what it cannot read.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn an_unreadable_watch_dir_stops_the_agent_from_taking_its_baseline() {
    let work = work_dir("priv-watch-start");
    let dir = root_only(work.path());
    let report = run_agent(&[cap::SYS_PTRACE], "watch_start", &[("DIR", &dir)]);
    report.expect_prefix(
        "start",
        "err:cannot take the baseline of the host: watch-unreadable",
    );
}

/// A: a directory that was readable at the baseline and is not after the job is a finding
/// of a check that did not run (`unchecked`, code `watch-unreadable`), not a pass: the
/// slot is not freed and the host is tainted. It runs against the real `/proc` and a real
/// directory closed with mode 0 after the baseline, as a user with `CAP_SYS_PTRACE` only.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_watch_dir_closed_after_the_baseline_taints_the_host_as_unchecked() {
    let work = work_dir("priv-watch-post");
    let own = work.path().join("agent-owned");
    make_dir(&own, 0o700, AGENT);
    let report = run_agent(&[cap::SYS_PTRACE], "watch_post_job", &[("DIR", &own)]);
    report.expect("start", "ok");
    report.expect("finding_kind", "unchecked");
    report.expect("finding_code", "watch-unreadable");
    assert!(
        report.get("finding_detail").contains("Permission denied"),
        "{}",
        report.get("finding_detail")
    );
}

/// B: a root agent reads a directory that belongs to another user and is closed to
/// everyone (mode 0) with no capability named, so no watch directory is out of its reach.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_root_agent_reads_a_closed_watch_dir_of_another_user() {
    require_root();
    let work = work_dir("priv-watch-root");
    let dir = work.path().join("closed");
    make_dir(&dir, 0, AGENT);
    let read = ProcHost::system().watched(&dir);
    assert!(matches!(read, Ok(Watched::Digest(_))), "{read:?}");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
}

/// A: a directory the agent may list but not search (mode `0744` for a user who is not the
/// owner: `r` without `x`) is NOT an error. Its entries cannot be examined, so they are
/// digested by name alone: rewriting a file in it does not change the digest, while an
/// agent with `CAP_DAC_READ_SEARCH` sees the change. This is the one way a watch directory
/// is weaker than unreadable-means-taint, and it is why shape A must give the agent search
/// permission on every watched directory (a capability, or `x` through an ACL), not read
/// alone. It pins the blind spot so the design's statement about it stays true.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_watch_dir_the_agent_may_list_but_not_search_is_digested_by_names_only() {
    let work = work_dir("priv-watch-listonly");
    let dir = work.path().join("list-only");
    make_dir(&dir, 0o744, 0);
    let file = dir.join("authorized_keys");
    fs::write(&file, "key").unwrap();

    let before = run_agent(&[], "watch_read", &[("DIR", &dir)]);
    before.expect("watched", "ok");
    fs::write(&file, "a much longer key than before").unwrap();
    let after = run_agent(&[], "watch_read", &[("DIR", &dir)]);
    assert_eq!(
        before.get("digest"),
        after.get("digest"),
        "a list-only directory is digested by name: a rewrite is not seen"
    );

    let seeing = run_agent(&[cap::DAC_READ_SEARCH], "watch_read", &[("DIR", &dir)]);
    fs::write(&file, "key").unwrap();
    let seeing_after = run_agent(&[cap::DAC_READ_SEARCH], "watch_read", &[("DIR", &dir)]);
    assert_ne!(seeing.get("digest"), seeing_after.get("digest"));
}
