//! The identity of a machine as the cloud signs it. An instance can ask the metadata
//! server for an identity token (`instance/service-accounts/default/identity` with
//! `format=full`): a JSON Web Token, signed by Google with RS256, whose claims name the
//! instance under `google.compute_engine` (its project, zone, id and name). A hello that
//! carries one proves the machine is the instance it says it is, with no secret on the
//! instance.
//!
//! **Keys.** Google publishes the public keys of these tokens and rotates them, each key
//! named by the `kid` of the token's header. [`GoogleIdentityVerifier`] checks a token
//! against the keys pinned in [`GOOGLE_KEYS`], by key id. No key is pinned in this release:
//! Google replaces its keys every few weeks, so a key pinned now would refuse every token
//! soon after, and the agent sends no identity yet. Until a key is pinned, every hello that
//! carries an identity is refused, which is the safe answer; a hello without one is
//! matched by its name and instance id.
//!
//! **What is not checked.** The token's `aud` and `exp`: the provider has no wall clock,
//! and a token, however old, names only the instance it was issued to, which the provider
//! compares with the instance it launched under that name.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use js_elastic::SignedIdentity;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The public keys Google signs instance identity tokens with that this crate pins: the
/// key id and the RSA modulus, big-endian without leading zeros; the exponent is 65537.
/// Empty in this release (see the module documentation).
pub const GOOGLE_KEYS: [(&str, &[u8]); 0] = [];

/// The public exponent of the keys, 65537, big-endian.
const EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];

/// The issuers an identity token may name (`iss`).
const ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

/// The claims of an identity token the provider checks: those of
/// `google.compute_engine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityClaims {
    /// `project_id`.
    pub project_id: String,
    /// `zone`.
    pub zone: String,
    /// `instance_id`, the instance's id in decimal.
    pub instance_id: String,
    /// `instance_name`.
    pub instance_name: String,
}

/// A check of a machine's signed identity.
pub trait IdentityVerifier: Send + Sync {
    /// The claims `identity` carries, when its signature verifies and its claims hold the
    /// four strings of [`IdentityClaims`]; `None` otherwise.
    fn verify(&self, identity: &SignedIdentity) -> Option<IdentityClaims>;
}

/// The claims of the object `value`: its four string fields.
fn claims_of(value: &Value) -> Option<IdentityClaims> {
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(IdentityClaims {
        project_id: field("project_id")?,
        zone: field("zone")?,
        instance_id: field("instance_id")?,
        instance_name: field("instance_name")?,
    })
}

/// The JSON object a part of a token holds, base64url without padding.
fn part(text: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The verifier of the identity tokens Google signs: RS256 over the token's header and
/// claims, under the pinned key its header names. The token is the identity's `document`,
/// as the metadata server serves it; it carries its own signature, so the identity's
/// `signature` is not read.
#[derive(Debug, Clone)]
pub struct GoogleIdentityVerifier {
    keys: Vec<(String, Vec<u8>)>,
}

impl GoogleIdentityVerifier {
    /// The verifier for the pinned keys ([`GOOGLE_KEYS`]).
    pub fn new() -> Self {
        Self::pinned(&GOOGLE_KEYS)
    }

    /// The verifier for `keys`, each a key id and the modulus of its key.
    fn pinned(keys: &[(&str, &[u8])]) -> Self {
        Self::with_keys(
            keys.iter()
                .map(|(kid, modulus)| ((*kid).to_owned(), modulus.to_vec()))
                .collect(),
        )
    }

    /// The verifier for other keys, such as a test key: each a key id and an RSA modulus
    /// in big-endian order without leading zeros, with the exponent 65537.
    pub fn with_keys(keys: Vec<(String, Vec<u8>)>) -> Self {
        Self { keys }
    }

    /// How many keys it holds.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }
}

impl Default for GoogleIdentityVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentityVerifier for GoogleIdentityVerifier {
    fn verify(&self, identity: &SignedIdentity) -> Option<IdentityClaims> {
        let token = identity.document.as_str();
        let (signed, signature) = token.rsplit_once('.')?;
        let (header, claims) = signed.split_once('.')?;
        let header = part(header)?;
        let rs256 = header.get("alg").and_then(Value::as_str) == Some("RS256");
        let kid = header.get("kid").and_then(Value::as_str)?;
        let (_, modulus) = self.keys.iter().find(|(id, _)| id == kid)?;
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        RsaPublicKeyComponents {
            n: modulus.as_slice(),
            e: EXPONENT.as_slice(),
        }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        .ok()?;
        let claims = part(claims)?;
        let issuer = claims.get("iss").and_then(Value::as_str);
        let issued = issuer.is_some_and(|iss| ISSUERS.contains(&iss));
        let engine = claims.get("google")?.get("compute_engine")?;
        (rs256 & issued).then(|| claims_of(engine)).flatten()
    }
}

/// A verifier for tests that signs and checks with a shared test key instead of the
/// cloud's: the document is the JSON object of the claims (the fields of
/// [`IdentityClaims`]), and its signature is the lower-case hexadecimal SHA-256 of the
/// key, a newline and the document.
#[derive(Debug, Clone)]
pub struct FakeIdentityVerifier {
    key: String,
}

impl FakeIdentityVerifier {
    /// A verifier whose test key is `key`.
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
        }
    }

    /// The signature of `document` under the test key.
    fn mac(&self, document: &str) -> String {
        let digest = Sha256::digest(format!("{}\n{document}", self.key).as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The identity a machine would send for `document`, signed with the test key.
    pub fn sign(&self, document: &str) -> SignedIdentity {
        SignedIdentity {
            document: document.to_owned(),
            signature: self.mac(document),
        }
    }
}

impl IdentityVerifier for FakeIdentityVerifier {
    fn verify(&self, identity: &SignedIdentity) -> Option<IdentityClaims> {
        let signed = identity.signature == self.mac(&identity.document);
        let value: Value = serde_json::from_str(&identity.document).ok()?;
        signed.then(|| claims_of(&value)).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the default verifier holds the pinned keys, none in this release, and
    /// so refuses a token whatever it says. It catches a key table that drifts from what the
    /// module documentation says is pinned.
    #[test]
    fn the_default_verifier_holds_the_pinned_keys() {
        assert_eq!(
            GoogleIdentityVerifier::default().key_count(),
            GOOGLE_KEYS.len()
        );
        assert_eq!(GoogleIdentityVerifier::new().key_count(), 0);
        let identity = SignedIdentity {
            document: "e30.e30.c2ln".to_owned(),
            signature: String::new(),
        };
        assert_eq!(GoogleIdentityVerifier::new().verify(&identity), None);
    }

    /// Asserts that a verifier made from pinned keys holds one key for each, whatever the
    /// pinned table holds in this release.
    #[test]
    fn a_verifier_holds_one_key_for_each_pinned_key() {
        let keys: [(&str, &[u8]); 2] = [("one", &[1, 2, 3]), ("two", &[4])];
        assert_eq!(GoogleIdentityVerifier::pinned(&keys).key_count(), 2);
    }
}
