//! Starting a program a test has just written.
//!
//! A test that copies a program (`fs::copy` of a built binary) and runs the copy can
//! find it busy: `exec` fails with `ETXTBSY` while any process holds a descriptor open
//! for writing on the file. The copy's own descriptor is closed when the copy returns,
//! but a process forked by *another thread of the test binary* while it was open (any
//! other test spawning a program, see [`crate::forked`]) holds a copy of it until that
//! child execs, a few milliseconds later. The file is whole and nothing is wrong with
//! it; the start has only to wait for the child to go.
//!
//! [`spawn_retrying`] starts the command again while it fails that way, a bounded
//! number of times, so a test can start a program it has just copied. When the start is made
//! by code the test cannot wrap (a library that spawns the program), [`install_copy`] and
//! [`install_script`] make the program without the test process ever holding it open.

use std::ffi::OsStr;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

/// How many times [`spawn_retrying`] tries to start the program.
pub const ATTEMPTS: u32 = 50;

/// How long [`spawn_retrying`] waits between tries: a second in all, far longer than a
/// fork takes to exec.
pub const PAUSE: Duration = Duration::from_millis(20);

/// `command.spawn()`, started again after [`PAUSE`] each time it fails because the
/// program is busy (`ETXTBSY`), at most [`ATTEMPTS`] times in all. Any other failure,
/// and the last busy one, is returned as it came.
///
/// # Errors
/// What `spawn` says.
pub fn spawn_retrying(command: &mut Command) -> io::Result<Child> {
    let mut started = None;
    let end = retry_busy(ATTEMPTS, PAUSE, &mut || {
        started = Some(command.spawn()?);
        Ok(())
    });
    end.map(|()| started.expect("a start that succeeded left its child"))
}

/// `start()`, called again after `pause` each time it fails because the program is busy
/// (`ETXTBSY`), at most `attempts` times in all (once, if `attempts` is less). Any other
/// failure, and the last busy one, is returned as it came.
///
/// # Errors
/// What `start` says.
pub fn retry_busy(
    attempts: u32,
    pause: Duration,
    start: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut left = attempts;
    loop {
        match start() {
            Err(error) if error.kind() == ErrorKind::ExecutableFileBusy && left > 1 => {
                left -= 1;
                std::thread::sleep(pause);
            }
            done => return done,
        }
    }
}

/// Makes `dest` a copy of the file `source` with mode `mode` (as `chmod` reads it: `755`,
/// `4755`), so that a program started from `dest` by code the test does not control (a
/// library that spawns it) is never busy.
///
/// Such a start cannot be retried, and closing the test's own descriptor is not enough: a
/// process another thread forks while the file is open keeps a copy until it execs (see the
/// module docs), and a rename does not change that, since the copy is of the open file, not
/// of its name. So the file is written by a process of its own, `cp` run by `sh`, that has
/// ended when this returns: the test process never has the file open, no child it forks can
/// hold it, and nothing has it open for writing afterwards.
///
/// # Panics
/// When `sh` cannot be run, or the copy fails.
pub fn install_copy(source: &Path, dest: &Path, mode: &str) {
    run_sh(
        r#"cp "$1" "$2" && chmod "$3" "$2""#,
        &[source.as_os_str(), dest.as_os_str(), OsStr::new(mode)],
    );
}

/// Makes `dest` a file holding `body` with mode `mode`, written the way [`install_copy`]
/// writes a copy.
///
/// # Panics
/// When `sh` cannot be run, or the write fails.
pub fn install_script(dest: &Path, body: &str, mode: &str) {
    run_sh(
        r#"printf '%s' "$1" > "$2" && chmod "$3" "$2""#,
        &[OsStr::new(body), dest.as_os_str(), OsStr::new(mode)],
    );
}

/// Runs `script` under `sh -c` with `args` as `$1`, `$2`, ...
fn run_sh(script: &str, args: &[&OsStr]) {
    let status = Command::new("sh")
        .args(["-c", script, "sh"])
        .args(args)
        .status()
        .expect("sh runs");
    assert!(status.success(), "installing a program: {status}");
}
