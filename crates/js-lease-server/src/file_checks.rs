//! The checks of a loaded configuration file that look across its tables, and the two
//! small rules a pinned download must meet: what the listeners and the advertised
//! address must be when a pool launches hosts, what a pool that spends money must
//! have, an `https://` address and a SHA-256.

use std::net::IpAddr;

use hyper::Uri;

use crate::error::StartupError;
use crate::file::{ElasticPlan, Launch, Server};

/// What the listeners and the advertised address must be, given whether a pool launches
/// hosts; the warnings the server shows.
pub(crate) fn check_listeners(server: &Server, elastic: bool) -> Result<Vec<String>, StartupError> {
    if elastic {
        let apart = match server.agent_listen {
            Some(agent) => agent != server.listen,
            None => false,
        };
        if !apart {
            return Err(StartupError::at(
                "server.agent_listen",
                "required with an elastic pool; must differ from server.listen",
            ));
        }
        let refused = match &server.advertise_url {
            Some(url) => plain_http(url),
            None => false,
        };
        if refused {
            return Err(StartupError::at(
                "server.advertise_url",
                "http:// is refused by agents; use https (or loopback)",
            ));
        }
    }
    let mut listeners = vec![("server.listen", server.listen)];
    if let Some(agent) = server.agent_listen {
        listeners.push(("server.agent_listen", agent));
    }
    let mut warnings = Vec::new();
    for (key, addr) in listeners {
        if server.tls.is_none() & !addr.ip().is_loopback() {
            return Err(StartupError::at(
                key,
                format!(
                    "{addr} is not a loopback address, so server.tls_cert and server.tls_key are required: a bearer token never crosses a network in clear"
                ),
            ));
        }
        if addr.ip().is_unspecified() {
            warnings.push(format!(
                "server: warning: binds all interfaces ({key} {addr})"
            ));
        }
    }
    Ok(warnings)
}

/// Whether `url` is `http://` to a host that is not loopback, which an agent refuses.
fn plain_http(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let host = uri.host().unwrap_or_default();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let address = match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    };
    let loopback = (host == "localhost") | address;
    let plain = uri.scheme_str() == Some("http");
    plain & !loopback
}

/// Refuses a pool that launches when the hosts it launches have nothing to dial: the
/// address they are told to dial is `server.advertise_url`.
pub(crate) fn check_live(server: &Server, plan: &ElasticPlan) -> Result<(), StartupError> {
    let silent = [plan.launch == Launch::Live, server.advertise_url.is_none()];
    match silent.iter().all(|set| *set) {
        true => Err(StartupError::at(
            "server.advertise_url",
            "required with a pool whose launch is live: the hosts it launches dial it",
        )),
        false => Ok(()),
    }
}

/// Whether `url` is an `https://` address with a host and nothing a script line could not
/// carry: the bootstrap downloads over https only.
pub(crate) fn https_url(url: &str) -> bool {
    let host = url.strip_prefix("https://").unwrap_or_default();
    let plain = !url.chars().any(|c| c.is_control() | c.is_whitespace());
    !host.is_empty() & plain
}

/// The 32 bytes of `text`, which must be 64 lower-case hexadecimal digits.
pub(crate) fn sha256(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *slot = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}
