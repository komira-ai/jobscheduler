//! The user name and password a registry's token realm takes as HTTP basic auth.

use std::fmt;

use base64::Engine as _;

use crate::Error;

/// A user name and password (a registry token, typically) that an allow-list entry
/// sends to its registry's token realm as HTTP basic auth ([`Allow::with_credentials`]).
///
/// It holds only the header value it sends. `Debug` prints `Credentials(<redacted>)` and
/// nothing of the user or password, so a `{:?}` of an [`Allow`], a [`Policy`] or a
/// [`Client`] that holds one is safe to log, and no [`Error`] carries either.
///
/// [`Allow`]: crate::Allow
/// [`Allow::with_credentials`]: crate::Allow::with_credentials
/// [`Policy`]: crate::Policy
/// [`Client`]: crate::Client
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// `Basic <base64 of user:password>`.
    header: String,
}

impl Credentials {
    /// The credentials of `user` with `password`.
    ///
    /// The user may not be empty or hold a colon (basic auth separates the two at the
    /// first one, RFC 7617); the password may not be empty. Neither may hold a control
    /// character (a line break would end the header).
    ///
    /// # Errors
    /// [`Error::Credentials`] saying which rule the text breaks, and never holding it.
    pub fn new(user: &str, password: &str) -> Result<Self, Error> {
        let controls = |text: &str| text.chars().any(char::is_control);
        let user_bad = user.is_empty() | user.contains(':') | controls(user);
        let password_bad = password.is_empty() | controls(password);
        match (user_bad, password_bad) {
            (true, _) => Err(Error::Credentials(
                "the user name is empty or holds a colon or a control character",
            )),
            (false, true) => Err(Error::Credentials(
                "the password is empty or holds a control character",
            )),
            (false, false) => {
                let pair =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
                Ok(Self {
                    header: format!("Basic {pair}"),
                })
            }
        }
    }

    /// The `Authorization` header value.
    pub(crate) fn header(&self) -> &str {
        &self.header
    }
}

/// Prints nothing of the user or the password.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a header built wrongly (the RFC 7617 example) and a password with a colon
    /// cut short: only the first colon separates the two.
    #[test]
    fn the_header_is_basic_auth_of_user_and_password() {
        let made = Credentials::new("Aladdin", "open sesame").expect("credentials");
        assert_eq!(made.header(), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        let colon = Credentials::new("u", "a:b").expect("credentials");
        assert_eq!(colon.header(), "Basic dTphOmI=");
    }

    /// Catches a user name or password that would make a header that means something
    /// else, or none: each rule on its own, and the error text holds none of the input.
    #[test]
    fn a_user_or_password_that_cannot_be_sent_is_refused() {
        let user =
            "registry credentials: the user name is empty or holds a colon or a control character";
        let password = "registry credentials: the password is empty or holds a control character";
        for (u, p, want) in [
            ("", "secret-1", user),
            ("a:b", "secret-1", user),
            ("a\nb", "secret-1", user),
            ("name", "", password),
            ("name", "secret\r\n1", password),
        ] {
            let err = Credentials::new(u, p).expect_err("refused");
            assert_eq!(err.to_string(), want, "{u:?} {p:?}");
        }
    }

    /// Catches a `Debug` that prints the user, the password or the header.
    #[test]
    fn debug_prints_nothing_of_the_secret() {
        let made = Credentials::new("name", "secret-1").expect("credentials");
        assert_eq!(format!("{made:?}"), "Credentials(<redacted>)");
        assert_eq!(format!("{made:#?}"), "Credentials(<redacted>)");
    }
}
