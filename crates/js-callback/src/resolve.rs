//! How the sender learns a callback host's addresses (`docs/design/job-retry-and-completion-
//! callback.md`, section 5.3): it asks a [`Resolve`] once per try, and judges every address the
//! answer holds before it connects to any. The server passes [`SystemResolver`]; a test passes
//! a fake, so no test looks a name up.

use std::fmt;
use std::io;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// What turns a callback host's name into its addresses.
pub trait Resolve: fmt::Debug {
    /// The addresses `host` names, looked up within `within`. `host` is a name: the sender reads
    /// an address literal itself.
    ///
    /// # Errors
    /// [`ResolveError`] when the lookup fails or does not answer within `within`.
    fn resolve(&self, host: &str, within: Duration) -> Result<Vec<IpAddr>, ResolveError>;
}

/// Why a name gave no addresses.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The lookup answered with an error.
    #[error("the lookup failed: {0}")]
    Lookup(#[source] io::Error),
    /// The lookup did not answer within its limit.
    #[error("the lookup did not answer within its limit")]
    TimedOut,
}

/// The system's resolver (`getaddrinfo`, through the standard library), each lookup on a
/// thread of its own so that it is bounded by the try's limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, host: &str, within: Duration) -> Result<Vec<IpAddr>, ResolveError> {
        let host = host.to_owned();
        bounded(within, move || {
            let found = (host.as_str(), 0).to_socket_addrs()?;
            Ok(found.map(|address| address.ip()).collect())
        })
    }
}

/// What `lookup` answers, run on a thread of its own, or [`ResolveError::TimedOut`] when it
/// has not answered within `within`. A lookup that never answers keeps its thread.
fn bounded(
    within: Duration,
    lookup: impl FnOnce() -> io::Result<Vec<IpAddr>> + Send + 'static,
) -> Result<Vec<IpAddr>, ResolveError> {
    let (answer, answered) = mpsc::sync_channel(1);
    thread::spawn(move || answer.send(lookup()));
    match answered.recv_timeout(within) {
        Ok(found) => found.map_err(ResolveError::Lookup),
        Err(_) => Err(ResolveError::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the system resolver answers an IPv4 and an IPv6 literal with that address,
    /// which the standard library reads without a lookup, and passes on as `Lookup` the error
    /// of a name the standard library refuses (one holding a NUL byte) before any lookup.
    /// Catches an answer that drops or reorders what the lookup gave, and a lookup error read
    /// as an empty answer.
    #[test]
    fn the_system_resolver_gives_what_the_lookup_gives() {
        let within = Duration::from_secs(60);
        let resolver = SystemResolver;
        let v4 = resolver.resolve("127.0.0.1", within).expect("a literal");
        let v6 = resolver.resolve("::1", within).expect("a literal");
        assert_eq!(
            (v4, v6),
            (
                vec!["127.0.0.1".parse().expect("an address")],
                vec!["::1".parse().expect("an address")]
            )
        );
        // A NUL byte: the standard library refuses the name before any lookup.
        let refused = resolver.resolve("a\0b", within).expect_err("a NUL byte");
        let text = refused.to_string();
        assert!(text.starts_with("the lookup failed: "), "{text}");
    }

    /// Asserts that a lookup that never answers (it waits on a channel nothing ever sends on)
    /// ends as `TimedOut` with its whole text, and that a lookup's error is passed on with
    /// its whole text. Catches a try that waits on a hung name server past its limit, and a
    /// failed lookup read as an empty answer.
    #[test]
    fn a_lookup_ends_at_its_limit_or_with_its_error() {
        let (keep, never) = mpsc::channel::<Vec<IpAddr>>();
        let hung = bounded(Duration::ZERO, move || Ok(never.recv().unwrap_or_default()));
        let failed = bounded(Duration::from_secs(60), || {
            Err(io::Error::new(io::ErrorKind::NotFound, "no such name"))
        });
        assert_eq!(
            [hung, failed].map(|result| result.expect_err("no answer").to_string()),
            [
                "the lookup did not answer within its limit",
                "the lookup failed: no such name"
            ]
        );
        drop(keep);
    }
}
