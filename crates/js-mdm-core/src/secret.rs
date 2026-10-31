//! A value that is never printed: the SCEP challenge the enrollment profile carries.

use std::fmt;

/// A secret value. It has no `Display`, and its `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// `value` as a secret.
    pub fn new(value: &str) -> Secret {
        Secret(value.to_owned())
    }

    /// The value, for the one place it goes (a profile, an environment file).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}
