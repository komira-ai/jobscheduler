//! The parameters of a store URL that libpq knows and the client does not: `sslmode` with the
//! values the client cannot read (`verify-ca`, `verify-full`) and `sslrootcert`. They are
//! taken out of the query before the client reads the rest.

use std::path::PathBuf;

use super::ConfigError;
use super::tls::TlsMode;

/// A URL with its TLS parameters taken out.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Query {
    /// The URL without them, for the client.
    pub(super) url: String,
    /// `sslmode`, [`TlsMode::Disable`] when the URL names none.
    pub(super) mode: TlsMode,
    /// `sslrootcert`, percent-decoded.
    pub(super) root: Option<PathBuf>,
    /// Whether the URL names `keepalives_idle` (the client has a default of its own in place
    /// of no value, so it cannot tell).
    pub(super) idle_set: bool,
}

/// Takes the TLS parameters out of `url`'s query. A parameter named twice takes its last
/// value, as the client does.
///
/// # Errors
/// [`ConfigError::Invalid`] for an `sslmode` value libpq does not have, or `allow`.
pub(super) fn split(url: &str) -> Result<Query, ConfigError> {
    let (head, query) = url.split_once('?').unwrap_or((url, ""));
    let mut kept = Vec::new();
    let mut mode = TlsMode::Disable;
    let mut root = None;
    let mut idle_set = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "sslmode" => mode = decode(value).parse()?,
            "sslrootcert" => root = Some(PathBuf::from(decode(value))),
            other => {
                idle_set |= other == "keepalives_idle";
                kept.push(pair);
            }
        }
    }
    let url = if kept.is_empty() {
        head.to_owned()
    } else {
        format!("{head}?{}", kept.join("&"))
    };
    Ok(Query {
        url,
        mode,
        root,
        idle_set,
    })
}

/// `value` percent-decoded; bytes that are not UTF-8 are replaced.
fn decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Query, TlsMode, split};
    use crate::postgres::ConfigError;

    // Catches a TLS parameter left in the URL (the client refuses `verify-full` and every
    // `sslrootcert` as unknown), another parameter dropped or reordered, and a path that is
    // not percent-decoded.
    #[test]
    fn the_tls_parameters_come_out_and_the_rest_stay() {
        let query = split(
            "postgres://u@h/db?connect_timeout=3&sslmode=verify-full&sslrootcert=%2Fetc%2Fca.pem&keepalives_idle=9",
        )
        .expect("splits");
        assert_eq!(
            query,
            Query {
                url: "postgres://u@h/db?connect_timeout=3&keepalives_idle=9".to_owned(),
                mode: TlsMode::VerifyFull,
                root: Some(PathBuf::from("/etc/ca.pem")),
                idle_set: true,
            }
        );
    }

    // Catches a URL with no query, an empty one, or only TLS parameters that comes out with
    // a stray `?`, and a missing `sslmode` that is not `disable`.
    #[test]
    fn a_url_without_other_parameters_has_no_query() {
        for url in [
            "postgres://u@h/db",
            "postgres://u@h/db?",
            "postgres://u@h/db?sslmode=require&",
        ] {
            let query = split(url).expect("splits");
            assert_eq!(query.url, "postgres://u@h/db", "{url}");
            assert!(!query.idle_set);
            assert_eq!(query.root, None);
        }
        assert_eq!(
            split("postgres://u@h/db").expect("splits").mode,
            TlsMode::Disable
        );
    }

    // Catches an `sslmode` libpq does not have (or `allow`, which the client cannot do)
    // taken as some mode, and a key with no `=` that is lost.
    #[test]
    fn an_unknown_sslmode_is_refused_and_a_bare_key_is_kept() {
        for value in ["bogus", "allow", ""] {
            assert_eq!(
                split(&format!("postgres://h/db?sslmode={value}")),
                Err(ConfigError::Invalid(
                    "invalid value for option `sslmode`".to_owned()
                )),
                "{value}"
            );
        }
        assert_eq!(
            split("postgres://h/db?flag").expect("splits").url,
            "postgres://h/db?flag"
        );
    }
}
