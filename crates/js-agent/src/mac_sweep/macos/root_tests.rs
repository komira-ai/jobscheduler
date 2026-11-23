//! The sweep's privileged calls on macOS, executed for real as root and read back from the
//! system: the listing of every user's processes with their real and effective uid, the
//! reading of their audit sessions (`auditon`, root only) and `SIGKILL` to processes of other
//! users. No fake stands in for any of them.
//!
//! Every test needs root, so each is `#[ignore]` with that reason: the ordinary test run lists
//! them as ignored, by name, and runs none. beta.yml's macOS job runs exactly them as root
//! (`sudo ... cargo test -p js-agent --lib -- --ignored mac_sweep::macos::root_tests`). A run
//! without root fails at its first line, saying so: it never passes without having run.
//!
//! The slot's uid and the other user's are uids no account of the runner has, so the sweep,
//! which ends every process of the slot's uid, can only reach the processes the test starts.

use std::io;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, Command, Stdio};

use super::super::{Found, ProcessTable, job_ids, sweep};
use super::MacTable;
use crate::mac_supervisor::macos::audit::{asid_of, new_audit_session};

/// The slot's uid: no account of the runner has it.
const SLOT: u32 = 64_123;
/// Another user's uid, as unused.
const OTHER: u32 = 64_124;
/// The group the children run in (`staff`).
const STAFF: u32 = 20;

/// Fails the test unless it runs as root: a privileged test never passes without running.
fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "this test needs root on macOS: beta.yml runs it with sudo"
    );
}

/// A `sleep` that outlives the test unless something ends it, made by `command`.
fn sleeper(command: &mut Command) -> Child {
    command
        .arg("600")
        .stdin(Stdio::null())
        .spawn()
        .expect("sleep starts")
}

/// A `sleep` run as `uid`, in this process's audit session.
fn as_user(uid: u32) -> Child {
    sleeper(Command::new("/bin/sleep").uid(uid).gid(STAFF))
}

/// A `sleep` in a new audit session of its own, opened as root before it drops to `uid`: what
/// the supervisor does for a job, so it is the session rule alone that finds it.
fn in_own_session(uid: u32) -> Child {
    let mut command = Command::new("/bin/sleep");
    // SAFETY: the closure runs in the child between fork and exec and makes only system calls
    // (getaudit_addr, setaudit_addr, setgid, setuid); it allocates nothing, since an error is
    // the OS error's number.
    unsafe {
        command.pre_exec(move || {
            new_audit_session(uid)?;
            if libc::setgid(STAFF) != 0 || libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    sleeper(&mut command)
}

/// A `sleep` whose real uid is `uid` and whose effective uid stays root: what a set-uid
/// program the job ran looks like.
fn set_uid_root_of(uid: u32) -> Child {
    let mut command = Command::new("/bin/sleep");
    // SAFETY: as in `in_own_session`: one system call, no allocation.
    unsafe {
        command.pre_exec(move || {
            if libc::setreuid(uid, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    sleeper(&mut command)
}

/// `child`'s pid as a sweep names it.
fn pid(child: &Child) -> i32 {
    i32::try_from(child.id()).expect("a pid fits in i32")
}

/// As root, over the real process table: a process of the slot's uid, a set-uid process whose
/// real uid is the slot's, and a process of another user in the job's own audit session are
/// ended, each found by its rule; a process of the other user outside that session, and the
/// agent's own session, are untouched. Read back: no process of the slot's uid and none of the
/// job's session is left, and each ended child's status is the `SIGKILL`.
///
/// Catches: a listing that cannot see other users' processes or reads only the effective uid,
/// a session that is not read as root, a kill that does not reach another user's process, and
/// a sweep that reaches a process of another uid outside the session.
#[test]
#[ignore = "needs root on macOS: beta.yml runs it with sudo"]
fn as_root_the_slots_and_the_sessions_processes_end_and_another_users_does_not() {
    require_root();
    let mut slot = as_user(SLOT);
    let mut set_uid = set_uid_root_of(SLOT);
    let mut session = in_own_session(OTHER);
    let mut bystander = as_user(OTHER);
    let own_pid = rustix::process::getpid().as_raw_nonzero().get();
    let own_asid = asid_of(own_pid);
    assert!(own_asid.is_some(), "root reads its own audit session");
    let job_asid = asid_of(pid(&session));
    assert!(
        job_asid.is_some(),
        "root reads another user's audit session"
    );
    assert_ne!(job_asid, own_asid, "the child opened a session of its own");

    let listed = MacTable.list(true).expect("the process table reads");
    let seen = |child: &Child| listed.iter().find(|p| p.pid == pid(child)).cloned();
    let set_uid_seen = seen(&set_uid).expect("the set-uid process is listed");
    assert_eq!((set_uid_seen.uid, set_uid_seen.ruid), (0, SLOT));
    assert_eq!(seen(&session).expect("listed").asid, job_asid);

    let ids = job_ids(SLOT, job_asid, own_pid, own_asid);
    assert_eq!(ids.asid, job_asid);
    let tally = sweep(&MacTable, &ids);
    assert!(tally.clean, "{tally:?}");
    assert!(tally.rounds >= 2, "{tally:?}");
    let by = |found: Found| -> Vec<i32> {
        let list = match found {
            Found::Session => &tally.by_session,
            Found::UserOnly => &tally.by_user_only,
        };
        let mut pids: Vec<i32> = list.iter().map(|swept| swept.pid).collect();
        pids.sort_unstable();
        pids
    };
    assert_eq!(by(Found::Session), [pid(&session)]);
    let mut user_only = vec![pid(&slot), pid(&set_uid)];
    user_only.sort_unstable();
    assert_eq!(by(Found::UserOnly), user_only);

    for child in [&mut slot, &mut set_uid, &mut session] {
        let status = child.wait().expect("reaped");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    }
    assert!(
        bystander.try_wait().expect("asked").is_none(),
        "the other user's process outside the session was ended"
    );
    let after = MacTable.list(true).expect("the process table reads");
    assert!(
        !after
            .iter()
            .any(|p| p.uid == SLOT || p.ruid == SLOT || p.asid == job_asid),
        "left: {after:?}"
    );
    assert!(after.iter().any(|p| p.pid == pid(&bystander)));
    let _ = bystander.kill();
    let _ = bystander.wait();
}

/// As root, the sweep of a job whose session is the agent's own (a supervisor that opened
/// none) drops the session and ends only the slot's uid: the agent's session-mates, root's
/// processes among them, are untouched.
///
/// Catches: the agent's whole session taken as the job's, with root's own processes in it.
#[test]
#[ignore = "needs root on macOS: beta.yml runs it with sudo"]
fn as_root_the_agents_own_session_is_never_swept() {
    require_root();
    let mut slot = as_user(SLOT);
    let mut mate = sleeper(&mut Command::new("/bin/sleep"));
    let own_pid = rustix::process::getpid().as_raw_nonzero().get();
    let own_asid = asid_of(own_pid);
    assert_eq!(
        asid_of(pid(&mate)),
        own_asid,
        "the mate shares the agent's session"
    );
    let ids = job_ids(SLOT, own_asid, own_pid, own_asid);
    assert_eq!(ids.asid, None);
    let tally = sweep(&MacTable, &ids);
    assert!(tally.clean, "{tally:?}");
    assert_eq!(tally.asid, None);
    assert!(tally.by_session.is_empty(), "{tally:?}");
    let ended: Vec<i32> = tally.by_user_only.iter().map(|swept| swept.pid).collect();
    assert_eq!(ended, [pid(&slot)]);
    assert_eq!(slot.wait().expect("reaped").signal(), Some(libc::SIGKILL));
    assert!(
        mate.try_wait().expect("asked").is_none(),
        "root's process was ended"
    );
    let _ = mate.kill();
    let _ = mate.wait();
}
