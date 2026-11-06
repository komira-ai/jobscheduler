//! The rule every listen address is held to, and the address of the job service.
//!
//! A bearer token never crosses a network in clear: an address off the loopback is served
//! only with TLS. [`clear_text`] is that rule, once; the flags (`--listen`, `--job-listen`)
//! and the configuration file's keys (`server.listen`, `server.agent_listen`,
//! `server.job_listen`) each ask it, and differ only in how they name the two TLS settings.

use std::net::SocketAddr;
use std::path::Path;

use super::{Cli, TlsFiles};
use crate::error::StartupError;
use crate::job_store::JobStoreSetup;

/// How the flags name the two TLS settings in a refusal.
const FLAG_PAIR: &str = "--tls-cert and --tls-key";

/// The address the job service is served on (`--job-listen`, `server.job_listen`), and the
/// flag or key that names it in a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobListen {
    /// The address to bind.
    pub(crate) addr: SocketAddr,
    /// `--job-listen`, or `server.job_listen` when the configuration file gave it.
    pub(crate) flag: &'static str,
}

/// Why `addr` may not be served without TLS, or `None` when it may: it is a loopback
/// address, or `tls` says the server has a certificate and key. `pair` names the two
/// settings that give them, as the refusal says it.
pub(crate) fn clear_text(addr: SocketAddr, tls: bool, pair: &str) -> Option<String> {
    let clear = !tls & !addr.ip().is_loopback();
    clear.then(|| {
        format!(
            "{addr} is not a loopback address, so {pair} are required: a bearer token never crosses a network in clear"
        )
    })
}

/// Where `--job-listen` serves the job service, named by the flag; `None` when it is off.
pub(super) fn job_listen(cli: &Cli) -> Option<JobListen> {
    let flag = "--job-listen";
    cli.job_listen.map(|addr| JobListen { addr, flag })
}

/// The job store's setup when `--job-listen` turns the job service on: named by the flag,
/// keeping an ended job for `--job-retention`, the newest `--job-feed-retention` changes of
/// its feed, and a freed allocation for `--job-allocation-retention`.
pub(super) fn job_store(cli: &Cli) -> Option<JobStoreSetup> {
    let flag = "--job-listen";
    let (retention, feed_retention) = (cli.job_retention, cli.job_feed_retention);
    let allocation_retention = cli.job_allocation_retention;
    cli.job_listen.map(|_| JobStoreSetup {
        flag,
        retention,
        feed_retention,
        allocation_retention,
    })
}

/// The TLS files of `--tls-cert` and `--tls-key`, both or neither, and the refusal of the
/// first listen address (`--listen`, then `--job-listen` when given) that is off the
/// loopback without them.
pub(super) fn tls_files(cli: &Cli) -> Result<Option<TlsFiles>, StartupError> {
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
    let listeners = [
        ("--listen", Some(cli.listen)),
        ("--job-listen", cli.job_listen),
    ];
    let refused = listeners.into_iter().find_map(|(flag, addr)| {
        let reason = clear_text(addr?, files.is_some(), FLAG_PAIR)?;
        Some(StartupError::new(flag, reason))
    });
    match refused {
        Some(refusal) => Err(refusal),
        None => Ok(files),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the rule on its own, with the whole refusal: an address off the loopback
    /// without TLS is refused naming the pair it was given, and the same address with TLS,
    /// or a loopback address (v4 or v6) without it, is not. It catches a rule that looks at
    /// only one of its two inputs, or that says another pair than the caller's.
    #[test]
    fn an_address_off_the_loopback_needs_tls_and_a_loopback_one_does_not() {
        let wide: SocketAddr = "192.0.2.7:7302".parse().unwrap();
        assert_eq!(
            clear_text(wide, false, "the pair"),
            Some(
                "192.0.2.7:7302 is not a loopback address, so the pair are required: a bearer token never crosses a network in clear"
                    .to_owned()
            )
        );
        assert_eq!(clear_text(wide, true, "the pair"), None);
        for loopback in ["127.0.0.1:7302", "[::1]:7302"] {
            let addr: SocketAddr = loopback.parse().unwrap();
            assert_eq!(clear_text(addr, false, "the pair"), None, "{loopback}");
        }
    }
}
