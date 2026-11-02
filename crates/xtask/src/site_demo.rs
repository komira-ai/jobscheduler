//! `cargo xtask site-demo [--out <path>] [--bin-dir <dir>] [--port <n>]`: records the demonstration on
//! the landing page of jobscheduler.dev, `site/test/demo.transcript`. It builds the lease server and the
//! client, leases a host from a static host file on this machine, lists the lease and releases it, and
//! writes the transcript to `--out` (default `site/test/demo.transcript` under `--root`). It needs cargo
//! and a free port 7300 on 127.0.0.1 (`--port 0` lets the server pick one; the transcript then shows the
//! port it picked). Lease ids and times differ on every run, so a new recording differs from the committed
//! one in those.
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
//! - the server is started there (`js_testkit::reaped`: stopped and reaped when the verb ends) with its
//!   output in `server.log`. It prints `listening on <address>` once it is bound, and the verb reads the
//!   address from the log, so the client talks to the port the server really holds. [`wait_ready`] then
//!   asks `GET /healthz` until it answers 200 or a deadline passes, with the probe and the clock given to
//!   it so a test needs neither a server nor time;
//! - [`Recorder`] makes the transcript text, from the commands and what they printed.
//!
//! [`Error`] names each way the recording fails.

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use js_testkit::dir::TestDir;

/// The port the recording uses: the lease server's default.
pub(crate) const DEFAULT_PORT: u16 = 7300;
/// The recording's default place, under the repository root.
pub(crate) const DEFAULT_OUT: &str = "site/test/demo.transcript";
/// What the recording says the host file is.
const HOSTS_COMMENT: &str = "# hosts.toml: one host, listed by hand; there is no agent";
/// The host file: one host of eight GPUs.
const HOSTS_TOML: &str = r#"[[host]]
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
const TOKEN: &str = "a-local-token-of-at-least-32-bytes";
/// The token file's name in the scratch directory.
const TOKEN_FILE: &str = "client-token";
/// How long the verb waits for the server to answer.
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`wait_ready`] sleeps between two probes.
const READY_INTERVAL: Duration = Duration::from_millis(100);
/// What the server prints, followed by the address it is bound to, once it can be reached.
const LISTENING: &str = "listening on ";

/// Why the recording failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `cargo build` ran and failed.
    #[error("`{command}` ended with {status}")]
    BuildFailed {
        /// The cargo command line.
        command: String,
        /// How it ended.
        status: ExitStatus,
    },
    /// A program could not be started.
    #[error("cannot start {program}: {source}")]
    Spawn {
        /// The program that was asked for.
        program: String,
        /// Why it could not start.
        #[source]
        source: std::io::Error,
    },
    /// The directory of the programs could not be made absolute.
    #[error("cannot resolve {path}: {source}")]
    ResolveBinDir {
        /// The directory as given.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The scratch directory, or a file in it, could not be made.
    #[error("cannot prepare the scratch directory {path}: {source}")]
    Scratch {
        /// The directory.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The server ended before it answered.
    #[error("js-lease-server ended with {status} before it answered: {log}")]
    ServerExited {
        /// How it ended.
        status: ExitStatus,
        /// What it printed.
        log: String,
    },
    /// The server did not answer 200 in time.
    #[error("{url} did not answer 200 within {timeout:?}")]
    NotReady {
        /// The address that was asked (or, when the server never said where it listens, the one requested).
        url: String,
        /// The deadline.
        timeout: Duration,
    },
    /// Asking the server failed in a way that waiting does not mend.
    #[error("cannot ask {url}: {source}")]
    Probe {
        /// The address that was asked.
        url: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A client command ended with a failure status.
    #[error("`{command}` ended with {status}: {output}")]
    Client {
        /// The command line, as the recording shows it.
        command: String,
        /// How it ended.
        status: ExitStatus,
        /// What it printed.
        output: String,
    },
    /// `js lease acquire` succeeded and printed no lease id.
    #[error("`js lease acquire` printed no lease id: {output}")]
    NoLeaseId {
        /// What it printed.
        output: String,
    },
    /// The directory of the transcript could not be made.
    #[error("cannot make the directory {path}: {source}")]
    CreateDir {
        /// The directory.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The transcript could not be written.
    #[error("cannot write {path}: {source}")]
    WriteTranscript {
        /// The transcript's path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// The command line `program args...` as the recording shows it.
fn shown(program: &str, args: &[String]) -> String {
    let mut line = program.to_owned();
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// The command that writes the token file, as the recording shows it (the verb writes the file itself).
fn token_command() -> String {
    format!("printf '{TOKEN}\\n' > {TOKEN_FILE} && chmod 600 {TOKEN_FILE}")
}

/// The flags of the server start: `--listen` only off the default port.
fn server_args(port: u16) -> Vec<String> {
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
fn acquire_args(port: u16) -> Vec<String> {
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
fn list_args(port: u16) -> Vec<String> {
    client_args(port, &["lease", "list"])
}

/// The arguments of `js lease release <id>`.
fn release_args(port: u16, id: &str) -> Vec<String> {
    client_args(port, &["lease", "release", id])
}

/// Every command line the recording shows, in order, for a lease acquired as `id`: the token file, the
/// server start (with its `&`), and the four client commands.
fn commands(port: u16, id: &str) -> Vec<String> {
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
fn lease_id(acquired: &str) -> Option<String> {
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
#[derive(Debug)]
struct Recorder {
    text: String,
}

impl Recorder {
    /// A transcript that starts with the host file and the line that says what it is.
    fn with_host_file() -> Self {
        Self {
            text: format!("{HOSTS_COMMENT}\n{HOSTS_TOML}"),
        }
    }

    /// Adds a `$ ` line for `command`.
    fn command(&mut self, command: &str) {
        let _ = writeln!(self.text, "$ {command}");
    }

    /// Adds what a command printed, as it was (nothing for no output; a line end is added when missing).
    fn output(&mut self, printed: &str) {
        self.text.push_str(printed);
        if !printed.is_empty() && !printed.ends_with('\n') {
            self.text.push('\n');
        }
    }

    /// The transcript.
    fn finish(self) -> String {
        self.text
    }
}

/// The time, and sleeping, for [`wait_ready`].
trait Clock {
    /// How long since the wait began.
    fn elapsed(&self) -> Duration;
    /// Waits `for_how_long`.
    fn sleep(&mut self, for_how_long: Duration);
}

/// The wall clock.
struct SystemClock(Instant);

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, for_how_long: Duration) {
        std::thread::sleep(for_how_long);
    }
}

/// Asks `probe` until it says `Some(ready)`, `interval` apart, for at most `timeout` by `clock`, and
/// returns what it said; `None` when the time ran out. A probe that says `Err` ends the wait with it.
fn wait_ready<T, E>(
    mut probe: impl FnMut() -> Result<Option<T>, E>,
    clock: &mut impl Clock,
    timeout: Duration,
    interval: Duration,
) -> Result<Option<T>, E> {
    loop {
        if let Some(ready) = probe()? {
            return Ok(Some(ready));
        }
        if clock.elapsed() >= timeout {
            return Ok(None);
        }
        clock.sleep(interval);
    }
}

/// Whether the first line of an HTTP response (`HTTP/1.1 200 OK`) is a 200.
fn healthy(status_line: &str) -> bool {
    let mut words = status_line.split_whitespace();
    words.next().is_some_and(|v| v.starts_with("HTTP/")) && words.next() == Some("200")
}

/// The address a server says it listens on: the first `listening on <address>` line of its `log`.
fn listening_addr(log: &str) -> Option<SocketAddr> {
    log.lines()
        .find_map(|line| line.strip_prefix(LISTENING)?.trim().parse().ok())
}

/// What the answer to one `GET /healthz` at `addr` says: `Some(addr)` when the server is up, `None` when
/// it is not yet (a status that is not 200, a refused or reset connection, an answer that did not come in
/// time), and an error for any other failure.
fn judge(addr: SocketAddr, answer: std::io::Result<String>) -> Result<Option<SocketAddr>, Error> {
    use std::io::ErrorKind::{
        ConnectionRefused, ConnectionReset, TimedOut, UnexpectedEof, WouldBlock,
    };
    match answer {
        Ok(line) => Ok(healthy(&line).then_some(addr)),
        Err(e)
            if matches!(
                e.kind(),
                ConnectionRefused | ConnectionReset | TimedOut | UnexpectedEof | WouldBlock
            ) =>
        {
            Ok(None)
        }
        Err(source) => Err(Error::Probe {
            url: format!("http://{addr}"),
            source,
        }),
    }
}

/// One `GET /healthz` against `addr`: the first line of the answer.
fn probe_healthz(addr: SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    let limit = Some(Duration::from_millis(500));
    stream.set_read_timeout(limit)?;
    stream.set_write_timeout(limit)?;
    let request = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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
    /// The directory the scratch directory of the run is made in.
    pub scratch: &'a Path,
    /// The server's port on 127.0.0.1; 0 lets the server pick a free one.
    pub port: u16,
    /// How long to wait for the server to answer.
    pub ready_timeout: Duration,
}

/// Builds the two programs in `root` and returns the directory they are in.
fn build(cargo: &std::ffi::OsStr, root: &Path) -> Result<PathBuf, Error> {
    let args = [
        "build",
        "--release",
        "-p",
        "js-lease-server",
        "-p",
        "js-cli",
    ];
    let status = Command::new(cargo)
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .status()
        .map_err(|source| Error::Spawn {
            program: cargo.to_string_lossy().into_owned(),
            source,
        })?;
    if !status.success() {
        return Err(Error::BuildFailed {
            command: format!("{} {}", cargo.to_string_lossy(), args.join(" ")),
            status,
        });
    }
    Ok(root.join("target").join("release"))
}

/// Writes the host file and the token file (owner-only) in `dir`, and makes the server's log there: two
/// handles to it, for the server's standard output and error.
fn prepare(dir: &Path) -> std::io::Result<(std::fs::File, std::fs::File)> {
    std::fs::write(dir.join("hosts.toml"), HOSTS_TOML)?;
    let mut token = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(TOKEN_FILE))?;
    writeln!(token, "{TOKEN}")?;
    let log = std::fs::File::create(dir.join("server.log"))?;
    let err_log = log.try_clone()?;
    Ok((log, err_log))
}

/// Runs `js` with `args` in `dir` and returns what it printed (standard output, then standard error).
fn run_client(js: &Path, dir: &Path, args: &[String]) -> Result<String, Error> {
    let out = Command::new(js)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|source| Error::Spawn {
            program: js.display().to_string(),
            source,
        })?;
    let mut printed = String::from_utf8_lossy(&out.stdout).into_owned();
    printed.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        return Ok(printed);
    }
    Err(Error::Client {
        command: shown("js", args),
        status: out.status,
        output: printed.trim().to_owned(),
    })
}

/// Records the demonstration and writes the transcript; returns the line to print.
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
        std::path::absolute(&bins).map_err(|source| Error::ResolveBinDir { path: bins, source })?;
    let scratch = |source| Error::Scratch {
        path: demo.scratch.to_path_buf(),
        source,
    };
    std::fs::create_dir_all(demo.scratch).map_err(scratch)?;
    // Declared first, so it drops last: the server is stopped before its directory goes.
    let dir = TestDir::under(demo.scratch, "xtask-site-demo");
    let (log, err_log) = prepare(&dir).map_err(scratch)?;
    let server_program = bins.join("js-lease-server");
    let mut server = js_testkit::reaped::spawn(
        Command::new(&server_program)
            .args(server_args(demo.port))
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err_log),
        3,
    )
    .map_err(|source| Error::Spawn {
        program: server_program.display().to_string(),
        source,
    })?;

    let log_path = dir.join("server.log");
    let mut listening = None;
    let probe = || {
        // Asked first: a server that has ended has written all it will, so the log read next is whole.
        let ended = server.try_wait();
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if let Ok(Some(status)) = ended {
            return Err(Error::ServerExited {
                status,
                log: log.trim().to_owned(),
            });
        }
        // The log only grows, so the first line to say where the server listens stays the same.
        listening = listening_addr(&log);
        match listening {
            Some(addr) => judge(addr, probe_healthz(addr)),
            None => Ok(None),
        }
    };
    let mut clock = SystemClock(Instant::now());
    let ready = wait_ready(probe, &mut clock, demo.ready_timeout, READY_INTERVAL)?;
    let Some(addr) = ready else {
        let at = listening.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], demo.port)));
        return Err(Error::NotReady {
            url: format!("http://{at}"),
            timeout: demo.ready_timeout,
        });
    };

    // The port the server really holds (the one asked for, unless that was 0): the recording shows the
    // commands as typed for that port, as if it had been asked for.
    let port = addr.port();
    let js = bins.join("js");
    let mut rec = Recorder::with_host_file();
    // The first three lines do not depend on the lease.
    for line in &commands(port, "")[..3] {
        rec.command(line);
    }
    let acquired = run_client(&js, &dir, &acquire_args(port))?;
    let id = lease_id(&acquired).ok_or_else(|| Error::NoLeaseId {
        output: acquired.trim().to_owned(),
    })?;
    rec.output(&acquired);
    let lines = commands(port, &id);
    let rest = [list_args(port), release_args(port, &id), list_args(port)];
    for (line, args) in lines[3..].iter().zip(&rest) {
        rec.command(line);
        rec.output(&run_client(&js, &dir, args)?);
    }
    if let Some(parent) = demo.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| Error::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(demo.out, rec.finish()).map_err(|source| Error::WriteTranscript {
        path: demo.out.to_path_buf(),
        source,
    })?;
    Ok(format!("recorded {}", demo.out.display()))
}

#[cfg(test)]
mod tests;
