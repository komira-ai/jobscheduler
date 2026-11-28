//! `probe`: one TCP connect and what became of it. It is what the root test runs as a pool uid
//! and as a control, through the `pf_probe` example (`pf_probe 192.0.2.10:443`), whose command
//! line [`parse_args`] reads.

use std::fmt;
use std::io::ErrorKind;
use std::net::{SocketAddrV4, TcpStream};
use std::time::Duration;

/// How a connect ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The connection was made.
    Connected,
    /// The peer, or a `block return` rule on the way, refused it at once.
    Refused,
    /// Nothing answered within the time: a drop, or no host.
    TimedOut,
    /// Any other failure.
    Failed(ErrorKind),
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Probe::Connected => f.write_str("connected"),
            Probe::Refused => f.write_str("refused"),
            Probe::TimedOut => f.write_str("timed out"),
            Probe::Failed(kind) => write!(f, "failed: {kind}"),
        }
    }
}

/// Connects to `addr`, waiting at most `timeout`.
#[must_use]
pub fn probe(addr: SocketAddrV4, timeout: Duration) -> Probe {
    match TcpStream::connect_timeout(&addr.into(), timeout) {
        Ok(_) => Probe::Connected,
        Err(e) => classify(e.kind()),
    }
}

/// The connect bound, in milliseconds, when the command line gives none.
pub const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// The address and the connect bound the command line `args` (after the program name) of the
/// `pf_probe` example name: `<ipv4:port>` and then, optionally, `--timeout-ms <n>`. `None` for
/// anything else.
#[must_use]
pub fn parse_args(args: &[String]) -> Option<(SocketAddrV4, Duration)> {
    let (addr, rest) = args.split_first()?;
    let addr = addr.parse().ok()?;
    let timeout = match rest {
        [] => DEFAULT_TIMEOUT_MS,
        [flag, ms] if flag == "--timeout-ms" => ms.parse().ok()?,
        _ => return None,
    };
    Some((addr, Duration::from_millis(timeout)))
}

/// The outcome for a failed connect of kind `kind`.
fn classify(kind: ErrorKind) -> Probe {
    match kind {
        ErrorKind::ConnectionRefused => Probe::Refused,
        ErrorKind::TimedOut | ErrorKind::WouldBlock => Probe::TimedOut,
        other => Probe::Failed(other),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn v4(l: &TcpListener) -> SocketAddrV4 {
        l.local_addr().unwrap().to_string().parse().unwrap()
    }

    #[test]
    fn a_listener_is_connected_and_a_closed_port_is_refused() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = v4(&l);
        assert_eq!(probe(addr, Duration::from_secs(5)), Probe::Connected);
        // Not `l` dropped: that frees the number for any other test to bind.
        let dead = js_testkit::port::dead_addr().to_string().parse().unwrap();
        assert_eq!(probe(dead, Duration::from_secs(5)), Probe::Refused);
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn the_command_line_is_an_address_and_an_optional_bound() {
        let addr: SocketAddrV4 = "192.0.2.10:443".parse().unwrap();
        assert_eq!(
            parse_args(&words(&["192.0.2.10:443"])),
            Some((addr, Duration::from_millis(DEFAULT_TIMEOUT_MS)))
        );
        assert_eq!(
            parse_args(&words(&["192.0.2.10:443", "--timeout-ms", "250"])),
            Some((addr, Duration::from_millis(250)))
        );
    }

    #[test]
    fn any_other_command_line_is_refused() {
        for args in [
            &[][..],
            &["not-an-address"][..],
            &["192.0.2.10:443", "--timeout-ms"][..],
            &["192.0.2.10:443", "--timeout-ms", "soon"][..],
            &["192.0.2.10:443", "--bound", "250"][..],
            &["192.0.2.10:443", "--timeout-ms", "250", "extra"][..],
        ] {
            assert_eq!(parse_args(&words(args)), None, "{args:?}");
        }
    }

    #[test]
    fn kinds_map_to_outcomes() {
        assert_eq!(classify(ErrorKind::ConnectionRefused), Probe::Refused);
        assert_eq!(classify(ErrorKind::TimedOut), Probe::TimedOut);
        assert_eq!(classify(ErrorKind::WouldBlock), Probe::TimedOut);
        assert_eq!(
            classify(ErrorKind::HostUnreachable),
            Probe::Failed(ErrorKind::HostUnreachable)
        );
    }

    #[test]
    fn a_zero_timeout_is_a_failure_not_a_hang() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        assert_eq!(
            probe(v4(&l), Duration::ZERO),
            Probe::Failed(ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn the_outcomes_print_as_one_word_or_two() {
        assert_eq!(Probe::Connected.to_string(), "connected");
        assert_eq!(Probe::Refused.to_string(), "refused");
        assert_eq!(Probe::TimedOut.to_string(), "timed out");
        assert!(
            Probe::Failed(ErrorKind::HostUnreachable)
                .to_string()
                .starts_with("failed: ")
        );
    }
}
