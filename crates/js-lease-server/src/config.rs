//! The flags, and the checks that decide whether the server may start.

use std::fs::OpenOptions;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use js_lease::{Boot, Settings};
use rustix::fs::OFlags;
use rustix::process::geteuid;
use subtle::ConstantTimeEq;

use crate::capacity::{CapacitySource, StaticPoolSource};
use crate::error::StartupError;

/// The command line. Configuration is flags only; secrets come from files.
#[derive(Debug, Parser)]
#[command(
    name = "js-lease-server",
    version,
    about = "Serves a lease table over an authenticated HTTP JSON API"
)]
pub struct Cli {
    /// The address to listen on. Anything but a loopback address needs --tls-cert and
    /// --tls-key: a bearer token never crosses a network in clear.
    #[arg(long, default_value = "127.0.0.1:7300")]
    pub listen: SocketAddr,
    /// The lease file.
    #[arg(long)]
    pub state_file: PathBuf,
    /// `initialise` creates the lease file when it is missing (the first boot);
    /// `resume` requires it. There is no default, so a lost state file is never
    /// silently started empty.
    #[arg(long, value_enum)]
    pub boot: BootMode,
    /// The TOML file listing the hosts to lease.
    #[arg(long)]
    pub pool_config: PathBuf,
    /// A file holding the bearer token. It must be owner-only (mode 0600 or 0400).
    #[arg(long)]
    pub token_file: PathBuf,
    /// The ttl of a lease whose client names none, such as `600s`, `10m` or `1h`.
    #[arg(long, default_value = "600s", value_parser = parse_duration)]
    pub default_ttl: Duration,
    /// The longest ttl granted at one time.
    #[arg(long, default_value = "3600s", value_parser = parse_duration)]
    pub max_ttl: Duration,
    /// How often the table is ticked (expiries, retries, pending starts).
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    pub tick: Duration,
    /// The PEM certificate chain to serve over TLS.
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    /// The PEM private key of the certificate.
    #[arg(long)]
    pub tls_key: Option<PathBuf>,
    /// The most connections held open at once, handshaking, idle or busy. At the cap
    /// the server stops accepting until one ends.
    #[arg(long, default_value = "256", value_parser = clap::value_parser!(u16).range(1..))]
    pub max_connections: u16,
    /// How long a TLS client has to finish its handshake.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub handshake_timeout: Duration,
    /// How long a client has to send the head of a request, from the connection's
    /// start and again after each response on it. This is also how long a keep-alive
    /// connection may sit idle between requests.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub header_timeout: Duration,
    /// How long a write to a client may make no progress before the connection is
    /// closed: a client that does not read its answers cannot hold a connection.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub write_timeout: Duration,
    /// The most requests served on one connection. The last answer says
    /// `Connection: close`; a client opens another connection for the next request.
    #[arg(long, default_value = "100", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_requests: u32,
    /// How long a stop waits for the requests in flight before it cuts what is left.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub drain_timeout: Duration,
}

/// What a client may hold open (see the module doc of `listener`). The flags set
/// these; the defaults are what the flags default to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most connections open at once.
    pub max_connections: u16,
    /// How long a TLS handshake may take.
    pub handshake_timeout: Duration,
    /// How long the head of a request may take to arrive; also the idle time of a
    /// keep-alive connection between two requests.
    pub header_timeout: Duration,
    /// How long a write to a client may make no progress.
    pub write_timeout: Duration,
    /// The most requests one connection serves.
    pub max_requests: u32,
    /// How long a stop waits for requests in flight.
    pub drain_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            handshake_timeout: Duration::from_secs(10),
            header_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            max_requests: 100,
            drain_timeout: Duration::from_secs(10),
        }
    }
}

impl From<&Cli> for Limits {
    fn from(cli: &Cli) -> Self {
        Self {
            max_connections: cli.max_connections,
            handshake_timeout: cli.handshake_timeout,
            header_timeout: cli.header_timeout,
            write_timeout: cli.write_timeout,
            max_requests: cli.max_requests,
            drain_timeout: cli.drain_timeout,
        }
    }
}

/// What a missing lease file means (`--boot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BootMode {
    /// A missing file starts an empty table.
    Initialise,
    /// A missing file is an error.
    Resume,
}

impl From<BootMode> for Boot {
    fn from(mode: BootMode) -> Self {
        match mode {
            BootMode::Initialise => Boot::Initialise,
            BootMode::Resume => Boot::Resume,
        }
    }
}

/// A duration in whole seconds: `90`, `90s`, `15m` or `2h`. Zero is refused.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (count, unit) = text.split_at(digits);
    let factor = ["", "s", "m", "h"].iter().position(|known| *known == unit);
    let factor = factor.map(|at| [1u64, 1, 60, 3600][at]);
    let seconds = count
        .parse::<u64>()
        .ok()
        .zip(factor)
        .and_then(|(count, factor)| count.checked_mul(factor));
    match seconds {
        Some(seconds) if seconds > 0 => Ok(Duration::from_secs(seconds)),
        _ => Err(format!(
            "{text:?} is not a positive duration such as 600, 600s, 10m or 1h"
        )),
    }
}

/// The bearer token. It is compared in constant time and never printed.
#[derive(Clone)]
pub struct Token(Vec<u8>);

impl Token {
    /// Whether `given` is the token. The comparison takes the same time for any two
    /// tokens of one length; a token of another length is refused at once (the length
    /// of a token is not a secret).
    pub(crate) fn matches(&self, given: &[u8]) -> bool {
        self.0.ct_eq(given).into()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The certificate and key files of `--tls-cert` and `--tls-key`.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    /// The PEM certificate chain.
    pub cert: PathBuf,
    /// The PEM private key.
    pub key: PathBuf,
}

/// A command line that passed every check that needs no running server: the flags make
/// sense together, and the token was read from an owner-only file.
pub struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) state_file: PathBuf,
    pub(crate) boot: Boot,
    pub(crate) settings: Settings,
    pub(crate) tick: Duration,
    pub(crate) token: Token,
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) limits: Limits,
    pub(crate) capacity: Box<dyn CapacitySource>,
}

impl Config {
    /// Checks `cli`: the ttls are consistent, a listen address that is not loopback has
    /// TLS flags, and the token file is owner-only and holds a token.
    pub fn from_cli(cli: Cli) -> Result<Self, StartupError> {
        let settings = Settings::new(cli.default_ttl, cli.max_ttl)
            .map_err(|err| StartupError::new("--default-ttl", err))?;
        let tls = tls_files(&cli)?;
        let token = read_token(&cli.token_file)?;
        let limits = Limits::from(&cli);
        Ok(Self {
            listen: cli.listen,
            state_file: cli.state_file,
            boot: cli.boot.into(),
            settings,
            tick: cli.tick,
            token,
            tls,
            limits,
            capacity: Box::new(StaticPoolSource::new(cli.pool_config)),
        })
    }

    /// The same configuration taking its hosts from `capacity` instead of the file
    /// `--pool-config` names.
    pub fn with_capacity_source(mut self, capacity: Box<dyn CapacitySource>) -> Self {
        self.capacity = capacity;
        self
    }

    /// The same configuration with other connection limits than the flags gave.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// How often the production ticker ticks (`--tick`).
    pub fn tick_period(&self) -> Duration {
        self.tick
    }
}

fn tls_files(cli: &Cli) -> Result<Option<TlsFiles>, StartupError> {
    let both = |cert: &Path, key: &Path| TlsFiles {
        cert: cert.to_path_buf(),
        key: key.to_path_buf(),
    };
    let files = match (&cli.tls_cert, &cli.tls_key) {
        (Some(cert), Some(key)) => Some(both(cert, key)),
        (None, None) => None,
        (Some(_), None) => return Err(StartupError::new("--tls-key", "required with --tls-cert")),
        (None, Some(_)) => return Err(StartupError::new("--tls-cert", "required with --tls-key")),
    };
    let clear = files.is_none() & !cli.listen.ip().is_loopback();
    match clear {
        true => Err(StartupError::new(
            "--listen",
            format!(
                "{} is not a loopback address, so --tls-cert and --tls-key are required: a bearer token never crosses a network in clear",
                cli.listen
            ),
        )),
        false => Ok(files),
    }
}

/// Whether a secret file may be trusted: a regular file, owned by the user running the
/// server (`euid`), that nobody else can read or write (mode 0600 or 0400).
fn trusted_private_file(is_file: bool, mode: u32, owner: u32, euid: u32) -> bool {
    is_file & matches!(mode, 0o600 | 0o400) & (owner == euid)
}

/// The fewest bytes a bearer token may have after its trailing whitespace is dropped.
/// A token that anyone on the network can try to guess is only as strong as it is long;
/// 32 bytes of a random source is out of reach.
pub const MIN_TOKEN_BYTES: usize = 32;

/// The token a file's bytes hold: its content without trailing whitespace, which must
/// be one run of printable ASCII (an HTTP header value cannot carry anything else, so
/// any other token could never match a request) of at least [`MIN_TOKEN_BYTES`] bytes.
fn parse_token(bytes: &[u8]) -> Result<Token, String> {
    let token = bytes.trim_ascii_end();
    let printable = token.iter().all(u8::is_ascii_graphic);
    match (token.is_empty(), printable, token.len() < MIN_TOKEN_BYTES) {
        (true, _, _) => Err("is empty".to_string()),
        (false, false, _) => Err(
            "must hold one token of printable ASCII with no space or line break inside it"
                .to_string(),
        ),
        (false, true, true) => Err(format!(
            "holds a token of {} bytes; a token must have at least {MIN_TOKEN_BYTES} (use a random one)",
            token.len()
        )),
        (false, true, false) => Ok(Token(token.to_vec())),
    }
}

/// Reads the secret file at `path`, named by `flag`, after checking, on the open file,
/// that it is a regular file owned by the server's user that only that user can use.
/// The file is opened without blocking, so a named pipe at the path is refused instead
/// of waited on; a symbolic link is followed (secret mounts are links) and the checks
/// are on what it leads to.
pub(crate) fn read_private(path: &Path, flag: &'static str) -> Result<Vec<u8>, StartupError> {
    let refuse = StartupError::because(flag);
    let nonblocking = OFlags::NONBLOCK.bits().cast_signed();
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path);
    let mut file = opened.map_err(|err| refuse(format!("{}: {err}", path.display())))?;
    let meta = file.metadata().map_err(StartupError::because(flag))?;
    let mode = meta.permissions().mode() & 0o777;
    let euid = geteuid().as_raw();
    if !trusted_private_file(meta.is_file(), mode, meta.uid(), euid) {
        return Err(StartupError::new(
            flag,
            format!(
                "{} must be a regular file owned by the user running the server and readable only by that user (mode 0600 or 0400), not mode {mode:04o} owned by uid {}",
                path.display(),
                meta.uid()
            ),
        ));
    }
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes);
    read.map_err(StartupError::because(flag))?;
    Ok(bytes)
}

/// Reads the token from the file `--token-file` names, see [`read_private`].
fn read_token(path: &Path) -> Result<Token, StartupError> {
    let bytes = read_private(path, "--token-file")?;
    parse_token(&bytes)
        .map_err(|reason| StartupError::new("--token-file", format!("{} {reason}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a token compares equal only to itself and prints without its bytes.
    #[test]
    fn a_token_matches_itself_and_hides_itself() {
        let token = Token(b"s3cret".to_vec());
        assert!(token.matches(b"s3cret"));
        assert!(!token.matches(b"s3creT"));
        assert!(!token.matches(b"s3cret!"));
        assert!(!token.matches(b""));
        assert_eq!(format!("{token:?}"), "Token(..)");
    }

    /// Asserts that a token file is trusted only when it is a regular file, owner-only
    /// in mode, and owned by the user running the server: each condition alone refuses.
    #[test]
    fn a_token_file_must_be_the_servers_own_private_file() {
        assert!(trusted_private_file(true, 0o600, 1000, 1000));
        assert!(trusted_private_file(true, 0o400, 1000, 1000));
        assert!(!trusted_private_file(false, 0o600, 1000, 1000));
        assert!(!trusted_private_file(true, 0o640, 1000, 1000));
        assert!(!trusted_private_file(true, 0o600, 0, 1000));
        assert!(!trusted_private_file(true, 0o600, 1000, 0));
    }

    /// Asserts that a token is the content without trailing whitespace, and that empty
    /// content and any space, line break, control or non-ASCII byte inside the token
    /// is refused: no header value could ever equal such a token.
    #[test]
    fn a_token_is_one_run_of_printable_ascii() {
        let good = "abcdefghijklmnopqrstuvwxyz-0123456789";
        let trailing = format!("{good}\r\n");
        assert!(
            parse_token(trailing.as_bytes())
                .unwrap()
                .matches(good.as_bytes())
        );
        assert_eq!(parse_token(b" \r\n").err().as_deref(), Some("is empty"));
        for bad in [
            &b"two\nlines\n"[..],
            b"has space",
            b" leading",
            b"tab\tinside",
            b"nul\0byte",
            "caf\u{e9}".as_bytes(),
        ] {
            let reason = parse_token(bad).err().unwrap();
            assert!(reason.contains("printable ASCII"), "{bad:?}");
        }
    }

    /// Asserts that a token of 31 bytes is refused and one of 32 is accepted, counting
    /// the bytes without the trailing whitespace, and that the refusal says how long
    /// the token was and how long it must be.
    #[test]
    fn a_token_must_have_at_least_32_bytes() {
        let short = "x".repeat(MIN_TOKEN_BYTES - 1);
        let reason = parse_token(format!("{short}\n\n").as_bytes())
            .err()
            .unwrap();
        assert!(reason.contains("31 bytes"), "{reason}");
        assert!(reason.contains("at least 32"), "{reason}");
        let enough = "x".repeat(MIN_TOKEN_BYTES);
        assert!(parse_token(enough.as_bytes()).is_ok());
    }

    /// Asserts that the default limits are what the flags default to, so the two
    /// cannot drift apart.
    #[test]
    fn the_default_limits_are_the_flag_defaults() {
        let cli = Cli::try_parse_from([
            "js-lease-server",
            "--state-file=a",
            "--boot=resume",
            "--pool-config=b",
            "--token-file=c",
        ])
        .unwrap();
        assert_eq!(Limits::from(&cli), Limits::default());
    }
}
