//! The `--server` URL and the rule that a token crosses a network only inside TLS.

use std::net::IpAddr;

use ureq::http::Uri;

use crate::error::ClientError;

/// A checked server address: the scheme and authority to put in front of a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    /// `http://host:port` or `https://host:port`, with no trailing slash.
    pub base: String,
    /// Whether the scheme is `https`.
    pub https: bool,
    /// The host, without the brackets of an IPv6 literal: the name the addresses of the
    /// server are looked up by.
    pub(crate) host: String,
    /// The port: the one named, or the scheme's (443 for `https`, 80 for `http`).
    pub(crate) port: u16,
}

impl ServerUrl {
    /// Checks `text`: an `https://` URL, or an `http://` one for a loopback host
    /// (`localhost`, `127.0.0.0/8`, `::1`) only, naming a host and optionally a port,
    /// with no user, path or query.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming `--server` when `text` breaks one of those rules.
    pub fn parse(text: &str) -> Result<Self, ClientError> {
        let refuse = |why: &str| ClientError::flag("--server", format!("{text:?} {why}"));
        let uri = text
            .parse::<Uri>()
            .map_err(|err| refuse(&format!("is not a URL: {err}")))?;
        let scheme = uri.scheme_str().unwrap_or_default();
        let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
        let authority = uri.authority().map_or("", |a| a.as_str());
        let bare = matches!(uri.path(), "" | "/") & uri.query().is_none();
        let https = scheme == "https";
        match (
            scheme,
            https | is_loopback(host),
            authority.contains('@'),
            bare,
        ) {
            ("https" | "http", true, false, true) => Ok(Self {
                base: format!("{scheme}://{authority}"),
                https,
                host: host.to_owned(),
                port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
            }),
            ("https" | "http", false, _, _) => Err(refuse(
                "is plain http to a host off the loopback: a bearer token never crosses a network in clear, use https://",
            )),
            ("https" | "http", true, true, _) => Err(refuse("must not carry a user")),
            ("https" | "http", true, false, false) => {
                Err(refuse("must be scheme://host:port with no path or query"))
            }
            _ => Err(refuse(
                "must start with https:// (or http:// for a loopback host)",
            )),
        }
    }
}

/// Whether `host` names the machine itself.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") | host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the host and port a server's addresses are looked up and kept by: the port
    /// named, or the scheme's own, and an IPv6 literal without its brackets. It catches a
    /// book kept under the scheme's port for a server on another, or one asked for `[::1]`.
    #[test]
    fn the_host_and_port_are_the_named_or_the_schemes() {
        for (text, host, port) in [
            ("https://js.example.test", "js.example.test", 443),
            ("https://js.example.test:7301", "js.example.test", 7301),
            ("http://localhost", "localhost", 80),
            ("http://[::1]:9", "::1", 9),
        ] {
            let url = ServerUrl::parse(text).unwrap();
            assert_eq!((url.host.as_str(), url.port), (host, port), "{text}");
        }
    }
}
