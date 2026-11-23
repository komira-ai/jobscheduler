//! The real table, listing and one kill only. No test here runs `sweep` over the real table:
//! the test user's uid would be the slot's, and the uid rule would end the test runner's own
//! processes (`sweep` refuses the sweeper's own uid, which is a second guard).

use std::process::{Child, Command, Stdio};

use super::super::{Proc, ProcessTable};
use super::MacTable;

/// The listed process `pid`, if any.
fn listed(pid: u32) -> Option<Proc> {
    let pid = i32::try_from(pid).expect("a pid fits in i32");
    MacTable
        .list(true)
        .expect("the process table reads")
        .into_iter()
        .find(|p| p.pid == pid)
}

/// A `sleep` that outlives the test unless it is killed.
fn sleeper() -> Child {
    Command::new("/bin/sleep")
        .arg("600")
        .stdin(Stdio::null())
        .spawn()
        .expect("sleep starts")
}

/// Waits until `child` has exited, leaving it a zombie (not reaped).
fn exited_not_reaped(child: &Child) {
    let id: libc::id_t = child.id();
    // SAFETY: an all-zero `siginfo_t` is a valid value of the plain C struct.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is ours and writable for the call; `WNOWAIT` leaves the child to be
    // reaped by its `wait` below.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    assert_eq!(rc, 0, "waitid: {}", std::io::Error::last_os_error());
}

/// The test process is listed with its own effective and real uid; its audit session is read
/// when the test runs as root (as the agent does) and is `None` otherwise (the kernel answers
/// `auditon` to root alone); without `with_asid` no session is read, and pids 0 and 1 are
/// never listed. Catches a listing that reads the wrong struct fields, leaves out a live
/// process, or reads a session it was not asked for.
#[test]
fn the_test_process_is_listed_with_its_uids_and_its_audit_session() {
    let me = listed(std::process::id()).expect("the test process is listed");
    let uid = rustix::process::getuid().as_raw();
    let euid = rustix::process::geteuid().as_raw();
    assert_eq!((me.uid, me.ruid), (euid, uid));
    assert_eq!(me.asid.is_some(), euid == 0, "{me:?}");
    let pid = i32::try_from(std::process::id()).expect("a pid fits in i32");
    let plain = MacTable.list(false).expect("the process table reads");
    let me = plain.iter().find(|p| p.pid == pid).expect("listed");
    assert_eq!(me.asid, None);
    assert!(plain.iter().all(|p| p.pid > 1), "pids 0 and 1 are left out");
}

/// A spawned child is listed by its name; killed and exited but not reaped (a zombie) it is
/// not; reaped it is not, and its status is the `SIGKILL`. Catches zombies counted as live
/// (a sweep that never ends clean while a parent has not reaped) and a kill that does not
/// end the process.
#[test]
fn a_spawned_child_is_listed_and_once_killed_and_exited_is_not() {
    use std::os::unix::process::ExitStatusExt as _;
    let mut child = sleeper();
    let seen = listed(child.id()).expect("the child is listed");
    assert_eq!(seen.name, "sleep");
    MacTable.kill(seen.pid);
    exited_not_reaped(&child);
    assert_eq!(listed(child.id()), None, "a zombie is left out");
    let status = child.wait().expect("the child is reaped");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_eq!(listed(child.id()), None);
}

/// A kill of a pid no process has (above the kernel's highest), or of a pid below 2, does
/// nothing and does not fail. Catches a kill that panics on `ESRCH` or signals a group.
#[test]
fn a_kill_of_a_missing_or_reserved_pid_is_nothing() {
    for pid in [i32::MAX, 1, 0, -1] {
        MacTable.kill(pid);
    }
    // The test process is still here to say so.
    assert!(listed(std::process::id()).is_some());
}
