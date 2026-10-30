//! `probe`: one TCP connect and what became of it. It is the check an operator runs as a
//! pool user (`sudo -u <pool user> js-mac-provision probe 192.0.2.10:443`), and what the
//! root test runs as a pool uid and as a control.

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
        drop(l);
        assert_eq!(probe(addr, Duration::from_secs(5)), Probe::Refused);
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
