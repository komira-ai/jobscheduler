//! How the privileged harness's `user_command` starts its program: as the test process's
//! own user, without forking the test process. A fork copies a process whose other tests
//! are running on other threads, and on macOS that copy aborted inside `fork` when another
//! thread was part way through a one-time initialisation, so the harness check that runs
//! `/bin/pwd` this way failed now and then with the child killed and nothing written.
//!
//! The test registers a fork handler that ends the copy with exit code 86 before it can
//! exec. `posix_spawn` runs no fork handlers, so a program the harness spawns runs; a
//! program it forks for ends with 86. The handler lasts for the whole process, so this
//! binary holds this one test only.

#[path = "support/priv_harness.rs"]
mod priv_harness;

use std::path::Path;
use std::process::Output;

use priv_harness::user_command;

/// The exit code of a forked copy of this process.
const FORKED: i32 = 86;

/// Runs in the child of every `fork` of this process, before the child can exec.
extern "C" fn end_a_forked_copy() {
    // SAFETY: `_exit` is async-signal-safe, so the child of a multithreaded fork may call it;
    // it ends the child without running anything the parent's threads were part way through.
    unsafe { libc::_exit(FORKED) }
}

fn ran(out: &Output) -> String {
    format!(
        "{}; stdout {:?}; stderr {:?}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Catches: `user_command` setting the user and group even when they are the test
/// process's own, which makes the standard library fork the test process to start
/// the program.
#[test]
fn a_command_as_the_callers_own_user_is_spawned_and_one_for_another_user_is_forked() {
    // SAFETY: `pthread_atfork` copies the function pointer; the handler is a plain
    // `extern "C"` function that lives as long as the process.
    let registered = unsafe { libc::pthread_atfork(None, None, Some(end_a_forked_copy)) };
    assert_eq!(registered, 0, "pthread_atfork");
    let me = rustix::process::geteuid().as_raw();
    let group = rustix::process::getegid().as_raw();

    let own = user_command(
        "/bin/echo",
        &[Path::new("-n"), Path::new("spawned")],
        me,
        group,
    )
    .output()
    .unwrap();
    assert!(own.status.success(), "as uid {me}: {}", ran(&own));
    assert_eq!(own.stdout, b"spawned", "{}", ran(&own));

    // Another user: the user can only be changed in a fork, so the handler ends the copy
    // before `/bin/echo` starts. This also shows the handler runs, so the case above
    // passes because nothing forked, not because fork handlers never run here.
    let other = me.wrapping_add(1);
    let theirs = user_command(
        "/bin/echo",
        &[Path::new("-n"), Path::new("forked")],
        other,
        group,
    )
    .output()
    .unwrap();
    assert_eq!(
        theirs.status.code(),
        Some(FORKED),
        "as uid {other}: {}",
        ran(&theirs)
    );
    assert!(theirs.stdout.is_empty(), "{}", ran(&theirs));

    // The group alone differs: still a fork.
    let other_group = group.wrapping_add(1);
    let regrouped = user_command("/bin/echo", &[Path::new("x")], me, other_group)
        .output()
        .unwrap();
    assert_eq!(
        regrouped.status.code(),
        Some(FORKED),
        "as gid {other_group}: {}",
        ran(&regrouped)
    );
}
