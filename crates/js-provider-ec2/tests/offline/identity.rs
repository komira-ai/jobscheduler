//! The check of a machine's signed identity. The documents are made-up (an account of
//! zeros, a region of the tests) and signed with a throwaway RSA key whose private half
//! was destroyed; its modulus is given here, so `AwsIdentityVerifier::with_modulus`
//! verifies them and the pinned AWS key does not.

use js_elastic::SignedIdentity;
use js_provider_ec2::{
    AwsIdentityVerifier, FakeIdentityVerifier, IdentityDocument, IdentityVerifier,
};

/// The modulus of the throwaway test key.
const TEST_MODULUS_HEX: &str = "B6C8460E14E6B4919F1A8741ADD7C0C0735B67A95027DE80451183F0A00EB6ABF1F2CAF2BAE681A7F643D5F43917C3D1ECE1F44BA57385713D07CF77D9C399147CD392A4BDBD6A0D492E2ECC9902D40ADB3EE08FA125EA24FB98E0039165983D1FE41F1DCCE2FDA40DA1D5918B730B0C7AE31CCF94E5BD2AE8995E962430FA9F";

const DOC: &str = include_str!("../fixtures/identity/doc.json");
const DOC_SIG: &str = include_str!("../fixtures/identity/doc.sig");
const OTHER: &str = include_str!("../fixtures/identity/other.json");
const OTHER_SIG: &str = include_str!("../fixtures/identity/other.sig");
const REGION: &str = include_str!("../fixtures/identity/region.json");
const REGION_SIG: &str = include_str!("../fixtures/identity/region.sig");

fn hex_bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn test_key() -> AwsIdentityVerifier {
    AwsIdentityVerifier::with_modulus(hex_bytes(TEST_MODULUS_HEX))
}

fn signed(document: &str, signature: &str) -> SignedIdentity {
    SignedIdentity {
        document: document.to_owned(),
        signature: signature.to_owned(),
    }
}

/// Asserts that the key AWS publishes does not verify a document signed by the test key,
/// so the pinned verifier, built with `new` or by default, rejects it. It catches a
/// verifier that accepts any key.
#[test]
fn the_pinned_aws_key_rejects_a_document_of_another_key() {
    let identity = signed(DOC, DOC_SIG);
    assert_eq!(AwsIdentityVerifier::new().verify(&identity), None);
    assert_eq!(AwsIdentityVerifier::default().verify(&identity), None);
}

/// Asserts that a document signed by the key is accepted and read into its instance id,
/// region and image id, whichever of the signed documents it is. It catches a verifier
/// that rejects a good signature and one that reads the wrong fields.
#[test]
fn a_document_signed_by_the_key_is_accepted_with_its_fields() {
    let verifier = test_key();
    assert_eq!(
        verifier.verify(&signed(DOC, DOC_SIG)),
        Some(IdentityDocument {
            instance_id: "i-0123456789abcdef0".to_owned(),
            region: "region-a".to_owned(),
            image_id: "ami-0123456789abcdef0".to_owned(),
        })
    );
    assert_eq!(
        verifier.verify(&signed(OTHER, OTHER_SIG)),
        Some(IdentityDocument {
            instance_id: "i-0fedcba9876543210".to_owned(),
            region: "region-a".to_owned(),
            image_id: "ami-0fedcba9876543210".to_owned(),
        })
    );
    assert_eq!(
        verifier.verify(&signed(REGION, REGION_SIG)),
        Some(IdentityDocument {
            instance_id: "i-0123456789abcdef0".to_owned(),
            region: "region-b".to_owned(),
            image_id: "ami-0123456789abcdef0".to_owned(),
        })
    );
}

/// Asserts that a document with one byte changed, another document's signature, a
/// signature that is not base64, a signature that is base64 of the wrong length and an
/// empty signature are all rejected. It catches a verifier that checks the signature
/// against anything but the document's exact bytes.
#[test]
fn a_changed_document_or_a_bad_signature_is_rejected() {
    let verifier = test_key();
    let changed = DOC.replace("arm64", "arm65");
    assert_ne!(changed, DOC);
    assert_eq!(verifier.verify(&signed(&changed, DOC_SIG)), None);
    assert_eq!(verifier.verify(&signed(DOC, OTHER_SIG)), None);
    assert_eq!(verifier.verify(&signed(OTHER, DOC_SIG)), None);
    assert_eq!(verifier.verify(&signed(DOC, "not base64 !!")), None);
    assert_eq!(verifier.verify(&signed(DOC, "c2lnbmF0dXJl")), None);
    assert_eq!(verifier.verify(&signed(DOC, "")), None);
    let mut flipped = DOC_SIG.to_owned();
    flipped.replace_range(..1, if DOC_SIG.starts_with('r') { "s" } else { "r" });
    assert_eq!(verifier.verify(&signed(DOC, &flipped)), None);
}

/// Asserts that the fake verifier accepts what it signed, reading the same three fields,
/// and rejects a document or signature changed, a signature made with another key, and a
/// document that is correctly signed but is not a JSON object with the three string fields
/// (not JSON, a field missing, a field that is not a string, a JSON value that is not an
/// object). It catches a document read without checking its fields.
#[test]
fn the_fake_verifier_checks_its_own_signature_and_the_fields() {
    let fake = FakeIdentityVerifier::new("test-key");
    let good = r#"{"instanceId":"i-01234567","region":"region-a","imageId":"ami-01234567"}"#;
    let identity = fake.sign(good);
    assert_eq!(identity.document, good);
    assert_eq!(
        fake.verify(&identity),
        Some(IdentityDocument {
            instance_id: "i-01234567".to_owned(),
            region: "region-a".to_owned(),
            image_id: "ami-01234567".to_owned(),
        })
    );
    let changed = SignedIdentity {
        document: good.replace("region-a", "region-b"),
        signature: identity.signature.clone(),
    };
    assert_eq!(fake.verify(&changed), None);
    let unsigned = SignedIdentity {
        signature: "00".to_owned(),
        ..identity.clone()
    };
    assert_eq!(fake.verify(&unsigned), None);
    assert_eq!(
        FakeIdentityVerifier::new("other-key").verify(&identity),
        None
    );
    for document in [
        "not json",
        "[]",
        r#"{"region":"region-a","imageId":"ami-01234567"}"#,
        r#"{"instanceId":"i-01234567","imageId":"ami-01234567"}"#,
        r#"{"instanceId":"i-01234567","region":"region-a"}"#,
        r#"{"instanceId":7,"region":"region-a","imageId":"ami-01234567"}"#,
    ] {
        assert_eq!(fake.verify(&fake.sign(document)), None, "{document}");
    }
}
