//! The plan, its hash, the choice of the offer, the dry run, and a spend run that passes.

use std::collections::BTreeSet;

use js_elastic::conformance::live::{LiveArgs, RUN_LABEL};
use js_elastic::{HostName, HostStatus, SchedulerId};
use sha2::{Digest, Sha256};

use crate::mem::*;

/// Asserts the exact plan lines of a dry run, that the hash is the SHA-256 of exactly the
/// plan text above it, that the read-only case passes, that nothing was launched, and
/// that a machine already listed is reported and left alone.
#[test]
fn a_dry_run_prints_the_plan_and_the_hash_and_launches_nothing() {
    let state = state(one_offer());
    seed_machine(&state, "i-00000000000000009");
    let cloud = MemCloud::new(&state);
    let (status, lines) = go(&args_of(&[]), &cloud);
    let plan = [
        "live conformance plan",
        "provider: mem",
        "run id: run-1",
        "scheduler: live-run-1",
        "location: us-east-1",
        "provider flags: none",
        "offer: cheap zone us-east-1b arch x86_64 purchase OnDemand price 10 US cents per hour",
        "launches: 3, one at a time (acquire_is_idempotent_on_the_acquire_id, \
         accepted_means_launched_not_ready, list_and_observe_see_the_machine)",
        "cost bound: 30 US cents if each machine is gone within its first hour; \
         this assumes hourly or finer billing and is not a quote",
        "labels stamped: validation-run=run-1, plus the scheduler marks the provider adds",
    ];
    let digest = Sha256::digest(format!("{}\n", plan.join("\n")).as_bytes());
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let tail = [
        format!("plan hash: {hash}"),
        "PASS capabilities_cover_every_offer_the_catalog_returns".to_owned(),
        "machines already listed for this scheduler: 1".to_owned(),
        "  i-00000000000000009 Booting".to_owned(),
        "dry run: nothing was launched".to_owned(),
    ];
    let want: Vec<String> = plan.iter().map(ToString::to_string).chain(tail).collect();
    assert_eq!((status, lines), (0, want));
    assert_eq!(
        statuses(&state),
        [HostStatus::Booting],
        "the dry run released nothing"
    );
    assert!(
        state.lock().unwrap().requests.is_empty(),
        "the dry run launched nothing"
    );
    assert_eq!(cloud.rooms(), [1]);
}

/// Asserts that the hash a dry run printed lets the spend run through: all three cases
/// pass, three machines are launched with the run label and the scheduler's name prefix,
/// the cleanup finds nothing running, the rooms asked of the cloud count the machines
/// still terminating, and the exit status is 0.
#[test]
fn a_matching_plan_hash_spends_and_passes_and_leaves_nothing() {
    let state = state(one_offer());
    let cloud = MemCloud::new(&state);
    let (status, lines) = spend(&state, &cloud);
    assert_eq!(status, 0, "{lines:#?}");
    let first = at(&lines, "PASS acquire_is_idempotent_on_the_acquire_id");
    let second = at(&lines, "PASS accepted_means_launched_not_ready");
    let third = at(&lines, "PASS list_and_observe_see_the_machine");
    let summary = at(&lines, "cases: 3 passed, 0 failed");
    assert!(first < second && second < third && third < summary);
    assert!(lines[summary + 1..].starts_with(&[
        "cleanup: releasing every machine of this scheduler".to_owned(),
        "cleanup: released i-00000000000000001: Terminating(Terminating)".to_owned(),
        "cleanup: released i-00000000000000002: Terminating(Terminating)".to_owned(),
        "cleanup: released i-00000000000000003: Terminating(Terminating)".to_owned(),
        "cleanup: 3 machine(s) listed, none left running".to_owned(),
    ]));
    assert_eq!(statuses(&state), vec![HostStatus::Terminating; 3]);
    // The run's own provider, the three cases (room for the case's machines and those
    // still terminating), and the cleanup.
    assert_eq!(cloud.rooms(), [1, 2, 2, 3, 1]);
    let prefix = HostName::prefix(&SchedulerId::new("live-run-1").unwrap());
    let state = state.lock().unwrap();
    assert_eq!(
        state.requests.len(),
        4,
        "two acquires of the first case, one for each other"
    );
    for (pool, labels, name) in &state.requests {
        assert_eq!(pool, "live-conformance");
        assert_eq!(labels, &[(RUN_LABEL.to_owned(), "run-1".to_owned())]);
        assert!(name.starts_with(&prefix), "{name}");
    }
}

/// Asserts that the spend run passes against a cloud whose release answers `Gone` and
/// whose read then says absent, and against one that says terminated.
#[test]
fn a_gone_or_terminated_release_passes() {
    for (release, left) in [(Release::Gone, 0), (Release::Terminated, 3)] {
        let state = state(one_offer());
        state.lock().unwrap().release = release;
        let cloud = MemCloud::new(&state);
        let (status, lines) = spend(&state, &cloud);
        assert_eq!(status, 0, "{lines:#?}");
        let cleaned = format!("cleanup: {left} machine(s) listed, none left running");
        at(&lines, &cleaned);
    }
}

/// Asserts that a plan hash that is not the plan's launches nothing and exits 1 naming
/// both hashes: the hash of a plan at one price does not release a spend at another.
#[test]
fn a_plan_hash_that_does_not_match_launches_nothing() {
    let cheap = state(one_offer());
    let (_, dry) = go(&args_of(&[]), &MemCloud::new(&cheap));
    let old = printed_hash(&dry);
    let dearer = state(vec![offer(
        "cheap",
        Some("us-east-1b"),
        "x86_64",
        Some(11),
        true,
    )]);
    let cloud = MemCloud::new(&dearer);
    let (status, lines) = go(&spend_args(&old), &cloud);
    let new = printed_hash(&lines);
    assert_ne!(new, old);
    assert_eq!(status, 1);
    let said =
        format!("the plan changed: this plan hashes to {new}, not {old}; nothing was launched");
    assert_eq!(lines.last(), Some(&said));
    assert!(dearer.lock().unwrap().requests.is_empty());
    assert_eq!(cloud.rooms(), [1], "no case built a provider");
}

/// Asserts that the named shape is launched when it is in the catalog for the location,
/// available, bootable by the image and priced, whatever other offers cost; and refused,
/// with the reason, when it is missing, is in another location, is not available, cannot be
/// booted by the image, or has no price. The dry run exits 1 for each refusal and a spend
/// run with a hash launches nothing; an offer with no zone is in every location; the default
/// `launchable` boots every offer.
#[test]
fn a_named_shape_is_found_in_the_location_or_refused() {
    let catalog = vec![
        offer("named", Some("us-east-1a"), "x86_64", Some(70), true),
        offer("cheaper", Some("us-east-1a"), "x86_64", Some(1), true),
        offer("far", Some("eu-west-1a"), "x86_64", Some(7), true),
        offer("gone", Some("us-east-1a"), "x86_64", Some(7), false),
        offer("arm", Some("us-east-1a"), "arm64", Some(7), true),
        offer("free", Some("us-east-1a"), "x86_64", None, true),
        offer("any-zone", None, "x86_64", Some(20), true),
    ];
    let state = state(catalog);
    let mut cloud = MemCloud::new(&state);
    cloud.unlaunchable = Some("arm64");
    for (shape, line) in [
        (
            "named",
            "offer: named zone us-east-1a arch x86_64 purchase unstated price 70 US cents per hour",
        ),
        (
            "any-zone",
            "offer: any-zone zone none arch x86_64 purchase unstated price 20 US cents per hour",
        ),
    ] {
        let (status, lines) = go(&args_of(&["--shape", shape]), &cloud);
        assert_eq!(status, 0, "{shape}");
        at(&lines, line);
    }
    // With the default `launchable`, every offer is launchable, so the arm offer is.
    let (status, lines) = go(&args_of(&["--shape", "arm"]), &Plain(&cloud));
    assert_eq!(status, 0);
    at(
        &lines,
        "offer: arm zone us-east-1a arch arm64 purchase unstated price 7 US cents per hour",
    );
    let refusals = [
        (
            "missing",
            "shape missing is not in the catalog for us-east-1",
        ),
        ("far", "shape far is not in the catalog for us-east-1"),
        (
            "gone",
            "shape gone is in the catalog for us-east-1 but is not available",
        ),
        (
            "arm",
            "shape arm is in the catalog but the machine image cannot boot it",
        ),
        ("free", "shape free has no known price; spending needs one"),
    ];
    let hash = "0".repeat(64);
    for (shape, message) in refusals {
        for more in [vec![], vec!["--spend", "--plan-hash", hash.as_str()]] {
            let mut all = vec!["--shape", shape];
            all.extend(more);
            let mut cloud = MemCloud::new(&state);
            cloud.unlaunchable = Some("arm64");
            let (status, lines) = go(&args_of(&all), &cloud);
            assert_eq!((status, lines), (1, vec![message.to_owned()]), "{shape}");
            assert_eq!(cloud.rooms(), [1]);
        }
    }
    assert!(state.lock().unwrap().requests.is_empty());
}

/// Asserts which zones are in a location: the location itself, the location and one
/// lowercase letter (`us-east-1a`), the location, a dash and one lowercase letter
/// (`us-central1-a`), and an offer with no zone; not another region that shares the
/// prefix (`us-east-10a`), not a longer suffix, not an upper-case or missing letter.
#[test]
fn a_zone_is_in_a_location_only_by_the_providers_zone_spelling() {
    let fits = [
        ("us-east-1", Some("us-east-1")),
        ("us-east-1", Some("us-east-1a")),
        ("us-east-1", Some("us-east-1z")),
        ("us-east-1a", Some("us-east-1a")),
        ("us-central1", Some("us-central1-a")),
        ("us-central1-a", Some("us-central1-a")),
        ("us-east-1", None),
    ];
    let misses = [
        ("us-east-1", Some("us-east-10a")),
        ("us-east-1", Some("us-east-10")),
        ("us-east-1", Some("us-east-1ab")),
        ("us-east-1", Some("us-east-1A")),
        ("us-east-1", Some("us-east-11")),
        ("us-east-1a", Some("us-east-1b")),
        ("us-east-1a", Some("us-east-1")),
        ("us-central1", Some("us-central1-ab")),
        ("us-central1", Some("us-central1-")),
        ("us-central1", Some("us-central1-A")),
        ("us-central1", Some("us-central1-1")),
        ("us-central1", Some("us-central12-a")),
        ("us-central1", Some("xus-central1-a")),
    ];
    for (want, cases) in [(0, &fits[..]), (1, &misses[..])] {
        for &(location, zone) in cases {
            let catalog = vec![offer("cheap", zone, "x86_64", Some(1), true)];
            let args = LiveArgs::parse(
                &strings(&["--run-id", "r", "--zone", location, "--shape", "cheap"]),
                &[],
            )
            .unwrap();
            let (status, lines) = go(&args, &MemCloud::new(&state(catalog)));
            assert_eq!(status, want, "{location} {zone:?}: {lines:?}");
            if want == 1 {
                let message = format!("shape cheap is not in the catalog for {location}");
                assert_eq!(lines, [message], "{location} {zone:?}");
            }
        }
    }
}

/// Asserts that the plan lists the provider flags sorted by name, and that changing the
/// value of one changes the hash, so that a spend run with another image or key file than
/// the dry run's launches nothing and exits 1.
#[test]
fn a_changed_provider_flag_changes_the_plan_hash_and_launches_nothing() {
    let state = state(one_offer());
    let flags = ["--key-file", "keys/one", "--image", "ami-1"];
    let (status, dry) = go(&args_of(&flags), &MemCloud::new(&state));
    assert_eq!(status, 0);
    at(&dry, "provider flags: image=ami-1, key-file=keys/one");
    let old = printed_hash(&dry);
    let mut same = flags.to_vec();
    same.extend(["--spend", "--plan-hash", &old]);
    let (status, lines) = go(&args_of(&same), &MemCloud::new(&state));
    assert_eq!(status, 0, "{lines:#?}");
    let before = state.lock().unwrap().requests.len();
    assert_eq!(before, 4);
    for changed in [
        ["--key-file", "keys/one", "--image", "ami-2"],
        ["--key-file", "keys/two", "--image", "ami-1"],
    ] {
        let (_, dry) = go(&args_of(&changed), &MemCloud::new(&state));
        let new = printed_hash(&dry);
        assert_ne!(new, old);
        let mut spend = changed.to_vec();
        spend.extend(["--spend", "--plan-hash", &old]);
        let cloud = MemCloud::new(&state);
        let (status, lines) = go(&args_of(&spend), &cloud);
        assert_eq!(status, 1);
        let said =
            format!("the plan changed: this plan hashes to {new}, not {old}; nothing was launched");
        assert_eq!(lines.last(), Some(&said));
        assert_eq!(cloud.rooms(), [1], "no case built a provider");
    }
    assert_eq!(state.lock().unwrap().requests.len(), before);
}

/// Asserts that a provider that cannot be built and a catalog that fails each exit 1 with
/// the cause, launching nothing.
#[test]
fn a_provider_or_catalog_error_at_the_start_exits_1() {
    let state = state(one_offer());
    let mut cloud = MemCloud::new(&state);
    cloud.fail_builds = BTreeSet::from([0]);
    let (status, lines) = go(&args_of(&[]), &cloud);
    assert_eq!(
        (status, lines),
        (
            1,
            vec!["cannot build the provider: build 0 was refused".to_owned()]
        )
    );
    state.lock().unwrap().catalog = Err("the catalog is down".to_owned());
    let (status, lines) = go(&args_of(&[]), &MemCloud::new(&state));
    assert_eq!(
        (status, lines),
        (
            1,
            vec!["the catalog failed: the catalog is down".to_owned()]
        )
    );
}
