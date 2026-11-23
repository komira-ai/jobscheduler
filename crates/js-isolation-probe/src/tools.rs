//! The system tools a probe asks (`sudo`, `dscl`, `launchctl`, `defaults`, ...), behind
//! [`Tools`], so the probes are tested with a fake and run on a Mac with [`System`].
//!
//! A tool is named by its absolute path and never searched for; a tool that is not installed
//! answers [`Outcome::NotApplicable`].

use std::io::{self, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::outcome::{Line, Outcome};

/// What a tool did.
#[derive(Debug)]
pub struct Ran {
    /// Its exit code; `None` when it did not start, was ended by a signal or timed out.
    pub code: Option<i32>,
    /// What it printed on standard output.
    pub stdout: Vec<u8>,
    /// What it printed on standard error.
    pub stderr: Vec<u8>,
    /// Why it could not be started.
    pub start: Option<io::Error>,
    /// Whether it was killed because it did not end within the limit.
    pub timed_out: bool,
}

impl Ran {
    /// A tool that ended with `code` and printed `stdout` and `stderr`.
    #[must_use]
    pub fn exited(code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            start: None,
            timed_out: false,
        }
    }

    /// A tool that could not be started.
    #[must_use]
    pub fn unstarted(err: io::Error) -> Self {
        Self {
            code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            start: Some(err),
            timed_out: false,
        }
    }

    /// Whether it exited 0.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Standard output as text, trimmed.
    #[must_use]
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    /// Why it did not succeed, in a few words: how it ended and the first line of its
    /// standard error, cut to 100 characters.
    #[must_use]
    pub fn why(&self) -> String {
        if let Some(err) = &self.start {
            return format!("cannot start: {err}");
        }
        if self.timed_out {
            return "timed out".to_owned();
        }
        let ended = self
            .code
            .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}"));
        let text = String::from_utf8_lossy(&self.stderr);
        let first: String = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(100)
            .collect();
        if first.is_empty() {
            ended
        } else {
            format!("{ended}: {first}")
        }
    }

    /// The usual reading of a tool run as a probe: exit 0 is [`Outcome::Allowed`], another
    /// exit [`Outcome::Denied`] (with [`Ran::why`]), a tool that is not installed
    /// [`Outcome::NotApplicable`], one the system refused to start (`EPERM`, `EACCES`)
    /// [`Outcome::Denied`] with the error number, and one that could not start otherwise or
    /// timed out [`Outcome::Error`].
    #[must_use]
    pub fn line(&self, name: &str) -> Line {
        if let Some(err) = &self.start {
            if err.kind() == io::ErrorKind::NotFound {
                return Line::new(name, Outcome::NotApplicable).detail(self.why());
            }
            // The sandbox refuses to start a setuid tool (sudo, su, crontab, at) with EPERM.
            return Line::refused(name, err, self.why());
        }
        if self.ok() {
            return Line::new(name, Outcome::Allowed);
        }
        let outcome = if self.timed_out || self.code.is_none() {
            Outcome::Error
        } else {
            Outcome::Denied
        };
        Line::new(name, outcome).detail(self.why())
    }
}

/// Runs a system tool, and signals a process.
pub trait Tools {
    /// Runs `program` (an absolute path) with `args`, writing `stdin` to it.
    fn run(&self, program: &str, args: &[&str], stdin: &[u8]) -> Ran;

    /// Sends SIGKILL to the process `pid`.
    ///
    /// # Errors
    /// The system's refusal (`ESRCH`, `EPERM`).
    fn kill(&self, pid: i32) -> io::Result<()>;

    /// Starts this program's `linger --token <token> --secs <secs>` in the background, with
    /// no standard input or output, and does not wait for it. Returns its pid.
    ///
    /// # Errors
    /// Why it could not be started.
    fn linger(&self, token: &str, secs: u64) -> io::Result<i32>;
}

/// The real tools: each run gets an empty environment but for `HOME`, `USER`, `LOGNAME` and
/// `TMPDIR` (the job's own), its own process group, and at most `limit` to end, after which
/// the group is killed.
#[derive(Debug, Clone)]
pub struct System {
    /// How long a tool may run.
    pub limit: Duration,
    /// This program, which `linger` starts.
    pub exe: std::path::PathBuf,
}

/// The variables a tool keeps from the probe's environment: the account's own, which the
/// native runtime sets for every run and the tools read to find the user's files (`TMPDIR`
/// the job's temporary directory in its home). Nothing configures the probe through them.
const KEPT_ENV: [&str; 4] = ["HOME", "USER", "LOGNAME", "TMPDIR"];

impl Tools for System {
    fn run(&self, program: &str, args: &[&str], stdin: &[u8]) -> Ran {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(
                KEPT_ENV
                    .iter()
                    .filter_map(|k| std::env::var_os(k).map(|v| (k, v))),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => return Ran::unstarted(err),
        };
        let group = rustix::process::Pid::from_child(&child);
        let input = stdin.to_vec();
        let feed = child.stdin.take().map(|mut pipe| {
            thread::spawn(move || {
                // A tool that exits without reading closes the pipe: nothing to report.
                let _ = pipe.write_all(&input);
            })
        });
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(child.wait());
        });
        let (code, timed_out) = match rx.recv_timeout(self.limit) {
            Ok(status) => (status.ok().and_then(|s| s.code()), false),
            Err(_) => {
                // The group is the tool and what it started; one gone already is fine.
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                let _ = rx.recv();
                (None, true)
            }
        };
        let _ = feed.map(thread::JoinHandle::join);
        Ran {
            code,
            stdout: collect(out),
            stderr: collect(err),
            start: None,
            timed_out,
        }
    }

    fn kill(&self, pid: i32) -> io::Result<()> {
        let pid = rustix::process::Pid::from_raw(pid)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))?;
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).map_err(io::Error::from)
    }

    fn linger(&self, token: &str, secs: u64) -> io::Result<i32> {
        let mut child = Command::new(&self.exe)
            .args(["linger", "--token", token, "--secs", &secs.to_string()])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // A pid fits an i32: the kernel's pid_t.
        let pid = child.id().cast_signed();
        // Reaped when it ends, while this process still runs; never waited for.
        thread::spawn(move || child.wait());
        Ok(pid)
    }
}

/// Reads a pipe to its end on a thread of its own.
fn drain(mut pipe: impl io::Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn collect(reader: Option<thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader.and_then(|r| r.join().ok()).unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod fake {
    //! A [`Tools`] that answers from a table and records what it was asked.

    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io;

    use super::{Ran, Tools};

    /// Answers by the command line (`program arg arg`); a line it does not know exits 1 with
    /// `unknown` on standard error.
    #[derive(Default)]
    pub struct Fake {
        /// The answers, by command line.
        pub answers: RefCell<BTreeMap<String, Vec<Ran>>>,
        /// Every command line asked, with its standard input.
        pub asked: RefCell<Vec<(String, String)>>,
        /// Every pid killed.
        pub killed: RefCell<Vec<i32>>,
        /// Whether a kill fails with EPERM.
        pub kill_refused: bool,
        /// Every `(token, secs)` lingered; a linger answers pid 4242, or EPERM when
        /// `linger_refused`.
        pub lingered: RefCell<Vec<(String, u64)>>,
        /// Whether a linger fails with EPERM.
        pub linger_refused: bool,
    }

    impl Fake {
        /// Answers `line` with `ran`; answers given for one line are used in order, the last
        /// one again and again.
        pub fn on(self, line: &str, ran: Ran) -> Self {
            self.answers
                .borrow_mut()
                .entry(line.to_owned())
                .or_default()
                .push(ran);
            self
        }

        /// Answers `line` with exit `code` and the given output.
        pub fn exit(self, line: &str, code: i32, stdout: &str, stderr: &str) -> Self {
            self.on(line, Ran::exited(code, stdout, stderr))
        }

        /// The command lines asked, in order.
        pub fn lines(&self) -> Vec<String> {
            self.asked.borrow().iter().map(|(l, _)| l.clone()).collect()
        }
    }

    fn copy(ran: &Ran) -> Ran {
        Ran {
            code: ran.code,
            stdout: ran.stdout.clone(),
            stderr: ran.stderr.clone(),
            start: ran
                .start
                .as_ref()
                .map(|e| io::Error::new(e.kind(), e.to_string())),
            timed_out: ran.timed_out,
        }
    }

    impl Tools for Fake {
        fn run(&self, program: &str, args: &[&str], stdin: &[u8]) -> Ran {
            let line = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.asked
                .borrow_mut()
                .push((line.clone(), String::from_utf8_lossy(stdin).into_owned()));
            let mut answers = self.answers.borrow_mut();
            match answers.get_mut(&line) {
                Some(list) if list.len() > 1 => list.remove(0),
                Some(list) => copy(&list[0]),
                None => Ran::exited(1, "", "unknown"),
            }
        }

        fn kill(&self, pid: i32) -> io::Result<()> {
            self.killed.borrow_mut().push(pid);
            if self.kill_refused {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            } else {
                Ok(())
            }
        }

        fn linger(&self, token: &str, secs: u64) -> io::Result<i32> {
            self.lingered.borrow_mut().push((token.to_owned(), secs));
            if self.linger_refused {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            } else {
                Ok(4242)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_zero_is_allowed_and_another_exit_denied_with_why() {
        assert_eq!(
            Ran::exited(0, "out\n", "").line("priv:sudo"),
            Line::new("priv:sudo", Outcome::Allowed)
        );
        assert_eq!(
            Ran::exited(1, "", "\n  sudo: a password is required\nmore\n")
                .line("priv:sudo")
                .to_string(),
            "priv:sudo: denied; exit 1: sudo: a password is required"
        );
        assert_eq!(
            Ran::exited(2, "", "").line("x:y").to_string(),
            "x:y: denied; exit 2"
        );
    }

    #[test]
    fn a_missing_tool_is_not_applicable_a_refused_start_denied_and_others_errors() {
        let missing = Ran::unstarted(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(missing.line("priv:at").outcome, Outcome::NotApplicable);
        assert!(missing.why().starts_with("cannot start: "));
        // What a Mac's sandbox answers when a job starts a setuid tool.
        let refused = Ran::unstarted(io::Error::from_raw_os_error(libc::EPERM));
        let line = refused.line("priv:sudo");
        assert_eq!(line.outcome, Outcome::Denied);
        assert_eq!(line.errno, Some(libc::EPERM));
        assert!(line.detail.starts_with("cannot start: "), "{line}");
        let odd = Ran::unstarted(io::Error::from_raw_os_error(libc::ENOEXEC));
        assert_eq!(odd.line("priv:at").outcome, Outcome::Error);
    }

    #[test]
    fn a_timeout_or_a_signal_is_an_error() {
        let mut ran = Ran::exited(0, "", "");
        ran.code = None;
        ran.timed_out = true;
        assert_eq!(ran.line("x").to_string(), "x: error; timed out");
        ran.timed_out = false;
        assert_eq!(ran.line("x").to_string(), "x: error; killed by a signal");
    }

    #[test]
    fn the_why_is_cut_to_a_hundred_characters() {
        let long = "e".repeat(300);
        assert_eq!(
            Ran::exited(3, "", &long).why(),
            format!("exit 3: {}", "e".repeat(100))
        );
        assert_eq!(Ran::exited(0, " a b \n", "").out(), "a b");
    }

    #[test]
    fn the_fake_answers_in_order_then_repeats_and_records() {
        let tools = fake::Fake::default()
            .exit("/bin/x a", 0, "one", "")
            .exit("/bin/x a", 1, "two", "")
            .on(
                "/bin/y",
                Ran::unstarted(io::Error::from(io::ErrorKind::NotFound)),
            );
        assert_eq!(tools.run("/bin/x", &["a"], b"in").out(), "one");
        assert_eq!(tools.run("/bin/x", &["a"], b"").out(), "two");
        assert_eq!(tools.run("/bin/x", &["a"], b"").out(), "two");
        assert!(tools.run("/bin/y", &[], b"").start.is_some());
        assert!(tools.run("/bin/y", &[], b"").start.is_some());
        assert_eq!(tools.run("/bin/z", &[], b"").why(), "exit 1: unknown");
        assert_eq!(
            tools.asked.borrow()[0],
            ("/bin/x a".to_owned(), "in".to_owned())
        );
        assert_eq!(tools.kill(7).ok(), Some(()));
        let refusing = fake::Fake {
            kill_refused: true,
            ..fake::Fake::default()
        };
        assert!(refusing.kill(8).is_err());
        assert_eq!(*refusing.killed.borrow(), [8]);
        assert_eq!(tools.linger("t", 5).ok(), Some(4242));
        let refusing = fake::Fake {
            linger_refused: true,
            ..fake::Fake::default()
        };
        assert!(refusing.linger("t", 5).is_err());
        assert_eq!(*refusing.lingered.borrow(), [("t".to_owned(), 5)]);
    }
}
