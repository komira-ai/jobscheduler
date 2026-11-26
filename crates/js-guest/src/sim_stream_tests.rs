//! A streamed command that writes some output and then waits: the turn that hands the sink
//! fewer reads than a batch goes round again without asking the clock, and the command's
//! end is still seen; with a batch of one, the turn that fills its batch asks the clock at
//! once. The command blocks on a FIFO that the test's clock opens only after the
//! sink has its output, so the order of events is fixed and nothing races a timer.

use std::cell::Cell;
use std::ffi::CString;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use crate::sim::Ended;
use crate::sim_run::{Clock, ProcessRunner, stream_with};
use crate::test_dir::TestDir;

/// A clock that never moves; its first sleep after the sink has output releases the command
/// by writing a line to `fifo`. The release is written without a branch on whether the sink
/// has output yet, or on whether the command was released, so which sleeps come before or
/// after the output (a matter of scheduling) changes no branch the test covers.
struct Release {
    seen: Rc<Cell<bool>>,
    released: Cell<bool>,
    fifo: PathBuf,
}

impl Clock for Release {
    fn now_ms(&self) -> u64 {
        0
    }
    fn sleep(&self, _: Duration) {
        let go = self.seen.get() & !self.released.get();
        Some(&self.fifo)
            .filter(|_| go)
            .into_iter()
            .for_each(|path| {
                let mut fifo = std::fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .expect("open the FIFO");
                fifo.write_all(b"go\n").expect("write the FIFO");
                self.released.set(true);
            });
        std::thread::yield_now();
    }
}

/// A FIFO in a new directory, a clock that releases a command waiting on it, and the command:
/// it prints one byte and then waits for a line on the FIFO.
fn fifo_run(name: &str) -> (TestDir, Rc<Cell<bool>>, Release, Vec<String>) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let fifo = dir.join("go");
    let path = CString::new(fifo.as_os_str().as_bytes()).expect("path");
    // SAFETY: `path` is a valid NUL-terminated string that lives across the call.
    let rc = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
    let seen = Rc::new(Cell::new(false));
    let clock = Release {
        seen: Rc::clone(&seen),
        released: Cell::new(false),
        fifo: fifo.clone(),
    };
    let argv: Vec<String> = [
        "/bin/sh",
        "-c",
        r#"printf x; read -r line < "$0""#,
        fifo.to_str().expect("utf-8 path"),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    (dir, seen, clock, argv)
}

/// Behaviour: a command that prints one byte and then waits for a line on a FIFO is still
/// running when the sink takes its byte, a turn that took fewer reads than a batch; the
/// stream goes on and sees it exit 0 once the clock, at its next sleep, writes the line. The
/// sink gets the byte once and the command is released once. Catches a stream that stops or
/// ends the command after a partial read.
#[test]
fn a_command_that_writes_then_waits_is_still_followed_to_its_end() {
    let (_dir, seen, clock, argv) = fifo_run("kbf-guest-stream-partial");
    let runner = ProcessRunner::new(clock, Vec::new());
    let mut out = Vec::new();
    let streamed = runner.stream(&argv, Duration::from_secs(3_600), &mut |chunk| {
        out.extend_from_slice(chunk);
        seen.set(true);
    });
    assert_eq!(streamed.ended, Ended::Exited(0));
    assert_eq!(out, b"x");
}

/// Behaviour: with a batch of one read per turn, the turn that takes the command's byte has a
/// full batch, so it asks the clock for a sleep (which releases the command) instead of going
/// round again; the stream then sees the command exit 0 and the sink has the byte once. The
/// command cannot end before the byte is taken and the clock moves only at a sleep, so the
/// full batch happens on every run, with no producer to outrun the sink. Catches a stream
/// that, after a full batch, ends or kills the command or loses its output. (That a full
/// batch lets a deadline pass while output still flows is asserted by the `yes` test in
/// `sim_tests.rs`.)
#[test]
fn a_full_batch_lets_the_clock_move_and_the_command_is_followed_to_its_end() {
    let (_dir, seen, clock, argv) = fifo_run("kbf-guest-stream-full");
    let mut out = Vec::new();
    let streamed = stream_with(
        &clock,
        &[],
        &argv,
        Duration::from_secs(3_600),
        1,
        &mut |chunk| {
            out.extend_from_slice(chunk);
            seen.set(true);
        },
    );
    assert_eq!(streamed.ended, Ended::Exited(0));
    assert_eq!(out, b"x");
    assert!(clock.released.get());
}
