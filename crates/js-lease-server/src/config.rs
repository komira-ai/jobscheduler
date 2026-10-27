//! The flags, and the checks that decide whether the server may start.

use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use js_lease::{Boot, Settings};
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
        Ok(Self {
            listen: cli.listen,
            state_file: cli.state_file,
            boot: cli.boot.into(),
            settings,
            tick: cli.tick,
            token,
            tls,
            capacity: Box::new(StaticPoolSource::new(cli.pool_config)),
        })
    }

    /// The same configuration taking its hosts from `capacity` instead of the file
    /// `--pool-config` names.
    pub fn with_capacity_source(mut self, capacity: Box<dyn CapacitySource>) -> Self {
        self.capacity = capacity;
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

/// Reads the token from `path` after checking, on the open file, that it is a regular
/// file only its owner can use. The token is the file's content without trailing
/// whitespace and is not empty.
fn read_token(path: &Path) -> Result<Token, StartupError> {
    let refuse = StartupError::because("--token-file");
    let mut file = File::open(path).map_err(|err| refuse(format!("{}: {err}", path.display())))?;
    let meta = file
        .metadata()
        .map_err(StartupError::because("--token-file"))?;
    let mode = meta.permissions().mode() & 0o777;
    let private = meta.is_file() & matches!(mode, 0o600 | 0o400);
    if !private {
        return Err(StartupError::new(
            "--token-file",
            format!(
                "{} must be a regular file readable only by its owner (mode 0600 or 0400), not mode {mode:04o}",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes);
    read.map_err(StartupError::because("--token-file"))?;
    let token = bytes.trim_ascii_end();
    match token.is_empty() {
        true => Err(StartupError::new(
            "--token-file",
            format!("{} is empty", path.display()),
        )),
        false => Ok(Token(token.to_vec())),
    }
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
}
