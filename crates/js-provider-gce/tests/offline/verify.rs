//! Matching a machine's first hello to the instance this provider launched.

use std::sync::Arc;

use js_elastic::{Acquired, CapacityProvider, Held, Hello, SignedIdentity};
use js_provider_gce::{FakeIdentityVerifier, InstanceStatus};

use crate::rig::{
    OTHER_ZONE, PROJECT, Rig, SCHEDULER, ZONE, ask, host, id, live, name_of, ours, rig, rig_over,
    stranger,
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

/// The instance id inside the scheduler's reference `<zone>/<id>`.
fn bare(held: &Held) -> String {
    held.id.as_str().rsplit('/').next().unwrap().to_owned()
}

/// The identity document of the instance `instance` named `name` in `zone` of `project`.
fn document(project: &str, zone: &str, instance: &str, name: &str) -> String {
    format!(
        r#"{{"project_id":"{project}","zone":"{zone}","instance_id":"{instance}","instance_name":"{name}"}}"#
    )
}

fn hello(name: &str, instance: Option<&str>, identity: Option<SignedIdentity>) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: instance.map(str::to_owned),
        identity,
    }
}

/// Asserts that a hello whose identity verifies, and names the configured project and the
/// zone, id and name of the instance, is matched to that instance, with the instance id of
/// the hello written as the reference, written as the bare id, and left out. It catches an
/// identity check that is never reached or that refuses the machine it was made for.
#[test]
fn a_hello_with_a_valid_identity_matches_its_acquire() {
    let (rig, verifier) = rig_checking(2);
    let held = launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    let signed = verifier.sign(&document(PROJECT, ZONE, &bare(&held), &name));
    for instance in [Some(held.id.as_str().to_owned()), Some(bare(&held)), None] {
        assert_eq!(
            rig.provider
                .verify_instance(&hello(&name, instance.as_deref(), Some(signed.clone()))),
            Some(held.id.clone()),
            "{instance:?}"
        );
    }
}

/// Asserts that a hello whose identity does not describe the machine it names is rejected:
/// the identity of another instance, one of another project, one of another zone and one of
/// another name, each signed with the right key, whether or not the hello names its
/// instance id. It catches a match that trusts the name without the document.
#[test]
fn a_hello_with_another_instances_identity_is_rejected() {
    let (rig, verifier) = rig_checking(2);
    let one = launch(&rig, "a1");
    let two = launch(&rig, "a2");
    let name = name_of(SCHEDULER, "a1");
    let other_name = name_of(SCHEDULER, "a2");
    for wrong in [
        document(PROJECT, ZONE, &bare(&two), &name),
        document("other-project", ZONE, &bare(&one), &name),
        document(PROJECT, OTHER_ZONE, &bare(&one), &name),
        document(PROJECT, ZONE, &bare(&one), &other_name),
    ] {
        let signed = verifier.sign(&wrong);
        for instance in [Some(one.id.as_str()), None] {
            assert_eq!(
                rig.provider
                    .verify_instance(&hello(&name, instance, Some(signed.clone()))),
                None,
                "{wrong} {instance:?}"
            );
        }
    }
}

/// Asserts that a hello with an identity whose signature does not verify is rejected: a
/// signature made with another key, a document changed after it was signed, a signed text
/// that is not a document, and a signed document that lacks a claim; and that the same
/// hello with no identity is matched. It catches an identity accepted without its
/// signature.
#[test]
fn a_hello_with_a_bad_signature_is_rejected() {
    let (rig, verifier) = rig_checking(1);
    let held = launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    let right = document(PROJECT, ZONE, &bare(&held), &name);
    let stranger_key = FakeIdentityVerifier::new("another-key");
    let changed = SignedIdentity {
        document: right.replace(ZONE, OTHER_ZONE),
        ..verifier.sign(&right)
    };
    for identity in [
        stranger_key.sign(&right),
        changed,
        verifier.sign("not a document"),
        verifier.sign(r#"{"project_id":"sample-project"}"#),
    ] {
        assert_eq!(
            rig.provider
                .verify_instance(&hello(&name, Some(held.id.as_str()), Some(identity))),
            None
        );
    }
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(held.id.as_str()), None)),
        Some(held.id)
    );
}

/// Asserts that a hello with no identity is matched by its name alone, and by its name and
/// instance id when it carries one, as the reference and as the bare id. It catches a match
/// that needs an identity the agent does not send yet.
#[test]
fn a_hello_without_identity_matches_by_name_and_instance_id() {
    let rig = rig(live(1));
    let held = launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        Some(held.id.clone())
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(held.id.as_str()), None)),
        Some(held.id.clone())
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&bare(&held)), None)),
        Some(held.id)
    );
}

/// Asserts that a hello with the right name and the wrong instance id is rejected (a
/// reference and a bare id of another instance), and so is a name this scheduler derives
/// for an acquire it never launched, a name another scheduler derives, and a name nobody
/// derives. It catches a match on the name's prefix alone.
#[test]
fn a_hello_with_the_right_name_and_the_wrong_instance_id_is_rejected() {
    let rig = rig(live(1));
    launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    let other = host(&stranger(1));
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(other.as_str()), None)),
        None
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&stranger(1)), None)),
        None
    );
    for foreign in [
        name_of(SCHEDULER, "never-launched"),
        name_of("sched-two", "a1"),
        "plain-vm".to_owned(),
    ] {
        assert_eq!(
            rig.provider.verify_instance(&hello(&foreign, None, None)),
            None,
            "{foreign}"
        );
    }
}

/// Asserts that matching a hello asks the cloud nothing, with an identity and without: the
/// calls the account has seen are the same before and after. It catches a match that reads
/// the instance, which would make a hello cost a request and fail with the cloud.
#[test]
fn verifying_a_hello_makes_no_call() {
    let (rig, verifier) = rig_checking(1);
    let held = launch(&rig, "a1");
    let name = name_of(SCHEDULER, "a1");
    let signed = verifier.sign(&document(PROJECT, ZONE, &bare(&held), &name));
    let before = rig.account.calls();
    assert!(!before.is_empty());
    assert!(
        rig.provider
            .verify_instance(&hello(&name, None, Some(signed)))
            .is_some()
    );
    assert!(
        rig.provider
            .verify_instance(&hello(&name, Some(held.id.as_str()), None))
            .is_some()
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name_of("sched-two", "a1"), None, None)),
        None
    );
    assert_eq!(rig.account.calls(), before);
}

/// Asserts that a hello by name alone, when two instances that are not ending carry the
/// name, is matched to neither, and that the instance id (as the reference or the bare id)
/// tells them apart; that when one is ending the other is matched by name alone; and that
/// when both are ending nothing is matched by name alone, though the id of one still is. It
/// catches a guess between two machines of one name and a machine on its way out taken for
/// the one that is coming up.
#[test]
fn a_name_two_instances_carry_needs_the_instance_id() {
    let rig = rig(live(5));
    let (one, two) = (stranger(1), stranger(2));
    for text in [&one, &two] {
        rig.account
            .add_instance(ours(text, InstanceStatus::Running, "a1"));
    }
    rig.provider.list().unwrap();
    let name = name_of(SCHEDULER, "a1");
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        None
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(host(&two).as_str()), None)),
        Some(host(&two))
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&two), None)),
        Some(host(&two))
    );
    rig.account
        .set_status(&id(&two), InstanceStatus::Terminated);
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        Some(host(&one))
    );
    rig.account.set_status(&id(&one), InstanceStatus::Stopping);
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.verify_instance(&hello(&name, None, None)),
        None
    );
    assert_eq!(
        rig.provider
            .verify_instance(&hello(&name, Some(&one), None)),
        Some(host(&one))
    );
}

/// Asserts that a provider that restarted knows an instance only once a list has shown it,
/// so a valid identity (and a bare name) is matched to an instance only after the list.
/// It catches a restarted provider that matches from nothing, and one that never learns
/// from a list.
#[test]
fn a_restarted_provider_matches_a_hello_after_a_list() {
    let (first, verifier) = rig_checking(1);
    let held = launch(&first, "a1");
    let mut restarted = rig_over(Arc::clone(&first.account), live(1));
    restarted.provider = restarted.provider.with_verifier(Arc::new(verifier.clone()));
    let signed = verifier.sign(&document(
        PROJECT,
        ZONE,
        &bare(&held),
        &name_of(SCHEDULER, "a1"),
    ));
    let name = name_of(SCHEDULER, "a1");
    let with_identity = hello(&name, None, Some(signed));
    assert_eq!(restarted.provider.verify_instance(&with_identity), None);
    assert_eq!(
        restarted
            .provider
            .verify_instance(&hello(&name, None, None)),
        None
    );
    restarted.provider.list().unwrap();
    assert_eq!(
        restarted.provider.verify_instance(&with_identity),
        Some(held.id.clone())
    );
    assert_eq!(
        restarted
            .provider
            .verify_instance(&hello(&name, None, None)),
        Some(held.id)
    );
}
