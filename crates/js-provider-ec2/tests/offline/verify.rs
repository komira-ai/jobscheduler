//! Matching a machine's first hello to the instance this provider launched.

use std::sync::Arc;

use js_elastic::{Acquired, CapacityProvider, Held, Hello, HostRef, SignedIdentity};
use js_provider_ec2::{FakeIdentityVerifier, InstanceState};

use crate::rig::{
    REGION, Rig, SCHEDULER, X86_IMAGE, ask, host, live, name_of, ours, rig, rig_over, stranger,
};

const KEY: &str = "test-key";

/// A rig whose provider checks identities against the test key.
fn rig_checking(max_machines: usize) -> (Rig, FakeIdentityVerifier) {
    let verifier = FakeIdentityVerifier::new(KEY);
    let mut rig = rig(live(max_machines));
    rig.provider = rig.provider.with_verifier(Arc::new(verifier.clone()));
    (rig, verifier)
}

fn launch(rig: &Rig, acquire: &str) -> Held {
    match rig.provider.acquire(&ask(acquire)) {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

/// The identity document of the instance `instance` in `region` from `image`.
fn document(instance: &str, region: &str, image: &str) -> String {
    format!(r#"{{"instanceId":"{instance}","region":"{region}","imageId":"{image}"}}"#)
}

fn hello(name: &str, instance: Option<&HostRef>, identity: Option<SignedIdentity>) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: instance.map(ToString::to_string),
        identity,
    }
}

/// Asserts that a hello whose identity verifies, and names the instance, the configured
/// region and the image the instance was launched from, is matched to that instance, with
/// the instance id in the hello and without it.
#[test]
fn a_hello_with_a_valid_identity_matches_its_acquire() {
    let (rig, verifier) = rig_checking(2);
    let held = launch(&rig, "a1");
    let signed = verifier.sign(&document(held.id.as_str(), REGION, X86_IMAGE));
    let name = name_of(SCHEDULER, "a1");
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&held.id), Some(signed.clone()))),
        Some(held.id.clone())
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, None, Some(signed))),
        Some(held.id)
    );
}

/// Asserts that a hello whose identity does not describe the machine it names is rejected:
/// the identity of another instance, one in another region, and one of another image, each
/// signed with the right key. It catches a match that trusts the name without the document.
#[test]
fn a_hello_with_another_instances_identity_is_rejected() {
    let (rig, verifier) = rig_checking(2);
    let one = launch(&rig, "a1");
    let two = launch(&rig, "a2");
    let name = name_of(SCHEDULER, "a1");
    let other_image = "ami-0123456789abcdef9";
    for wrong in [
        document(two.id.as_str(), REGION, X86_IMAGE),
        document(one.id.as_str(), "region-b", X86_IMAGE),
        document(one.id.as_str(), REGION, other_image),
    ] {
        let signed = verifier.sign(&wrong);
        assert_eq!(
            rig.provider
                .verify_instance(&hello(&name, Some(&one.id), Some(signed))),
            None,
            "{wrong}"
        );
    }
}

/// Asserts that a hello with an identity whose signature does not verify is rejected: a
/// signature made with another key, a document changed after it was signed, and a signed
/// text that is not an identity document.
#[test]
fn a_hello_with_a_bad_signature_is_rejected() {
    let (rig, verifier) = rig_checking(1);
    let held = launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    let right = document(held.id.as_str(), REGION, X86_IMAGE);
    let stranger_key = FakeIdentityVerifier::new("another-key");
    let changed = SignedIdentity {
        document: right.replace(REGION, "region-b"),
        ..verifier.sign(&right)
    };
    for identity in [
        stranger_key.sign(&right),
        changed,
        verifier.sign("not a document"),
    ] {
        assert_eq!(
            rig.provider
                .verify_instance(&hello(&name, Some(&held.id), Some(identity))),
            None
        );
    }
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&held.id), None)),
        Some(held.id)
    );
}

/// Asserts that a hello with no identity is matched by its name, and by its name and
/// instance id when it carries one.
#[test]
fn a_hello_without_identity_matches_by_name_and_instance_id() {
    let rig = rig(live(1));
    let held = match rig.provider.acquire(&ask("a1")) {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    };
    let name = name_of(SCHEDULER, "a1");
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        Some(held.id.clone())
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&held.id), None)),
        Some(held.id)
    );
}

/// Asserts that a hello with the right name and the wrong instance id is rejected, and so
/// is a name this scheduler derives for an acquire it never launched.
#[test]
fn a_hello_with_the_right_name_and_the_wrong_instance_id_is_rejected() {
    let rig = rig(live(1));
    launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&host(&stranger(1))), None)),
        None
    );
    let never = name_of(SCHEDULER, "never-launched");
    assert_eq!(
        rig.provider.verify_instance(&hello(&never, None, None)),
        None
    );
}

/// Asserts that a hello by name alone, when two instances that are not ending carry the
/// name, is matched to neither, and that the instance id tells them apart; and that an
/// instance that is ending is not matched by name alone.
#[test]
fn a_name_two_instances_carry_needs_the_instance_id() {
    let rig = rig(live(5));
    let (one, two) = (stranger(1), stranger(2));
    for text in [&one, &two] {
        rig.account
            .add_instance(ours(text, InstanceState::Running, "a1"));
    }
    rig.provider.list().unwrap();
    let name = name_of(SCHEDULER, "a1");
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        None
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&host(&two)), None)),
        Some(host(&two))
    );
    rig.account.set_state(
        &js_provider_ec2::InstanceId::new(&two).unwrap(),
        InstanceState::Terminated,
    );
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        Some(host(&one))
    );
}

/// Asserts that a provider that restarted knows which image an instance runs from a list,
/// so a valid identity is matched to an instance only a list showed it.
#[test]
fn a_restarted_provider_checks_an_identity_against_the_listed_image() {
    let (first, verifier) = rig_checking(1);
    let held = launch(&first, "a1");
    let mut restarted = rig_over(Arc::clone(&first.account), live(1));
    restarted.provider = restarted.provider.with_verifier(Arc::new(verifier.clone()));
    let signed = verifier.sign(&document(held.id.as_str(), REGION, X86_IMAGE));
    let name = name_of(SCHEDULER, "a1");
    let with_identity = hello(&name, None, Some(signed));
    assert_eq!(restarted.provider.verify_instance(&with_identity), None);
    restarted.provider.list().unwrap();
    assert_eq!(
        restarted.provider.verify_instance(&with_identity),
        Some(held.id)
    );
}
