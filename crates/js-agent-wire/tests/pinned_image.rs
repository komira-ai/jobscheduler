//! A start must pin its image by digest: `require_pinned` and the start command's
//! `validate`, each test saying the defect it catches.

mod common;

use common::{broken, expect, job, job_start};
use js_agent_wire::{Command, HeartbeatReply, JobStartSpec, Rule, refusal};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn pinned(image: &str) -> Result<(), js_agent_wire::ValidationError> {
    let mut spec = job();
    spec.image = image.into();
    spec.validate()?;
    spec.require_pinned()
}

/// A tag-only image, a bare name, a short digest and other algorithms are refused with
/// `image-digest-required` naming `image`. It catches a start whose image can change under
/// a host that pulls what it lacks.
#[test]
fn an_image_without_a_sha256_digest_is_refused() {
    let short = format!("a@sha256:{}", &HEX[..40]);
    let long = format!("a@sha256:{HEX}0");
    let sha512 = format!("a@sha512:{HEX}{HEX}");
    for image in [
        "ubuntu:24.04",
        "alpine",
        "ghcr.io/o/i:v1",
        short.as_str(),
        long.as_str(),
        sha512.as_str(),
    ] {
        let err = pinned(image).unwrap_err();
        expect(
            err,
            "image",
            Rule::NotPinned,
            refusal::IMAGE_DIGEST_REQUIRED,
        );
    }
}

/// Digests that break the grammar itself (upper case hex, fewer than 32 digits, no
/// algorithm) are refused as `invalid-image` before the pin is looked at.
#[test]
fn a_malformed_digest_is_an_invalid_image() {
    let upper = format!("a@sha256:{}", HEX.to_uppercase());
    let tiny = format!("a@sha256:{}", &HEX[..31]);
    for image in [upper.as_str(), tiny.as_str(), "a@sha256", "a@:abc"] {
        let err = pinned(image).unwrap_err();
        assert_eq!(err.code(), refusal::INVALID_IMAGE, "{image}");
    }
}

/// A digest alone, and a tag with a digest, pass. It catches a pin check that refuses
/// the form `docker pull name:tag@sha256:...` accepts.
#[test]
fn a_digest_with_or_without_a_tag_passes() {
    for image in [
        format!("a@sha256:{HEX}"),
        format!("ghcr.io/o/i:v1@sha256:{HEX}"),
        format!("localhost:5000/x/y:1.2-rc_1@sha256:{HEX}"),
    ] {
        assert_eq!(pinned(&image), Ok(()), "{image}");
    }
}

/// The job start command, which the agent validates, refuses a tag-only image and takes a
/// pinned one; a stop beside it is unaffected. It catches an agent that acts on a start
/// the server should not have sent.
#[test]
fn a_start_command_needs_a_pinned_image() {
    let tag_only = broken(
        &job_start(),
        |j| j.image = "ubuntu:24.04".into(),
        |j: &JobStartSpec| Command::JobStart(Box::new(j.clone())).validate(),
    );
    expect(
        tag_only,
        "image",
        Rule::NotPinned,
        refusal::IMAGE_DIGEST_REQUIRED,
    );
    assert_eq!(Command::JobStart(Box::new(job_start())).validate(), Ok(()));
    let reply = HeartbeatReply {
        commands: vec![
            Command::JobStart(Box::new(broken_job("ubuntu:24.04"))),
            Command::JobStop { job_id: "r".into() },
        ],
        ..HeartbeatReply::default()
    };
    let checked = reply.validate();
    assert_eq!(checked.refused.len(), 1);
    assert_eq!(
        checked.refused[0].error.code(),
        refusal::IMAGE_DIGEST_REQUIRED
    );
    assert_eq!(checked.commands.len(), 1);
}

fn broken_job(image: &str) -> JobStartSpec {
    let mut spec = job_start();
    spec.image = image.into();
    spec
}
