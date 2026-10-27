//! The `--server` URL and the rule that a token crosses a network only inside TLS.

use std::net::IpAddr;

use ureq::http::Uri;

use crate::failure::Failure;

/// A checked server address: the scheme and authority to put in front of a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerUrl {
    /// `http://host:port` or `https://host:port`, with no trailing slash.
    pub(crate) base: String,
    /// Whether the scheme is `https`.
    pub(crate) https: bool,
}

impl ServerUrl {
    /// Checks `text`: an `https://` URL, or an `http://` one for a loopback host
    /// (`localhost`, `127.0.0.0/8`, `::1`) only, naming a host and optionally a port,
    /// with no user, path or query.
    pub(crate) fn parse(text: &str) -> Result<Self, Failure> {
        let refuse = |why: &str| Failure::flag("--server", format!("{text:?} {why}"));
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
