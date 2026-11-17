//! Where a running container is when its group below the lease is not there
//! ([`whereabouts_of`]): the order of the looks and what each answer means. The agent's
//! `/proc` is a function of the test; the group is a directory below the test's own.

use std::fs;

use js_testkit::dir::TestDir;

use super::whereabouts::whereabouts_of;
use crate::lifecycle::Whereabouts;

/// A `/proc` in which the process 77 runs in `/actions/other` (with a version 1 line before
/// the unified one), and that asserts it is asked for that process's control group alone.
fn in_other_group(path: &str) -> Option<String> {
    assert_eq!(path, "/proc/77/cgroup");
    Some("12:memory:/old\n0::/actions/other\n".to_owned())
}

/// A `/proc` that holds no process.
fn empty(_path: &str) -> Option<String> {
    None
}

/// A `/proc` that must not be read.
fn unread(path: &str) -> Option<String> {
    panic!("{path} was read")
}

/// Asserts that a container `crun` names no process for (not made yet, or ended and deleted
/// with its group) is `NoProcess` without a look at `/proc`, and that one whose process `crun`
/// named and is no longer in `/proc` (it ended after `crun state` answered) is `NoProcess`
/// too. Catches a host tainted as unchecked by a container whose group is missing only
/// because the container is not there: the slow start and the ended run of the farm.
#[test]
fn a_container_with_no_process_is_not_elsewhere() {
    let dir = TestDir::under(std::env::temp_dir(), "where-no-process");
    let ctr = dir.path().join("ctr");
    let unmade = whereabouts_of(&ctr, Err("no container js-x-r1".to_owned()), unread);
    assert_eq!(unmade, Whereabouts::NoProcess);
    assert_eq!(whereabouts_of(&ctr, Ok(77), empty), Whereabouts::NoProcess);
}

/// Asserts that a group made by the time `crun state` answered is the container's
/// (`Grouped`), without a look at `/proc`. Catches a group made between the lifecycle's look
/// and the runtime's being judged as blindness.
#[test]
fn a_group_there_once_crun_named_the_process_is_the_containers() {
    let dir = TestDir::under(std::env::temp_dir(), "where-grouped");
    let ctr = dir.path().join("ctr");
    fs::create_dir(&ctr).expect("the container's group");
    assert_eq!(
        whereabouts_of(&ctr, Ok(77), unread),
        Whereabouts::Grouped(ctr)
    );
}

/// Asserts that a process `crun` named, still in `/proc`, while the group below the lease is
/// not there, runs elsewhere, named by its unified line; and that a table with no unified
/// line is still elsewhere, named by what was read. Catches an escape from the lease's group
/// passing as a container that is merely not there.
#[test]
fn a_process_outside_its_group_is_elsewhere_and_named() {
    let dir = TestDir::under(std::env::temp_dir(), "where-elsewhere");
    let ctr = dir.path().join("ctr");
    assert_eq!(
        whereabouts_of(&ctr, Ok(77), in_other_group),
        Whereabouts::Elsewhere("/actions/other".to_owned())
    );
    let v1_only = whereabouts_of(&ctr, Ok(77), |_| Some("12:memory:/old\n".to_owned()));
    assert_eq!(
        v1_only,
        Whereabouts::Elsewhere(r#"no unified group (read "12:memory:/old")"#.to_owned())
    );
}
