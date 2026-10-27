//! The `shm-owner` step and its tracker: the names it makes, and what removes them.

use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::time::Duration;

use super::{act, eventually, output, text};

/// A fresh name prefix, short enough for macOS's 31-byte names.
fn shm_prefix(tag: &str) -> String {
    format!("kt{}{tag}", std::process::id() % 100_000)
}

/// Starts `shm-owner` for `prefix`, in a process group of its own, and waits until it
/// says it is ready; returns it and its tracker's pid.
fn start_owner(prefix: &str) -> (std::process::Child, i32) {
    use std::io::BufRead as _;
    let mut owner = act(&["shm-owner", prefix, "2"])
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("owner");
    let mut line = String::new();
    std::io::BufReader::new(owner.stdout.as_mut().expect("stdout"))
        .read_line(&mut line)
        .expect("ready");
    let tracker = line
        .trim()
        .strip_prefix("shm-owner ready ")
        .unwrap_or_else(|| panic!("the owner said {line:?}"))
        .parse()
        .expect("tracker pid");
    (owner, tracker)
}

fn signal(pid: i32, signal: i32) {
    // SAFETY: kill(2) takes plain integers.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "kill {pid}");
}

/// Catches: an owner that makes fewer names than asked, a tracker that is not running
/// when the owner says ready, and names that are not shared memory and semaphores
/// both.
#[test]
fn the_owner_makes_every_name_and_holds_them() {
    let prefix = shm_prefix("o");
    let (mut owner, tracker) = start_owner(&prefix);
    let found = js_testkit::shm::present(&prefix, 2);
    signal(tracker, libc::SIGKILL);
    let pid = i32::try_from(owner.id()).expect("pid");
    signal(pid, libc::SIGKILL);
    owner.wait().expect("reaped");
    js_testkit::shm::unlink(&prefix, 2);
    assert_eq!(found.len(), 4, "{found:?}");
}

/// Catches: a tracker that does not unlink on SIGTERM (the handler missing or not
/// waking the poll), one that unlinks and also takes the owner down, and an owner that
/// unlinks by itself (the owner is still alive after, and the names gone).
#[test]
fn the_tracker_unlinks_every_name_on_sigterm() {
    let prefix = shm_prefix("t");
    let (mut owner, tracker) = start_owner(&prefix);
    signal(tracker, libc::SIGTERM);
    eventually("the names are gone after SIGTERM", || {
        js_testkit::shm::present(&prefix, 2).is_empty()
    });
    assert!(
        owner.try_wait().expect("owner").is_none(),
        "the owner ended"
    );
    signal(i32::try_from(owner.id()).expect("pid"), libc::SIGKILL);
    owner.wait().expect("reaped");
    js_testkit::shm::unlink(&prefix, 2);
}

/// Catches: a tracker that ignores its pipe closing (the owner killed with nothing to
/// catch: the names would stay), which is how Python's tracker learns the workload is
/// gone.
#[test]
fn the_tracker_unlinks_every_name_when_the_owner_goes() {
    let prefix = shm_prefix("p");
    let (mut owner, _) = start_owner(&prefix);
    signal(i32::try_from(owner.id()).expect("pid"), libc::SIGKILL);
    owner.wait().expect("reaped");
    eventually("the names are gone after the owner died", || {
        js_testkit::shm::present(&prefix, 2).is_empty()
    });
    js_testkit::shm::unlink(&prefix, 2);
}

/// Catches: a test double that cleans up on SIGKILL (it would show no leak when the
/// whole group is killed at once, and the lease tests that need a leak to see would
/// pass for nothing).
#[test]
fn a_sigkill_of_the_whole_group_leaves_every_name() {
    let prefix = shm_prefix("k");
    let (mut owner, _) = start_owner(&prefix);
    let group = i32::try_from(owner.id()).expect("pid");
    signal(-group, libc::SIGKILL);
    owner.wait().expect("reaped");
    std::thread::sleep(Duration::from_millis(300));
    let left = js_testkit::shm::present(&prefix, 2);
    js_testkit::shm::unlink(&prefix, 2);
    assert_eq!(left.len(), 4, "{left:?}");
}

/// Catches: an owner that does not end when its hold is over, and one that ends without
/// its tracker seeing the pipe close: the way Python's tracker learns that the workload
/// ended on its own.
#[test]
fn an_owner_that_ends_normally_lets_its_tracker_unlink_the_names() {
    let prefix = shm_prefix("n");
    let out = output(&["shm-owner", &prefix, "2", "0.3"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with("shm-owner ready "),
        "{}",
        text(&out.stdout)
    );
    eventually("the names are gone after the owner ended", || {
        js_testkit::shm::present(&prefix, 2).is_empty()
    });
    js_testkit::shm::unlink(&prefix, 2);
}
