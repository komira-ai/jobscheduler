//! Network probes: one TCP connect to each address an argument names, and one name lookup per
//! named host. Only connects and lookups: no scan, nothing sent.

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::time::Duration;

use crate::host::Named;
use crate::outcome::{Line, Outcome};

/// `connect:<name>`: one TCP connect to `HOST:PORT` within `limit`. A connection, or a refusal
/// by the peer (it answered), is [`Outcome::Allowed`]; a refusal by this host (the sandbox's
/// `EPERM`), no answer within `limit`, or no route is [`Outcome::Denied`], with the error
/// number. A host name is looked up first, and a failed lookup is a denial too.
#[must_use]
pub fn connect(named: &Named, limit: Duration) -> Line {
    let name = format!("connect:{}", named.name);
    let addr = match resolve(&named.value) {
        Ok(addr) => addr,
        Err(err) => {
            return Line::new(name, Outcome::Denied)
                .errno_of(&err)
                .detail(format!("{}: cannot resolve: {err}", named.value));
        }
    };
    reached(
        name,
        &addr,
        TcpStream::connect_timeout(&addr, limit).map(drop),
    )
}

fn resolve(value: &str) -> io::Result<SocketAddr> {
    value
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::new(io::ErrorKind::NotFound, "no address"))
}

/// What a connect's result says.
fn reached(name: String, addr: &SocketAddr, result: io::Result<()>) -> Line {
    match result {
        Ok(()) => Line::new(name, Outcome::Allowed).detail(format!("{addr}: connected")),
        Err(err) if err.raw_os_error() == Some(libc::ECONNREFUSED) => {
            Line::new(name, Outcome::Allowed)
                .errno_of(&err)
                .detail(format!("{addr}: the peer refused, so it was reached"))
        }
        Err(err) => Line::new(name, Outcome::Denied)
            .errno_of(&err)
            .detail(format!("{addr}: {}", short(&err))),
    }
}

/// An error's text without the `(os error N)` the errno already says.
fn short(err: &io::Error) -> String {
    let text = err.to_string();
    match text.find(" (os error") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// `lookup:<name>`: one lookup of the host name with the system resolver.
#[must_use]
pub fn lookup(named: &Named) -> Line {
    lookup_with(named, &|host| {
        (host, 0).to_socket_addrs().map(Iterator::count)
    })
}

/// [`lookup`] with the resolver given: it answers how many addresses the name has.
pub fn lookup_with(named: &Named, resolver: &dyn Fn(&str) -> io::Result<usize>) -> Line {
    let name = format!("lookup:{}", named.name);
    match resolver(&named.value) {
        Ok(n) => {
            Line::new(name, Outcome::Allowed).detail(format!("{}: {n} addresses", named.value))
        }
        Err(err) => Line::new(name, Outcome::Denied)
            .errno_of(&err)
            .detail(format!("{}: {}", named.value, short(&err))),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn named(name: &str, value: &str) -> Named {
        Named {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn a_listening_loopback_port_is_reached() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let line = connect(&named("loopback", &addr), Duration::from_secs(30));
        assert_eq!(
            line.to_string(),
            format!("connect:loopback: allowed; {addr}: connected")
        );
    }

    #[test]
    fn a_closed_port_is_reached_because_the_peer_refused() {
        let addr = js_testkit::port::dead_addr();
        let line = connect(
            &named("loopback", &addr.to_string()),
            Duration::from_secs(30),
        );
        assert_eq!(line.outcome, Outcome::Allowed, "{line}");
        assert_eq!(line.errno, Some(libc::ECONNREFUSED));
    }

    #[test]
    fn a_refusal_by_this_host_or_no_answer_is_denied() {
        let addr: SocketAddr = "192.0.2.1:443".parse().unwrap();
        for errno in [libc::EPERM, libc::ETIMEDOUT, libc::EHOSTUNREACH] {
            let line = reached(
                "connect:x".to_owned(),
                &addr,
                Err(io::Error::from_raw_os_error(errno)),
            );
            assert_eq!(line.outcome, Outcome::Denied);
            assert_eq!(line.errno, Some(errno));
            assert!(!line.detail.contains("os error"), "{line}");
        }
        let line = reached(
            "connect:x".to_owned(),
            &addr,
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            )),
        );
        assert_eq!(
            line.to_string(),
            "connect:x: denied; 192.0.2.1:443: connection timed out"
        );
    }

    #[test]
    fn an_address_that_does_not_parse_is_denied_with_why() {
        let line = connect(&named("bad", "not an address"), Duration::from_secs(1));
        assert_eq!(line.outcome, Outcome::Denied);
        assert!(
            line.detail.starts_with("not an address: cannot resolve: "),
            "{line}"
        );
        assert_eq!(
            resolve("[::1]:1").map(|a| a.port()).ok(),
            Some(1),
            "a literal address needs no lookup"
        );
    }

    #[test]
    fn a_lookup_counts_addresses_or_says_why_it_failed() {
        let line = lookup(&named("literal", "127.0.0.1"));
        assert_eq!(
            line.to_string(),
            "lookup:literal: allowed; 127.0.0.1: 1 addresses"
        );
        let line = lookup_with(&named("public-name", "example.org"), &|_| {
            Err(io::Error::other("failed to lookup address information"))
        });
        assert_eq!(
            line.to_string(),
            "lookup:public-name: denied; example.org: failed to lookup address information"
        );
    }
}
