//! A job can pin a bundle by digest: the same pin table as an image, the exclusion with
//! `image`, and the old-agent behaviour. Each test says the defect it catches.

mod common;

use std::collections::BTreeMap;

use common::{expect, job};
use js_agent_wire::{Command, Isolation, JobSpec, Limits, Mount, Rule, ValidationError, refusal};
use serde::Deserialize;
use serde_json::{Value, json};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// What a pin check says about a reference, with the field and code left out so an image
/// and a bundle can be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Accepted,
    Unpinned,
    Malformed,
}

/// A start whose image is `image`.
fn image_verdict(image: &str) -> Verdict {
    let mut spec = job();
    spec.image = image.into();
    match spec.validate().and_then(|()| spec.require_pinned()) {
        Ok(()) => Verdict::Accepted,
        Err(err) if err.code() == refusal::IMAGE_DIGEST_REQUIRED => Verdict::Unpinned,
        Err(err) => {
            assert_eq!(err.code(), refusal::INVALID_IMAGE, "{image}");
            Verdict::Malformed
        }
    }
}

/// A start whose bundle is `bundle`, beside a good image. `validate` refuses the pair, so
/// the grammar is read from its rule (anything but the exclusion is the grammar) and the
/// pin from `require_pinned`, which `validate` does not stop.
fn bundle_verdict(bundle: &str) -> Verdict {
    let mut spec = job();
    spec.bundle = Some(bundle.into());
    if let Err(err) = spec.validate()
        && err.rule() != Rule::Inconsistent
    {
        assert_eq!(err.code(), refusal::INVALID_BUNDLE, "{bundle}");
        assert_eq!(err.field(), "bundle");
        return Verdict::Malformed;
    }
    match spec.require_pinned() {
        Ok(()) => Verdict::Accepted,
        Err(err) => {
            expect(
                err,
                "bundle",
                Rule::NotPinned,
                refusal::BUNDLE_DIGEST_REQUIRED,
            );
            Verdict::Unpinned
        }
    }
}

/// One table of references run through the image check and the bundle check. It catches
/// a rule that differs between the two: every row must give the stated verdict for both.
#[test]
fn images_and_bundles_accept_and_refuse_the_same_pins() {
    let short = format!("a@sha256:{}", &HEX[..40]);
    let short63 = format!("a@sha256:{}", &HEX[..63]);
    let long = format!("a@sha256:{HEX}0");
    let sha512 = format!("a@sha512:{HEX}{HEX}");
    let tiny = format!("a@sha256:{}", &HEX[..31]);
    let upper = format!("a@sha256:{}", HEX.to_uppercase());
    let upper_name = format!("A@sha256:{HEX}");
    let transport = format!("oci:foo@sha256:{HEX}");
    let nul = format!("a\0b@sha256:{HEX}");
    let table: [(String, Verdict); 18] = [
        (format!("a@sha256:{HEX}"), Verdict::Accepted),
        (format!("ghcr.io/o/i:v1@sha256:{HEX}"), Verdict::Accepted),
        (
            format!("localhost:5000/x/y:1.2-rc_1@sha256:{HEX}"),
            Verdict::Accepted,
        ),
        ("ubuntu:24.04".into(), Verdict::Unpinned),
        ("alpine".into(), Verdict::Unpinned),
        (short, Verdict::Unpinned),
        (short63, Verdict::Unpinned),
        (long, Verdict::Unpinned),
        (sha512, Verdict::Unpinned),
        (tiny, Verdict::Malformed),
        (upper, Verdict::Malformed),
        ("a@sha256".into(), Verdict::Malformed),
        ("a@sha256:".into(), Verdict::Malformed),
        ("a@:abc".into(), Verdict::Malformed),
        (upper_name, Verdict::Malformed),
        (transport, Verdict::Malformed),
        (nul, Verdict::Malformed),
        ("two words".into(), Verdict::Malformed),
    ];
    for (text, want) in &table {
        assert_eq!(image_verdict(text), *want, "image {text:?}");
        assert_eq!(bundle_verdict(text), *want, "bundle {text:?}");
    }
}

/// A bundle with a tag only is refused `bundle-digest-required` naming `bundle`, and a
/// malformed one `invalid-bundle`; a start command carries the same check. It catches a
/// bundle that a host would fetch by a name that can move.
#[test]
fn an_unpinned_bundle_is_refused_by_its_own_code() {
    let mut spec = job();
    spec.bundle = Some("toolchain:latest".into());
    expect(
        spec.require_pinned().unwrap_err(),
        "bundle",
        Rule::NotPinned,
        refusal::BUNDLE_DIGEST_REQUIRED,
    );
    let mut spec = job();
    spec.bundle = Some("Bad Name@sha256:00".into());
    expect(
        spec.validate().unwrap_err(),
        "bundle",
        Rule::BadChars,
        refusal::INVALID_BUNDLE,
    );
    let mut spec = job();
    spec.bundle = Some("x".repeat(js_agent_wire::limits::MAX_IMAGE_LEN + 1));
    expect(
        spec.validate().unwrap_err(),
        "bundle",
        Rule::TooLong,
        refusal::INVALID_BUNDLE,
    );
    spec.bundle = Some(String::new());
    expect(
        spec.validate().unwrap_err(),
        "bundle",
        Rule::Empty,
        refusal::INVALID_BUNDLE,
    );
}

/// A bundle beside a non-empty image is refused `invalid-bundle`, by `validate`, by the
/// start command and inside a reply (which drops only that start). It catches a job that
/// an agent from before the field would run as a plain image job, without its tools.
#[test]
fn a_bundle_beside_an_image_is_refused() {
    let mut spec = job();
    spec.bundle = Some(format!("toolchain@sha256:{HEX}"));
    let err = spec.validate().unwrap_err();
    expect(err, "bundle", Rule::Inconsistent, refusal::INVALID_BUNDLE);
    let err = Command::Start(Box::new(spec.clone()))
        .validate()
        .unwrap_err();
    assert_eq!(err.code(), refusal::INVALID_BUNDLE);
    let reply = js_agent_wire::HeartbeatReply {
        commands: vec![
            Command::Start(Box::new(spec)),
            Command::Stop { run_id: "r".into() },
        ],
        ..js_agent_wire::HeartbeatReply::default()
    };
    let checked = reply.validate();
    assert_eq!(checked.refused.len(), 1);
    assert_eq!(checked.refused[0].error.code(), refusal::INVALID_BUNDLE);
    assert_eq!(checked.commands.len(), 1);
}

/// Today every job needs an image, so a bundle with none is refused as the image is: the
/// bundle alone does not make a job runnable. It catches a bundle that slips past the
/// image rule before a runtime exists to run it.
#[test]
fn a_bundle_without_an_image_is_refused_as_an_empty_image() {
    let mut spec = job();
    spec.image = String::new();
    spec.bundle = Some(format!("toolchain@sha256:{HEX}"));
    expect(
        spec.validate().unwrap_err(),
        "image",
        Rule::Empty,
        refusal::INVALID_IMAGE,
    );
}

/// A job without a bundle is unchanged: it validates, serialises without the field and
/// decodes without it. It catches a field that is not optional on the wire.
#[test]
fn a_job_without_a_bundle_is_unchanged() {
    let spec = job();
    assert_eq!(spec.bundle, None);
    assert_eq!(spec.validate(), Ok(()));
    assert_eq!(spec.require_pinned(), Ok(()));
    let value = serde_json::to_value(&spec).unwrap();
    assert!(value.get("bundle").is_none());
    let decoded: JobSpec = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.bundle, None);
}

/// The exact JSON of a spec that carries a bundle, with a field a later version adds
/// beside it. The bundle round trips; the unknown field is ignored, not kept.
#[test]
fn a_bundle_round_trips_and_an_unknown_field_is_ignored() {
    let mut spec = job();
    spec.bundle = Some(format!("toolchain:1@sha256:{HEX}"));
    let mut value = serde_json::to_value(&spec).unwrap();
    assert_eq!(value["bundle"], json!(format!("toolchain:1@sha256:{HEX}")));
    value["added_later"] = json!({"k": 1});
    let decoded: JobSpec = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, spec);
    let again = serde_json::to_value(&decoded).unwrap();
    assert!(again.get("added_later").is_none());
    assert_eq!(again["bundle"], json!(format!("toolchain:1@sha256:{HEX}")));
}

/// A copy of `JobSpec` as it was before the `bundle` field, frozen. It stands for an agent
/// that has not been upgraded.
#[derive(Debug, Deserialize)]
struct OldJobSpec {
    run_id: String,
    lease_id: String,
    image: String,
    #[serde(default)]
    args: Vec<String>,
    limits: Limits,
    #[serde(default)]
    gpu_indices: Vec<u32>,
    #[serde(default)]
    mounts: Vec<Mount>,
    #[serde(default)]
    isolation: Isolation,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    heartbeat_seq: u64,
    #[serde(default)]
    valid_for_ms: u64,
}

impl OldJobSpec {
    /// What an old agent checks before it acts: the rules of this version without a bundle.
    fn check(&self) -> Result<(), ValidationError> {
        let spec = JobSpec {
            run_id: self.run_id.clone(),
            lease_id: self.lease_id.clone(),
            image: self.image.clone(),
            args: self.args.clone(),
            limits: self.limits.clone(),
            gpu_indices: self.gpu_indices.clone(),
            mounts: self.mounts.clone(),
            isolation: self.isolation,
            labels: self.labels.clone(),
            heartbeat_seq: self.heartbeat_seq,
            valid_for_ms: self.valid_for_ms,
            bundle: None,
            native: None,
        };
        Command::Start(Box::new(spec)).validate()
    }
}

/// A frozen old agent ignores `bundle`, so the one defence is that a job with a bundle has
/// an empty image: the old agent refuses it as `invalid-image`. A job with both would run
/// there as a plain image job, which is why this version refuses that pair. It catches a
/// change that lets a bundle sit beside an image.
#[test]
fn an_old_agent_refuses_a_bundle_job_by_its_empty_image() {
    let mut spec = job();
    spec.image = String::new();
    spec.bundle = Some(format!("toolchain@sha256:{HEX}"));
    let wire: Value = serde_json::to_value(Command::Start(Box::new(spec))).unwrap();
    let old: OldJobSpec = serde_json::from_value(wire).unwrap();
    let err = old.check().unwrap_err();
    expect(err, "image", Rule::Empty, refusal::INVALID_IMAGE);

    let mut both = job();
    both.bundle = Some(format!("toolchain@sha256:{HEX}"));
    let wire: Value = serde_json::to_value(&both).unwrap();
    let old: OldJobSpec = serde_json::from_value(wire).unwrap();
    assert_eq!(old.check(), Ok(()), "the old agent takes the pair");
    assert_eq!(
        both.validate().unwrap_err().code(),
        refusal::INVALID_BUNDLE,
        "this version does not send it"
    );
}
