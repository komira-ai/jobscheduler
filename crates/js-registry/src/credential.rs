use std::fmt;

use js_agent_wire::{InstanceIdentity, Taint};
use sha2::Digest;

/// What a host presented to be admitted, reduced to its SHA-256 digest.
///
/// The registry binds a host id to the first credential it was admitted with and compares
/// later hellos against that binding, so it keeps the digest and never the token. The
/// transport computes it from the join token it authenticated the request with.
///
/// Equality is on the digest, which is what the registry compares. The constant-time
/// comparison of the token itself, when the transport checks a request against the
/// pool's accepted tokens, belongs to the transport and is not done here.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Credential([u8; 32]);

impl Credential {
    /// The credential of a join token.
    pub fn of_token(token: &str) -> Self {
        Self(sha2::Sha256::digest(token.as_bytes()).into())
    }

    /// The digest as 64 lower-case hex digits: the form a caller that persists bindings
    /// writes. It is a one-way digest of a token of at least 32 random bytes, but it is
    /// what a host id is bound to, so the file that holds it should be as private as the
    /// token file.
    pub fn to_hex(&self) -> String {
        crate::session::secret_text(&self.0)
    }

    /// The credential whose [`Credential::to_hex`] is `text`; `None` for anything else
    /// (a wrong length, an upper-case or non-hex digit).
    pub fn from_hex(text: &str) -> Option<Self> {
        let digits = text.as_bytes();
        if digits.len() != 64 {
            return None;
        }
        let mut digest = [0; 32];
        for (byte, pair) in digest.iter_mut().zip(digits.chunks_exact(2)) {
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(digest))
    }
}

/// The value of a lower-case hex digit.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(..)")
    }
}

/// What a host id is bound to: the credential its first hello was admitted with and, when
/// that hello named the instance it runs on, that instance.
///
/// A later hello for the id must match the credential, and must match the instance when
/// the binding has one; any mismatch is a refusal as host-bound. A binding without an
/// instance holds at the level of the pool credential only: a holder of the pool's token
/// can say hello for such an id once the host is lost. Per-host tokens or client
/// certificates close that later (design D18).
///
/// A binding also carries the taint the host reported, when it has, and the time its
/// cordon lapses, when it is cordoned, so that what a caller persists for a host id holds
/// them too ([`Binding::with_taint`], [`Binding::with_cordon`]). Neither plays a part in
/// who may say hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    credential: Credential,
    instance: Option<InstanceIdentity>,
    taint: Option<Taint>,
    cordon_until_ms: Option<u64>,
}

impl Binding {
    /// A binding to `credential`, and to `instance` when there is one, with no taint and
    /// no cordon.
    pub fn new(credential: Credential, instance: Option<InstanceIdentity>) -> Self {
        Self {
            credential,
            instance,
            taint: None,
            cordon_until_ms: None,
        }
    }

    /// This binding with the cordon that lapses at `until_ms` (milliseconds on the
    /// registry's clock), or with none.
    pub fn with_cordon(self, until_ms: Option<u64>) -> Self {
        Self {
            cordon_until_ms: until_ms,
            ..self
        }
    }

    /// When the host's cordon lapses, in milliseconds on the registry's clock, when the
    /// host is cordoned.
    pub fn cordon_until_ms(&self) -> Option<u64> {
        self.cordon_until_ms
    }

    /// This binding with `taint` as the host's reported taint.
    pub fn with_taint(self, taint: Option<Taint>) -> Self {
        Self { taint, ..self }
    }

    /// The taint the host reported, when it has.
    pub fn taint(&self) -> Option<&Taint> {
        self.taint.as_ref()
    }

    /// The credential the host id is bound to.
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The instance the host id is bound to, when its first hello named one.
    pub fn instance(&self) -> Option<&InstanceIdentity> {
        self.instance.as_ref()
    }

    /// Whether a hello presenting `credential` and `instance` comes from what this
    /// binding names.
    pub(crate) fn admits(
        &self,
        credential: &Credential,
        instance: Option<&InstanceIdentity>,
    ) -> bool {
        let same_credential = self.credential == *credential;
        let same_instance = self
            .instance
            .as_ref()
            .is_none_or(|bound| Some(bound) == instance);
        [same_credential, same_instance].into_iter().all(|ok| ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential survives its hex form, the form is 64 lower-case digits, and any
    /// other text is no credential. It catches a persisted binding that reads back as a
    /// different credential (a host id bound to nobody) or one that accepts malformed
    /// text as a digest.
    #[test]
    fn a_credential_round_trips_through_hex_and_nothing_else_parses() {
        let credential = Credential::of_token("a-join-token-of-some-length-0123456789");
        let text = credential.to_hex();
        assert_eq!(text.len(), 64);
        assert!(text.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(text, text.to_ascii_lowercase());
        assert_eq!(Credential::from_hex(&text), Some(credential));
        assert_eq!(Credential::from_hex(&text[1..]), None);
        assert_eq!(Credential::from_hex(&format!("{text}0")), None);
        assert_eq!(Credential::from_hex(&text.to_uppercase()), None);
        let low = format!("g{}", &text[1..]);
        assert_eq!(Credential::from_hex(&low), None);
        let high = format!("{}g", &text[..63]);
        assert_eq!(Credential::from_hex(&high), None);
        assert_eq!(Credential::from_hex(""), None);
    }
}
