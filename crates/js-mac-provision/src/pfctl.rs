//! `pfctl`, behind a seam: every call the crate makes goes through [`Pfctl`], so the
//! logic of install, check and remove is tested against a fake on every platform.

use std::io;
use std::path::PathBuf;
use std::process::Command;

/// Where macOS keeps `pfctl`.
pub const SYSTEM_PFCTL: &str = "/sbin/pfctl";

/// What a `pfctl` run printed and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

impl Output {
    /// Whether it exited with 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Standard output, then standard error: `pfctl -E` prints its token on either,
    /// depending on the release.
    #[must_use]
    pub fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// One `pfctl` run.
pub trait Pfctl {
    /// Runs `pfctl` with `args`.
    ///
    /// # Errors
    /// The program could not be started. A non-zero exit is an [`Output`], not an error.
    fn run(&self, args: &[&str]) -> io::Result<Output>;
}

/// The real `pfctl`: a program started with the arguments and no shell.
#[derive(Clone, Debug)]
pub struct SystemPfctl {
    program: PathBuf,
}

impl SystemPfctl {
    /// A runner of the program at `program`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Pfctl for SystemPfctl {
    fn run(&self, args: &[&str]) -> io::Result<Output> {
        let out = Command::new(&self.program).args(args).output()?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_that_is_not_there_is_an_error() {
        let pf = SystemPfctl::new("/nonexistent/pfctl");
        let err = pf.run(&["-sr"]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_exit_code_and_both_streams_come_back() {
        // `ls` of a path that is there, then of one that is not.
        let pf = SystemPfctl::new("ls");
        let ok = pf.run(&["/"]).unwrap();
        assert!(ok.success());
        assert!(!ok.stdout.is_empty());
        let bad = pf.run(&["/nonexistent-js-mac-provision"]).unwrap();
        assert!(!bad.success());
        assert!(bad.code.is_some());
        assert!(!bad.stderr.is_empty());
        assert!(bad.combined().contains("nonexistent-js-mac-provision"));
    }
}
