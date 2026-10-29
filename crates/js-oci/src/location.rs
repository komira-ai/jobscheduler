//! Where an artifact lives: the registry that serves its repository.

use std::fmt;
use std::net::Ipv6Addr;

use crate::Error;

/// The port a registry is reached at when its name gives none.
pub(crate) const HTTPS_PORT: u16 = 443;

/// A host and port, compared as the connection sees them: the host in lower case and
/// without the brackets of an IPv6 literal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Authority {
    host: String,
    port: u16,
}

impl Authority {
    /// The authority `text` spells: `host`, `host:port`, `[v6]` or `[v6]:port`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (host, port) = match text.strip_prefix('[') {
            Some(rest) => {
                let (host, after) = rest.split_once(']')?;
                host.parse::<Ipv6Addr>().ok()?;
                let port = if after.is_empty() {
                    HTTPS_PORT
                } else {
                    after.strip_prefix(':')?.parse().ok()?
                };
                (host, port)
            }
            None => match text.split_once(':') {
                Some((host, port)) => (host, port.parse().ok()?),
                None => (text, HTTPS_PORT),
            },
        };
        let named = !host.is_empty()
            && host
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'.' | b':'));
        named.then(|| Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// The authority of `uri`, the https default port filled in.
    pub(crate) fn of_uri(uri: &hyper::Uri) -> Option<Self> {
        let host = uri.host()?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        Some(Self {
            host: host.to_ascii_lowercase(),
            port: uri.port_u16().unwrap_or(HTTPS_PORT),
        })
    }

    /// The host, lower case, an IPv6 literal without brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The value of a `Host` header for this authority.
    pub(crate) fn header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == HTTPS_PORT {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Whether `component` is a repository path component: lower-case letters and digits,
/// with `.`, `_` and `-` between them (the distribution spec's grammar, which also keeps
/// `..` and an empty component out of a URL).
pub(crate) fn component_ok(component: &str) -> bool {
    let edge = |b: &u8| matches!(b, b'a'..=b'z' | b'0'..=b'9');
    let bytes = component.as_bytes();
    let ok = [
        bytes.first().is_some_and(edge),
        bytes.last().is_some_and(edge),
        bytes
            .iter()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')),
    ];
    ok.into_iter().all(|ok| ok)
}

/// Whether `path` is `/`-separated repository path components.
pub(crate) fn path_ok(path: &str) -> bool {
    path.split('/').all(component_ok)
}

/// The registry address and repository path of an artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    registry: Authority,
    endpoint: Authority,
    repository: String,
}

/// Docker Hub's name in a reference and the host that serves its API.
const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";

impl Location {
    /// The location `name` spells: `<registry>/<path>`. A first path component with a
    /// dot or a port, or `localhost`, is the registry; without one the registry is
    /// Docker Hub, where a path of one component is under `library/`.
    ///
    /// # Errors
    /// [`Error::Reference`] when the registry or the path is not well formed.
    pub fn parse(name: &str) -> Result<Self, Error> {
        let bad = || Error::Reference(name.to_owned());
        let (first, rest) = name.split_once('/').unwrap_or((name, ""));
        let names_registry =
            !rest.is_empty() && (first.contains(['.', ':']) || first == "localhost");
        let (registry, path) = if names_registry {
            (first, rest)
        } else {
            (DOCKER_HUB, name)
        };
        let registry = Authority::parse(registry).ok_or_else(bad)?;
        if !path_ok(path) {
            return Err(bad());
        }
        let hub = matches!(registry.host(), DOCKER_HUB | "index.docker.io");
        let repository = if hub && !path.contains('/') {
            format!("library/{path}")
        } else {
            path.to_owned()
        };
        let endpoint = if hub {
            Authority {
                host: DOCKER_HUB_API.to_owned(),
                port: registry.port(),
            }
        } else {
            registry.clone()
        };
        Ok(Self {
            registry,
            endpoint,
            repository,
        })
    }

    /// The registry as the reference names it (`docker.io`, `ghcr.io`, `host:5000`).
    #[must_use]
    pub fn registry(&self) -> &Authority {
        &self.registry
    }

    /// The host and port requests go to: the registry, except Docker Hub's.
    #[must_use]
    pub fn endpoint(&self) -> &Authority {
        &self.endpoint
    }

    /// The repository path on the registry (`library/busybox`).
    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(registry: &str, endpoint: &str, repository: &str) -> (String, String, String) {
        (
            registry.to_owned(),
            endpoint.to_owned(),
            repository.to_owned(),
        )
    }

    fn parts(name: &str) -> (String, String, String) {
        let loc = Location::parse(name).expect("location");
        (
            loc.registry().to_string(),
            loc.endpoint().to_string(),
            loc.repository().to_owned(),
        )
    }

    /// Catches Docker Hub names going to the wrong host or path: a one-component name
    /// is under `library/`, the API host is not the name, and a named registry is taken
    /// as written, with its port.
    #[test]
    fn registries_and_repositories_are_told_apart() {
        let hub = at(
            "docker.io:443",
            "registry-1.docker.io:443",
            "library/busybox",
        );
        assert_eq!(parts("docker.io/library/busybox"), hub);
        assert_eq!(parts("docker.io/busybox"), hub);
        assert_eq!(parts("busybox"), hub);
        assert_eq!(parts("index.docker.io/busybox").1, hub.1);
        assert_eq!(
            parts("someone/tool"),
            at("docker.io:443", "registry-1.docker.io:443", "someone/tool")
        );
        assert_eq!(
            parts("ghcr.io/komira-ai/bundle"),
            at("ghcr.io:443", "ghcr.io:443", "komira-ai/bundle")
        );
        assert_eq!(
            parts("localhost/x"),
            at("localhost:443", "localhost:443", "x")
        );
        assert_eq!(
            parts("127.0.0.1:5000/a/b"),
            at("127.0.0.1:5000", "127.0.0.1:5000", "a/b")
        );
        assert_eq!(parts("[::1]:5000/a"), at("[::1]:5000", "[::1]:5000", "a"));
        assert_eq!(
            parts("Registry.Example/a").0,
            "registry.example:443",
            "host names are compared in lower case"
        );
    }

    /// Catches a name that could reach a URL as something else: an empty component,
    /// `..`, an upper-case or odd character in the path, and a malformed registry.
    #[test]
    fn a_name_that_is_not_a_registry_path_is_refused() {
        for bad in [
            "gcr.io/",
            "docker.io//x",
            "a//b",
            "gcr.io/a/../b",
            "gcr.io/A/b",
            "gcr.io/a b",
            "gcr.io/-a",
            "gcr.io/a-",
            "gcr.io:notaport/a",
            "gcr.io:1:2/a",
            "[::1/a",
            "[nonsense]:5000/a",
            "[::1]x/a",
            "gcr.io:99999/a",
            ":5000/a",
            "my_host.io/a",
            "",
        ] {
            assert!(
                matches!(Location::parse(bad), Err(Error::Reference(_))),
                "{bad:?}: {:?}",
                Location::parse(bad)
            );
        }
    }

    /// Catches a `Host` header with the default port spelt out, or an IPv6 literal
    /// without its brackets.
    #[test]
    fn host_headers_leave_out_the_https_port_and_bracket_ipv6() {
        let header = |name: &str| Location::parse(name).expect("location").endpoint().header();
        assert_eq!(header("ghcr.io/a"), "ghcr.io");
        assert_eq!(header("ghcr.io:8443/a"), "ghcr.io:8443");
        assert_eq!(header("[::1]/a"), "[::1]");
        assert_eq!(header("[::1]:5000/a"), "[::1]:5000");
    }

    /// Catches an authority read from a URL with its brackets or its case kept, or the
    /// default port missing.
    #[test]
    fn the_authority_of_a_uri_is_normalised() {
        let of = |url: &str| Authority::of_uri(&url.parse().expect("uri")).expect("authority");
        assert_eq!(of("https://Example.COM/x").to_string(), "example.com:443");
        assert_eq!(of("https://[fd00::1]:8443/x").to_string(), "[fd00::1]:8443");
        assert!(Authority::of_uri(&"/just/a/path".parse().expect("uri")).is_none());
    }
}
