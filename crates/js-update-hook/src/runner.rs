//! How a program is run: the one place a port starts one, so a test feeds recorded output
//! instead of touching the machine.

use std::time::Duration;

/// Runs a program and returns what it printed.
pub trait CommandRunner {
    /// Runs `program` with `args` and returns its standard output when it exits with
    /// status 0. The error says which program failed and how, never what it printed.
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;

    /// Like [`CommandRunner::run`], but the program's environment is exactly `env`: the
    /// environment of the caller is not passed on.
    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String>;

    /// Like [`CommandRunner::run_in`], but both of the program's output streams are read,
    /// each kept to its last `keep` bytes: a program that prints more is not an error, the
    /// older bytes are dropped and [`Streams::dropped`] says so. The call fails when the
    /// program has not exited within `limit`. A runner that cannot read the second stream
    /// reports that and does not guess.
    fn run_streams(
        &self,
        program: &str,
        _args: &[&str],
        _env: &[(&str, &str)],
        _keep: usize,
        _limit: Duration,
    ) -> Result<Streams, String> {
        Err(format!("{program}: this runner reads no standard error"))
    }
}

/// The last bytes of the two output streams of a program that exited with status 0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Streams {
    /// The end of standard output.
    pub stdout: Vec<u8>,
    /// The end of standard error.
    pub stderr: Vec<u8>,
    /// Whether either stream printed more than was kept.
    pub dropped: bool,
}
