//! `cargo xtask site-demo [--out <path>] [--bin-dir <dir>] [--port <n>]`: records the demonstration on
//! the landing page of jobscheduler.dev, `site/test/demo.transcript`. It builds the lease server and the
//! client, leases a host from a static host file on this machine, lists the lease and releases it, and
//! writes the transcript to `--out` (default `site/test/demo.transcript` under `--root`). It needs cargo
//! and a free port 7300 on 127.0.0.1. Lease ids and times differ on every run, so a new recording differs
//! from the committed one in those.
//!
//! The recording, in order: the host file (a `#` line saying what it is, then the file), the command that
//! writes the token file, the server start (a command and no output), then `js lease acquire`, `list`,
//! `release <id>` and `list` again, each with what it printed. The commands are the ones [`commands`]
//! builds and nothing else is typed in the transcript, so the test `committed_transcript_shows_the_commands_this_verb_runs`
//! compares them with `site/test/demo.transcript`, and a command changed here and not there fails it.
//!
//! The steps, each a piece with its own tests:
//! - [`build`] runs `cargo build --release -p js-lease-server -p js-cli` in the repository root, unless
//!   `--bin-dir` names a directory that already holds `js-lease-server` and `js`;
//! - the host file and the token file are written into a scratch directory
//!   (`js_testkit::dir::TestDir`, removed on every way out, a failed run too);
//! - the server is started there (`js_testkit::reaped`: stopped and reaped when the verb ends) and
//!   [`wait_ready`] polls `GET /healthz` until it answers or a deadline passes, with the probe and the
//!   clock given to it so a test needs neither a server nor time;
//! - [`Recorder`] makes the transcript text, from the commands and what they printed.

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use js_testkit::dir::TestDir;

/// The port the recording uses: the lease server's default.
pub const DEFAULT_PORT: u16 = 7300;
/// The recording's default place, under the repository root.
pub const DEFAULT_OUT: &str = "site/test/demo.transcript";
/// What the recording says the host file is.
pub const HOSTS_COMMENT: &str = "# hosts.toml: one host, listed by hand; there is no agent";
/// The host file: one host of eight GPUs.
pub const HOSTS_TOML: &str = r#"[[host]]
id = "gpu-01"
gpu_class = "nvidia-a100"

[host.resources]
cpu_millis = 64000
memory_bytes = 549755813888
gpus = 8

[host.capabilities]
isa = "x86-64-v3"
driver = "550"
"#;
/// The client's token: printable ASCII of at least 32 bytes, as the server requires.
pub const TOKEN: &str = "a-local-token-of-at-least-32-bytes";
/// The token file's name in the scratch directory.
pub const TOKEN_FILE: &str = "client-token";
/// How long the verb waits for the server to answer.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`wait_ready`] sleeps between two probes.
pub const READY_INTERVAL: Duration = Duration::from_millis(100);

/// Why the recording failed; the whole message.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("site-demo: {0}")]
pub struct Error(pub String);

/// The command line `program args...` as the recording shows it.
#[must_use]
pub fn shown(program: &str, args: &[String]) -> String {
    let mut line = program.to_owned();
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// The command that writes the token file, as the recording shows it (the verb writes the file itself).
#[must_use]
pub fn token_command() -> String {
    format!("printf '{TOKEN}\\n' > {TOKEN_FILE} && chmod 600 {TOKEN_FILE}")
}

/// The flags of the server start: `--listen` only off the default port.
#[must_use]
pub fn server_args(port: u16) -> Vec<String> {
    let mut args: Vec<String> = [
        "--state-file",
        "leases.toml",
        "--boot",
        "initialise",
        "--token-file",
        TOKEN_FILE,
        "--pool-config",
        "hosts.toml",
    ]
    .map(str::to_owned)
    .into();
    if port != DEFAULT_PORT {
        args.extend(["--listen".to_owned(), format!("127.0.0.1:{port}")]);
    }
    args
}

/// The arguments of `js` for `rest` (the verb and its arguments) against the server on `port`.
fn client_args(port: u16, rest: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--server".to_owned(),
        format!("http://127.0.0.1:{port}"),
        "--token-file".to_owned(),
        TOKEN_FILE.to_owned(),
    ];
    args.extend(rest.iter().map(|word| (*word).to_owned()));
    args
}

/// The arguments of the lease the recording acquires: a whole GPU host for an hour.
#[must_use]
pub fn acquire_args(port: u16) -> Vec<String> {
    client_args(
        port,
        &[
            "lease",
            "acquire",
            "--class",
            "gpu-host",
            "--gpus",
            "1",
            "--gpu-class",
            "nvidia-a100",
            "--max-wall",
            "1h",
        ],
    )
}

/// The arguments of `js lease list`.
#[must_use]
pub fn list_args(port: u16) -> Vec<String> {
    client_args(port, &["lease", "list"])
}

/// The arguments of `js lease release <id>`.
#[must_use]
pub fn release_args(port: u16, id: &str) -> Vec<String> {
    client_args(port, &["lease", "release", id])
}

/// Every command line the recording shows, in order, for a lease acquired as `id`: the token file, the
/// server start (with its `&`), and the four client commands.
#[must_use]
pub fn commands(port: u16, id: &str) -> Vec<String> {
    vec![
        token_command(),
        format!("{} &", shown("js-lease-server", &server_args(port))),
        shown("js", &acquire_args(port)),
        shown("js", &list_args(port)),
        shown("js", &release_args(port, id)),
        shown("js", &list_args(port)),
    ]
}

/// The lease id `js lease acquire` printed (`lease <id> host <host> expires <ms>`), if it did.
#[must_use]
pub fn lease_id(acquired: &str) -> Option<String> {
    acquired.lines().find_map(|line| {
        let mut words = line.split(' ');
        if words.next() != Some("lease") {
            return None;
        }
        let id = words.next()?;
        (words.next() == Some("host")).then(|| id.to_owned())
    })
}

/// The text of a transcript, built piece by piece.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recorder {
    text: String,
}

impl Recorder {
    /// A transcript that starts with the host file and the line that says what it is.
    #[must_use]
    pub fn with_host_file() -> Self {
        Self {
            text: format!("{HOSTS_COMMENT}\n{HOSTS_TOML}"),
        }
    }

    /// Adds a `$ ` line for `command`.
    pub fn command(&mut self, command: &str) {
        let _ = writeln!(self.text, "$ {command}");
    }

    /// Adds what a command printed, as it was (nothing for no output; a line end is added when missing).
    pub fn output(&mut self, printed: &str) {
        self.text.push_str(printed);
        if !printed.is_empty() && !printed.ends_with('\n') {
            self.text.push('\n');
        }
    }

    /// The transcript.
    #[must_use]
    pub fn finish(self) -> String {
        self.text
    }
}

/// The time, and sleeping, for [`wait_ready`].
pub trait Clock {
    /// How long since the wait began.
    fn elapsed(&self) -> Duration;
    /// Waits `for_how_long`.
    fn sleep(&mut self, for_how_long: Duration);
}

/// The wall clock.
#[derive(Debug)]
pub struct SystemClock(Instant);

impl SystemClock {
    /// A clock that starts now.
    #[must_use]
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, for_how_long: Duration) {
        std::thread::sleep(for_how_long);
    }
}

/// Asks `probe` until it says `Ok(true)`, `interval` apart, for at most `timeout` by `clock`. A probe that
/// says `Err` ends the wait with that reason (the server died).
///
/// # Errors
/// The probe's reason, or that nothing answered within `timeout`.
pub fn wait_ready(
    mut probe: impl FnMut() -> Result<bool, String>,
    clock: &mut impl Clock,
    timeout: Duration,
    interval: Duration,
) -> Result<(), String> {
    loop {
        if probe()? {
            return Ok(());
        }
        if clock.elapsed() >= timeout {
            return Err(format!("the server did not answer within {timeout:?}"));
        }
        clock.sleep(interval);
    }
}

/// Whether the first line of an HTTP response (`HTTP/1.1 200 OK`) is a 200.
#[must_use]
pub fn healthy(status_line: &str) -> bool {
    let mut words = status_line.split_whitespace();
    words.next().is_some_and(|v| v.starts_with("HTTP/")) && words.next() == Some("200")
}

/// One `GET /healthz` against `127.0.0.1:port`: whether the server says 200. A refused connection is no.
fn probe_healthz(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let limit = Some(Duration::from_millis(500));
    let _ = stream.set_read_timeout(limit);
    let _ = stream.set_write_timeout(limit);
    status_line(stream, port).is_ok_and(|line| healthy(&line))
}

/// Sends `GET /healthz` on `stream` and returns the first line of the answer.
fn status_line(mut stream: TcpStream, port: u16) -> std::io::Result<String> {
    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status)?;
    Ok(status)
}

/// What a run needs to know.
#[derive(Debug)]
pub struct SiteDemo<'a> {
    /// The cargo program.
    pub cargo: &'a std::ffi::OsStr,
    /// The repository root, where cargo builds.
    pub root: &'a Path,
    /// Where `js-lease-server` and `js` are, instead of building them.
    pub bin_dir: Option<&'a Path>,
    /// Where the transcript is written.
    pub out: &'a Path,
    /// The server's port on 127.0.0.1.
    pub port: u16,
    /// How long to wait for the server to answer.
    pub ready_timeout: Duration,
}

/// The reason for a failed step `what`.
fn failed(what: &str, why: impl std::fmt::Display) -> Error {
    Error(format!("{what}: {why}"))
}

/// The conversion of an I/O error of the step `what` to the verb's error.
fn io_err(what: String) -> impl FnOnce(std::io::Error) -> Error {
    move |e| failed(&what, e)
}

/// Builds the two programs in `root` and returns the directory they are in.
///
/// # Errors
/// Cargo cannot be started or fails.
pub fn build(cargo: &std::ffi::OsStr, root: &Path) -> Result<PathBuf, Error> {
    let status = Command::new(cargo)
        .args([
            "build",
            "--release",
            "-p",
            "js-lease-server",
            "-p",
            "js-cli",
        ])
        .current_dir(root)
        .stdout(Stdio::null())
        .status()
        .map_err(io_err("cannot start cargo".to_owned()))?;
    if !status.success() {
        return Err(Error(format!("cargo build failed ({status})")));
    }
    Ok(root.join("target").join("release"))
}

/// Writes the token file, owner-only.
fn write_token(dir: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(TOKEN_FILE))?;
    writeln!(file, "{TOKEN}")
}

/// Runs `js` with `args` in `dir` and returns what it printed (standard output, then standard error).
fn run_client(js: &Path, dir: &Path, args: &[String]) -> Result<String, Error> {
    let out = Command::new(js)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(io_err(format!("cannot start {}", js.display())))?;
    let mut printed = String::from_utf8_lossy(&out.stdout).into_owned();
    printed.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        return Ok(printed);
    }
    Err(Error(format!(
        "`{}` ended with {}: {}",
        shown("js", args),
        out.status,
        printed.trim()
    )))
}

/// Records the demonstration and writes the transcript.
///
/// # Errors
/// A step failed: the build, the scratch files, the server start or its readiness, a client command, or the
/// write of the transcript. The scratch directory and the server are gone either way.
pub fn run(demo: &SiteDemo<'_>) -> Result<String, Error> {
    let bins = match demo.bin_dir {
        Some(dir) => dir.to_path_buf(),
        None => build(demo.cargo, demo.root)?,
    };
    // The server is started from another directory: its path must not be relative to this one.
    let bins =
        std::path::absolute(&bins).map_err(io_err(format!("cannot resolve {}", bins.display())))?;
    // Declared first, so it drops last: the server is stopped before its directory goes.
    let dir = TestDir::under(std::env::temp_dir(), "xtask-site-demo");
    std::fs::write(dir.join("hosts.toml"), HOSTS_TOML)
        .map_err(io_err("cannot write hosts.toml".to_owned()))?;
    write_token(&dir).map_err(io_err("cannot write the token file".to_owned()))?;

    let log_path = dir.join("server.log");
    let log =
        std::fs::File::create(&log_path).map_err(io_err("cannot make server.log".to_owned()))?;
    let err_log = log
        .try_clone()
        .map_err(io_err("cannot make server.log".to_owned()))?;
    let mut server = js_testkit::reaped::spawn(
        Command::new(bins.join("js-lease-server"))
            .args(server_args(demo.port))
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err_log),
        3,
    )
    .map_err(io_err("cannot start js-lease-server".to_owned()))?;
    let port = demo.port;
    let probe = || match server.try_wait() {
        Ok(Some(status)) => Err(format!(
            "js-lease-server ended with {status}: {}",
            std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .trim()
        )),
        Ok(None) | Err(_) => Ok(probe_healthz(port)),
    };
    wait_ready(
        probe,
        &mut SystemClock::start(),
        demo.ready_timeout,
        READY_INTERVAL,
    )
    .map_err(Error)?;

    let js = bins.join("js");
    let mut rec = Recorder::with_host_file();
    // The first three lines do not depend on the lease.
    for line in &commands(port, "")[..3] {
        rec.command(line);
    }
    let acquired = run_client(&js, &dir, &acquire_args(port))?;
    let id = lease_id(&acquired).ok_or_else(|| {
        Error(format!(
            "`js lease acquire` printed no lease: {}",
            acquired.trim()
        ))
    })?;
    rec.output(&acquired);
    let lines = commands(port, &id);
    let rest = [list_args(port), release_args(port, &id), list_args(port)];
    for (line, args) in lines[3..].iter().zip(&rest) {
        rec.command(line);
        rec.output(&run_client(&js, &dir, args)?);
    }
    if let Some(parent) = demo.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(io_err(
            "cannot make the directory of the transcript".to_owned(),
        ))?;
    }
    std::fs::write(demo.out, rec.finish())
        .map_err(io_err(format!("cannot write {}", demo.out.display())))?;
    Ok(format!("recorded {}", demo.out.display()))
}

#[cfg(test)]
mod tests;
