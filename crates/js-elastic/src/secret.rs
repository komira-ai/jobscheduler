//! Text that must never reach a log, an error message or a status page.

use std::fmt;

/// A secret. It is never in a log, a `Debug` or a status page: it implements neither
/// `Display`, `Clone` nor `Serialize`, and its `Debug` prints `***`. The text comes out
/// only through [`Secret::expose`], at the one place that has to send it.
pub struct Secret(String);

impl Secret {
    /// A secret holding `text`.
    pub fn new(text: String) -> Self {
        Self(text)
    }

    /// The text. Call it only where the secret is delivered.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("***")
    }
}

/// What a machine is given to boot with, rendered by the service and opaque to the
/// provider, which delivers it as it is in whatever field its cloud has. It holds
/// secrets (a join token, a network key), so it is a [`Secret`].
#[derive(Debug)]
pub struct BootPayload(Secret);

impl BootPayload {
    /// A payload of `text`.
    pub fn new(text: String) -> Self {
        Self(Secret::new(text))
    }

    /// The payload's size in bytes.
    pub fn len(&self) -> usize {
        self.0.expose().len()
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.0.expose().is_empty()
    }

    /// The payload, to be delivered to the machine as it is.
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Display;
    use std::marker::PhantomData;

    use super::*;

    const MARKER: &str = "tskey-auth-MARKER-9f3a";

    /// Answers whether `T` implements `Display`, by the method that wins resolution on a
    /// value of `Probe<T>`.
    struct Probe<T>(PhantomData<T>);

    trait ViaDisplay {
        fn displayable(&self) -> bool {
            true
        }
    }

    impl<T: Display> ViaDisplay for Probe<T> {}

    trait ViaNothing {
        fn displayable(&self) -> bool {
            false
        }
    }

    impl<T> ViaNothing for &Probe<T> {}

    /// Asserts that a secret prints `***` and not its text in `Debug`, also inside the
    /// payload that wraps it and inside `{:#?}`.
    #[test]
    fn a_secret_never_prints_its_text() {
        let secret = Secret::new(MARKER.to_owned());
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{secret:#?}"), "***");
        let payload = BootPayload::new(format!("#!/bin/sh\njoin {MARKER}\n"));
        let shown = format!("{payload:?} {payload:#?}");
        assert!(!shown.contains(MARKER), "{shown}");
        assert!(shown.contains("***"));
    }

    /// Asserts that a secret and a payload have no `Display`, so a format string cannot
    /// print them. The probe finds `Display` on a `String`, so a probe that always says
    /// no would fail here.
    #[test]
    #[allow(clippy::needless_borrow)]
    fn a_secret_has_no_display() {
        assert!(Probe::<String>(PhantomData).displayable());
        assert!(!(&Probe::<Secret>(PhantomData)).displayable());
        assert!(!(&Probe::<BootPayload>(PhantomData)).displayable());
    }

    /// Asserts that the text comes out whole through `expose`, and that the payload
    /// reports its size in bytes and whether it is empty.
    #[test]
    fn the_text_comes_out_only_through_expose() {
        let secret = Secret::new(MARKER.to_owned());
        assert_eq!(secret.expose(), MARKER);
        let payload = BootPayload::new("héllo".to_owned());
        assert_eq!(payload.expose(), "héllo");
        // Bytes, not characters.
        assert_eq!(payload.len(), 6);
        assert!(!payload.is_empty());
        assert!(BootPayload::new(String::new()).is_empty());
    }
}
