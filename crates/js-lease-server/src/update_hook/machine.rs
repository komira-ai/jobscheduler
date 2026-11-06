//! The hook on this machine: the steps over the real unit manager (`systemctl`), release link,
//! store and `/healthz`.
//!
//! What a step needs comes from three places: the declaration, read from
//! [`DECLARATION_PATH`](super::DECLARATION_PATH); the flags before the step (`--store`,
//! `--healthz-url`); and the machine itself (how the state directory's owner is looked at, how
//! a program is run). A [`Machine`] holds the seams, so a test runs the same steps over a
//! declaration, a state directory and a store of its own.
//!
//! The store file is the one the server's `--store` names, read the way the server reads it
//! (one line, `postgres://...` or `sqlite:<path>`), except that its owner may be any user (the
//! hook runs as root, the server as its own user): it must be a regular file that only its
//! owner can read or write. Nothing of its content is ever printed: a store that cannot be
//! reached is said to be, and no more ([`StoreUnreachable`]).

use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use js_state::postgres::schema::{self, SchemaError, Supported};
use js_state::{LockState, WriterLock, WriterTarget};
use js_update_hook::{
    CommandRunner, DirStat, NoGate, SystemClock, SystemLink, SystemUnits, fd_stat,
};

use super::declaration::{DECLARATION_PATH, Declaration};
use super::ports::{Health, Ports, Schema, StoreUnreachable, StoreView, Writer};
use super::step::{self, Env, StatFn};
use super::{Done, Invocation, Outcome, Params, Reason, Report};

/// The most bytes of the store file that are read: one line.
const STORE_FILE_MAX: u64 = 64 * 1024;

/// How long a look at `/healthz` may take: the connect, the request and the answer, each.
pub const HEALTHZ_LIMIT: Duration = Duration::from_secs(5);

/// Runs a program with an empty environment and its standard input closed, and returns what
/// it printed when it exits with status 0. The error names the program and how it failed,
/// never what it printed.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.run_in(program, args, &[])
    }

    fn run_in(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        let output = Command::new(program)
            .args(args)
            .env_clear()
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .map_err(|why| format!("{program}: {why}"))?;
        match output.status.success() {
            true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            false => Err(format!("{program}: {}", output.status)),
        }
    }
}

/// The store `--store` names, read only.
pub struct StoreProbe {
    target: WriterTarget,
}

impl StoreProbe {
    /// The store at `target`.
    #[must_use]
    pub fn new(target: WriterTarget) -> StoreProbe {
        StoreProbe { target }
    }
}

/// What `found` is to a release that runs on `supported`.
#[must_use]
pub fn judge(found: u32, supported: Supported) -> Schema {
    match found < supported.min {
        true => Schema::TooOld {
            found,
            min: supported.min,
        },
        false => Schema::Runs(found),
    }
}

impl StoreView for StoreProbe {
    fn writer(&self) -> Result<Writer, StoreUnreachable> {
        match WriterLock::probe(&self.target).map_err(|_| StoreUnreachable)? {
            LockState::Free => Ok(Writer::Free),
            LockState::Held { holder } => Ok(Writer::Held {
                holder: holder.to_string(),
            }),
        }
    }

    fn schema(&self) -> Result<Schema, StoreUnreachable> {
        let WriterTarget::Postgres(config) = &self.target else {
            return Ok(Schema::Unchecked);
        };
        match schema::current(config) {
            Ok(found) => Ok(judge(found, Supported::this_build())),
            Err(SchemaError::NotInitialised) => Ok(Schema::Missing),
            Err(_) => Err(StoreUnreachable),
        }
    }
}

/// `GET /healthz` over plain HTTP/1.1 to a loopback address.
pub struct HttpHealth {
    addr: SocketAddr,
    limit: Duration,
}

impl HttpHealth {
    /// `/healthz` at `addr`, each part of a look bounded by `limit`.
    #[must_use]
    pub fn new(addr: SocketAddr, limit: Duration) -> HttpHealth {
        HttpHealth { addr, limit }
    }

    fn get(&self) -> io::Result<u16> {
        let mut stream = TcpStream::connect_timeout(&self.addr, self.limit)?;
        stream.set_read_timeout(Some(self.limit))?;
        stream.set_write_timeout(Some(self.limit))?;
        let addr = self.addr;
        let request = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes())?;
        // The status line is all that is read: the rest of the answer is not waited for.
        let mut head = Vec::new();
        BufReader::new(stream.take(1024)).read_until(b'\n', &mut head)?;
        let text = String::from_utf8_lossy(&head);
        let mut words = text.lines().next().unwrap_or_default().split(' ');
        let version = words.next().unwrap_or_default();
        words
            .next()
            .filter(|_| version.starts_with("HTTP/1."))
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| io::Error::other("the answer is not HTTP/1.x with a status"))
    }
}

impl Health for HttpHealth {
    fn status(&self) -> Result<u16, String> {
        self.get()
            .map_err(|why| format!("GET http://{}/healthz: {why}", self.addr))
    }
}

/// Reads the store file at `path`: a regular file that only its owner can read or write,
/// opened without waiting (a pipe in its place is refused, not waited on).
fn read_store_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "must be a regular file that only its owner can read or write",
        ));
    }
    let mut bytes = Vec::new();
    file.take(STORE_FILE_MAX).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The store the file at `path` names. The refusal names the path, never the content.
///
/// # Errors
/// The file cannot be read, is not owner-only, or does not hold one store line.
pub fn store_target(path: &Path) -> Result<WriterTarget, String> {
    let bytes =
        read_store_file(path).map_err(|why| format!("--store: {}: {why}", path.display()))?;
    crate::config::hook_store_target(&bytes, path).map_err(|why| why.to_string())
}

/// What the hook takes from the machine besides the command line and the params.
pub struct Machine<'a> {
    /// Where the declaration is.
    pub declaration: &'a Path,
    /// How the state directory and the directories above it are looked at.
    pub stat: &'a StatFn,
    /// How `systemctl` is run.
    pub runner: &'a dyn CommandRunner,
}

fn system_stat(_: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
    fd_stat(fd)
}

impl Machine<'static> {
    /// This machine: the declaration the updater installs, directories looked at as they are,
    /// programs run as they are.
    #[must_use]
    pub fn system() -> Self {
        Self {
            declaration: Path::new(DECLARATION_PATH),
            stat: &system_stat,
            runner: &SystemRunner,
        }
    }
}

/// A failure before the step: nothing was done.
fn refused(reason: Reason, detail: String) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(reason, detail),
    }
}

/// Runs the step `invocation` names on `machine`, with the params already read.
#[must_use]
pub fn run_step(invocation: &Invocation, params: &Params, machine: &Machine<'_>) -> Done {
    let declaration = match Declaration::read(machine.declaration) {
        Ok(declaration) => declaration,
        Err(why) => return refused(Reason::StateInvalid, why.to_string()),
    };
    let flags = &invocation.flags;
    let target = flags
        .store
        .as_deref()
        .ok_or_else(|| "--store is required".to_owned())
        .and_then(store_target);
    let (target, healthz) = match (target, flags.healthz) {
        (Ok(target), Some(healthz)) => (target, healthz),
        (Err(why), _) => return refused(Reason::BadParams, why),
        (Ok(_), None) => return refused(Reason::BadParams, "--healthz-url is required".to_owned()),
    };
    let units = SystemUnits::new(machine.runner);
    let store = StoreProbe::new(target);
    let health = HttpHealth::new(healthz, HEALTHZ_LIMIT);
    let env = Env {
        ports: Ports {
            units: &units,
            link: &SystemLink,
            store: &store,
            health: &health,
            clock: &SystemClock,
            gate: &NoGate,
        },
        declaration: &declaration,
        stat: machine.stat,
    };
    step::run(invocation.step, &env, params)
}
