//! The form of a callback URL, as the submit checks it (`docs/design/job-retry-and-completion-
//! callback.md`, section 5.2): https, a host, no userinfo, no fragment, at most
//! [`MAX_URL_LEN`] bytes. The URL's address is not judged here: the sender resolves the host
//! and checks every address it would connect to at each try (section 5.3).
//!
//! A refusal never repeats the URL, whose path or query may carry the receiver's own
//! reference, and a [`CallbackUrl`]'s `Debug` shows its host alone.

use std::fmt;

/// The longest a callback URL may be, in bytes.
pub const MAX_URL_LEN: usize = 2048;

/// The scheme every callback URL has, compared without regard to case.
const SCHEME: &str = "https://";

/// Why a text is not a callback URL. No variant holds any of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    /// The text is longer than [`MAX_URL_LEN`] bytes.
    #[error("a callback URL is at most {MAX_URL_LEN} bytes")]
    TooLong,
    /// The text holds a byte outside printable ASCII, or a space.
    #[error("a callback URL holds only printable ASCII and no space")]
    NotPrintableAscii,
    /// The text does not start with `https://`.
    #[error("a callback URL is https")]
    NotHttps,
    /// The text has a fragment (a `#`).
    #[error("a callback URL has no fragment")]
    Fragment,
    /// The text names a user before its host (an `@` in its authority).
    #[error("a callback URL has no userinfo")]
    Userinfo,
    /// The text names no host, or an IPv6 literal without its closing bracket.
    #[error("a callback URL names a host")]
    NoHost,
    /// The text's port is empty, not decimal, or outside 1 to 65535.
    #[error("a callback URL's port is 1 to 65535")]
    BadPort,
}

impl UrlError {
    /// The word a refusal names the fault by.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::TooLong => "too-long",
            Self::NotPrintableAscii => "not-printable-ascii",
            Self::NotHttps => "not-https",
            Self::Fragment => "fragment",
            Self::Userinfo => "userinfo",
            Self::NoHost => "no-host",
            Self::BadPort => "bad-port",
        }
    }
}

/// A callback URL of the right form ([`check_url`]). Its `Debug` shows its host and nothing
/// else.
#[derive(Clone, PartialEq, Eq)]
pub struct CallbackUrl {
    /// The URL as given.
    text: String,
    /// The host, as the URL writes it (an IPv6 literal with its brackets).
    host: String,
    /// The port, 443 when the URL names none.
    port: u16,
}

impl CallbackUrl {
    /// The URL as it was given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The host, as the URL writes it: a name, an IPv4 literal, or an IPv6 literal in its
    /// brackets. A log line names this and no other part of the URL.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port the call goes to: the URL's, else 443.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Debug for CallbackUrl {
    /// `CallbackUrl { host: "…", .. }`: the path and query, which may carry the receiver's
    /// reference, are not shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallbackUrl")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// `url` as a callback URL, when it has the form section 5.2 of the design asks for: at most
/// [`MAX_URL_LEN`] bytes of printable ASCII with no space, the scheme `https` (in any case), a
/// host, an optional port of 1 to 65535, no userinfo and no fragment. The checks run in that
/// order, so the first fault is the one named.
///
/// # Errors
/// The [`UrlError`] of the first fault.
pub fn check_url(url: &str) -> Result<CallbackUrl, UrlError> {
    if url.len() > MAX_URL_LEN {
        return Err(UrlError::TooLong);
    }
    if !url.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(UrlError::NotPrintableAscii);
    }
    let scheme = url.get(..SCHEME.len()).unwrap_or_default();
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return Err(UrlError::NotHttps);
    }
    if url.contains('#') {
        return Err(UrlError::Fragment);
    }
    let rest = &url[SCHEME.len()..];
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(UrlError::Userinfo);
    }
    let (host, port) = host_and_port(authority)?;
    Ok(CallbackUrl {
        text: url.to_owned(),
        host: host.to_owned(),
        port,
    })
}

/// The host and port of `authority`, which holds no `@`.
fn host_and_port(authority: &str) -> Result<(&str, u16), UrlError> {
    let (host, port) = match authority.strip_prefix('[') {
        Some(literal) => {
            let close = literal.find(']').ok_or(UrlError::NoHost)?;
            let after = &literal[close + 1..];
            let host = &authority[..close + 2];
            match after {
                "" => (host, None),
                _ => (
                    host,
                    Some(after.strip_prefix(':').ok_or(UrlError::BadPort)?),
                ),
            }
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if matches!(host, "" | "[]") {
        return Err(UrlError::NoHost);
    }
    let port = match port {
        None => 443,
        Some(text) => port_of(text)?,
    };
    Ok((host, port))
}

/// The port `text` names: 1 to 5 decimal digits whose value is 1 to 65535.
fn port_of(text: &str) -> Result<u16, UrlError> {
    let digits = (1..=5).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    let value = digits.then(|| text.parse::<u16>().ok()).flatten();
    value.filter(|port| *port != 0).ok_or(UrlError::BadPort)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the host and port taken from each accepted form: a name with and without a
    /// path, query and port, the scheme in capitals, an IPv4 literal, an IPv6 literal with and
    /// without a port, the port at both ends of its range, and a URL of exactly the longest
    /// length; the URL is kept as given. Catches a host cut at the wrong place, a default port
    /// other than 443, a bound off by one, and a URL changed on the way in.
    #[test]
    fn each_accepted_form_gives_its_host_and_port() {
        let longest = format!("https://h.example/{}", "p".repeat(MAX_URL_LEN - 18));
        assert_eq!(longest.len(), MAX_URL_LEN);
        let cases = [
            ("https://hooks.example", "hooks.example", 443),
            ("https://hooks.example/js-done/ref-7", "hooks.example", 443),
            ("https://hooks.example?ref=7", "hooks.example", 443),
            ("https://hooks.example:8443/x?y=z", "hooks.example", 8443),
            ("HTTPS://Hooks.Example/x", "Hooks.Example", 443),
            ("https://203.0.113.9:1/x", "203.0.113.9", 1),
            ("https://[2001:db8::1]/x", "[2001:db8::1]", 443),
            ("https://[2001:db8::1]:65535", "[2001:db8::1]", 65535),
            (longest.as_str(), "h.example", 443),
        ];
        for (text, host, port) in cases {
            let url = check_url(text).expect(text);
            assert_eq!((url.as_str(), url.host(), url.port()), (text, host, port));
        }
    }

    /// Asserts the fault named for each refused form, and that the first fault in the order of
    /// the checks is the one named: one byte too long, a space, a control byte, a byte above
    /// ASCII, plain http, no scheme, another scheme, a fragment (also an empty one), userinfo
    /// (also with a password), no host, an empty IPv6 literal, an unclosed one, text after a
    /// literal that is not a port, an empty port, a port of 0, of 65536, of six digits, with a
    /// sign and with a letter. Catches each form let through, and a fault named for another.
    #[test]
    fn each_refused_form_names_its_fault() {
        let too_long = format!("https://h.example/{}", "p".repeat(MAX_URL_LEN - 17));
        let cases = [
            (too_long.as_str(), UrlError::TooLong),
            ("https://h.example/a b", UrlError::NotPrintableAscii),
            ("https://h.example/\n", UrlError::NotPrintableAscii),
            ("https://h.example/caf\u{e9}", UrlError::NotPrintableAscii),
            ("http://h.example/", UrlError::NotHttps),
            ("h.example/", UrlError::NotHttps),
            ("", UrlError::NotHttps),
            ("ftp://h.example/", UrlError::NotHttps),
            ("https:/h.example/", UrlError::NotHttps),
            ("https://h.example/#top", UrlError::Fragment),
            ("https://h.example#", UrlError::Fragment),
            ("https://user@h.example/", UrlError::Userinfo),
            ("https://user:pw@h.example/", UrlError::Userinfo),
            ("https:///path", UrlError::NoHost),
            ("https://", UrlError::NoHost),
            ("https://:443/", UrlError::NoHost),
            ("https://[]/", UrlError::NoHost),
            ("https://[2001:db8::1/", UrlError::NoHost),
            ("https://[2001:db8::1]x/", UrlError::BadPort),
            ("https://h.example:/", UrlError::BadPort),
            ("https://h.example:0/", UrlError::BadPort),
            ("https://h.example:65536/", UrlError::BadPort),
            ("https://h.example:000443/", UrlError::BadPort),
            ("https://h.example:+443/", UrlError::BadPort),
            ("https://h.example:44a/", UrlError::BadPort),
            ("https://h.example:1:2/", UrlError::BadPort),
            // Two faults: the earlier check names its own.
            ("http://user@h.example/#f", UrlError::NotHttps),
            ("https://user@h.example/#f", UrlError::Fragment),
        ];
        for (text, fault) in cases {
            assert_eq!(check_url(text), Err(fault), "{text:?}");
        }
    }

    /// Asserts each fault's word and its whole text, none of which holds any of a URL.
    /// Catches a refusal word other than the one the job API names, and a message that would
    /// repeat what the submitter sent.
    #[test]
    fn each_fault_has_its_word_and_text() {
        let faults = [
            (
                UrlError::TooLong,
                "too-long",
                "a callback URL is at most 2048 bytes",
            ),
            (
                UrlError::NotPrintableAscii,
                "not-printable-ascii",
                "a callback URL holds only printable ASCII and no space",
            ),
            (UrlError::NotHttps, "not-https", "a callback URL is https"),
            (
                UrlError::Fragment,
                "fragment",
                "a callback URL has no fragment",
            ),
            (
                UrlError::Userinfo,
                "userinfo",
                "a callback URL has no userinfo",
            ),
            (UrlError::NoHost, "no-host", "a callback URL names a host"),
            (
                UrlError::BadPort,
                "bad-port",
                "a callback URL's port is 1 to 65535",
            ),
        ];
        for (fault, code, text) in faults {
            assert_eq!((fault.code(), fault.to_string().as_str()), (code, text));
        }
    }

    /// Asserts the whole `Debug` text of a URL with a path, a query and a port: its host
    /// alone. Catches a derived `Debug`, which would print the receiver's reference in the
    /// path or query into a log line.
    #[test]
    fn a_urls_debug_shows_its_host_alone() {
        let url = check_url("https://hooks.example:8443/js-done/ref-7?token=t").expect("a URL");
        assert_eq!(
            format!("{url:?}"),
            r#"CallbackUrl { host: "hooks.example", .. }"#
        );
        assert_eq!(url.clone(), url);
    }
}
