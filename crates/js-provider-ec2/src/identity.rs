//! The identity of a machine as the cloud signs it. An instance can read a document from
//! the instance metadata service that names it (its id, its region, its image) and a
//! signature over that document; a hello that carries both proves the machine is the
//! instance it says it is, with no secret on the instance.

use base64::Engine as _;
use js_elastic::SignedIdentity;
use ring::signature::{RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, RsaPublicKeyComponents};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The RSA modulus of the key AWS signs instance identity documents with, as 128 bytes in
/// big-endian order. AWS publishes one certificate per region for the `signature` of the
/// document; each one checked (nine regions across the Americas, Europe and Asia Pacific)
/// carries this one 1024-bit key, exponent 65537, so the crate pins the key and the
/// provider checks the document's region against the configured one. Source: the AWS EC2
/// user guide pages "Verify the instance identity document" and "AWS public certificates
/// for instance identity document signatures".
const AWS_MODULUS: [u8; 128] = [
    0x87, 0xBD, 0x18, 0xDF, 0xFF, 0x49, 0x12, 0xB6, 0x92, 0x76, 0xE3, 0xC9, 0x21, 0xB4, 0x86, 0x8D,
    0xF2, 0xA9, 0x03, 0x37, 0x7B, 0x64, 0xC3, 0x85, 0x63, 0xBC, 0x0F, 0x67, 0xBC, 0xF9, 0x76, 0x6A,
    0x72, 0x4E, 0xF9, 0xE2, 0x01, 0x52, 0xA3, 0xDF, 0x40, 0x6D, 0x3D, 0x91, 0x99, 0x70, 0xA5, 0x6A,
    0x66, 0xC8, 0xEF, 0x1B, 0x18, 0x1D, 0x91, 0x5A, 0xA5, 0xB1, 0x0B, 0x0B, 0x81, 0xFD, 0xD7, 0x27,
    0x22, 0x86, 0xFA, 0xC3, 0x8D, 0xB4, 0x93, 0xD5, 0x98, 0xE4, 0x2D, 0x08, 0x20, 0x6B, 0x43, 0x44,
    0xD6, 0xAE, 0x37, 0x79, 0x2E, 0xBC, 0x65, 0xE4, 0xC3, 0x71, 0xC4, 0x9C, 0x5D, 0x04, 0x8D, 0x8A,
    0xF4, 0xA5, 0xCC, 0x96, 0x52, 0xF0, 0x72, 0x59, 0x8E, 0x0A, 0xB3, 0x06, 0x55, 0xE3, 0x65, 0xFB,
    0x63, 0xB5, 0xD2, 0x4B, 0x5D, 0xE1, 0x38, 0x87, 0x0B, 0xE8, 0xD2, 0xC0, 0xF8, 0x7F, 0x78, 0xFD,
];

/// The public exponent of the key, 65537, big-endian.
const EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];

/// The fields of an identity document the provider checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityDocument {
    /// The id of the instance, such as `i-0123456789abcdef0`.
    pub instance_id: String,
    /// The region the instance runs in.
    pub region: String,
    /// The id of the image the instance was launched from.
    pub image_id: String,
}

/// A check of a machine's signed identity.
pub trait IdentityVerifier: Send + Sync {
    /// The document `identity` carries, when its signature verifies and the document is a
    /// JSON object with the string fields `instanceId`, `region` and `imageId`; `None`
    /// otherwise.
    fn verify(&self, identity: &SignedIdentity) -> Option<IdentityDocument>;
}

/// The document `text` as a record, when it is a JSON object with the three string fields.
fn parse_document(text: &str) -> Option<IdentityDocument> {
    let value: Value = serde_json::from_str(text).ok()?;
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(IdentityDocument {
        instance_id: field("instanceId")?,
        region: field("region")?,
        image_id: field("imageId")?,
    })
}

/// The verifier for the documents AWS signs: RSA with SHA-256 over the document's bytes,
/// under the key AWS publishes.
#[derive(Debug, Clone)]
pub struct AwsIdentityVerifier {
    modulus: Vec<u8>,
}

impl AwsIdentityVerifier {
    /// The verifier for the pinned AWS key ([`AWS_MODULUS`]).
    pub fn new() -> Self {
        Self::with_modulus(AWS_MODULUS.to_vec())
    }

    /// The verifier for another key, such as a test key: `modulus` is the RSA modulus in
    /// big-endian order without leading zeros, and the exponent is 65537.
    pub fn with_modulus(modulus: Vec<u8>) -> Self {
        Self { modulus }
    }
}

impl Default for AwsIdentityVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentityVerifier for AwsIdentityVerifier {
    fn verify(&self, identity: &SignedIdentity) -> Option<IdentityDocument> {
        let signature = base64::engine::general_purpose::STANDARD
            .decode(&identity.signature)
            .ok()?;
        RsaPublicKeyComponents {
            n: self.modulus.as_slice(),
            e: EXPONENT.as_slice(),
        }
        .verify(
            &RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
            identity.document.as_bytes(),
            &signature,
        )
        .ok()?;
        parse_document(&identity.document)
    }
}

/// A verifier for tests that signs and checks with a shared test key instead of the
/// cloud's: the signature of a document is the lower-case hexadecimal SHA-256 of the key,
/// a newline and the document.
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
    fn verify(&self, identity: &SignedIdentity) -> Option<IdentityDocument> {
        match identity.signature == self.mac(&identity.document) {
            true => parse_document(&identity.document),
            false => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the pinned key is the 128-byte modulus AWS publishes, byte for byte,
    /// and that the default verifier holds it. It catches a mistyped byte in the table.
    #[test]
    fn the_pinned_aws_key_is_the_published_modulus() {
        let hex: String = AWS_MODULUS
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        assert_eq!(
            hex,
            "87BD18DFFF4912B69276E3C921B4868DF2A903377B64C38563BC0F67BCF9766A724EF9E20152A3DF406D3D919970A56A66C8EF1B181D915AA5B10B0B81FDD7272286FAC38DB493D598E42D08206B4344D6AE37792EBC65E4C371C49C5D048D8AF4A5CC9652F072598E0AB30655E365FB63B5D24B5DE138870BE8D2C0F87F78FD"
        );
        assert_eq!(AwsIdentityVerifier::default().modulus, AWS_MODULUS.to_vec());
    }
}
