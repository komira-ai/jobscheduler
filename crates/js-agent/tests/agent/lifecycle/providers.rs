//! A start whose layers or mounts name a node-local provider
//! (`docs/design/mount-providers.md`): what the admission refuses, the providers asked at
//! assignment, the wait for their directories beside the image pull, and how a start that a
//! provider refuses, or that waits too long, ends. The providers here answer as a test
//! scripts them (`fake_providers`); the release at the end of a run is `providers_end`'s and
//! a restart's `providers_restart`'s.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_agent::lifecycle::{Event, JobFs, ProviderAnswer};
use js_agent_wire::refusal;
use js_agent_wire::{HeldState, JobMount, JobStartSpec, Layer, MountMode, MountSource, Outcome};

use super::fake_providers::{
    FakeProviders, IMAGE, LAYER, WAIT_MS, bare, mount, prepare, provided_mount, rig, sweep, tree,
    with_layer, with_layer_and_mount, with_mount,
};
use super::per_job::{job_start, listing, start};
use super::{Harness, names};

/// Steps `h` four times and returns every event they gave, as `names` has them.
fn steps(h: &mut Harness) -> Vec<String> {
    (0..4).flat_map(|_| names(&h.life.step())).collect()
}

/// The message of the finding of `code` of `job_id`.
fn finding(h: &Harness, job_id: &str, code: &str) -> String {
    let found = h.life.findings();
    let found = found
        .iter()
        .find(|finding| finding.run_id.as_deref() == Some(job_id) && finding.code == code);
    found.expect("the finding").message.clone()
}

/// Asserts that the providers are asked for each source of a start at once, when it is
/// admitted: `Prepare` with the ref and the mode (a layer's read-only, a mount's its own),
/// in order of the sources, and then `Mount`, before the start's image is here and so before
/// any container is made; and that the image is asked for in the same step, once. Catches a
/// provider that begins its work only when the container is being made, which is what the
/// whole design is to avoid.
#[test]
fn prepare_is_asked_at_assignment_before_the_image_is_ready() {
    let (mut h, providers) = rig("prov-prepare", |_| {});
    h.fake.pull_pending(IMAGE);
    let spec = JobStartSpec {
        mounts: vec![provided_mount("k2", "/opt/sdk", MountMode::ReadWrite)],
        ..with_layer("j1")
    };
    h.apply(vec![job_start(spec)]);
    assert_eq!(
        providers.sent("prepare"),
        [
            "provider prepare tier j1#0 k1 ReadOnly",
            "provider prepare tier j1#1 k2 ReadWrite"
        ]
    );
    assert_eq!(
        providers.sent("mount"),
        ["provider mount tier j1#0", "provider mount tier j1#1"],
        "asked as soon as each was prepared"
    );
    assert_eq!(h.fake.asks(IMAGE), [None], "the image is asked for once");
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert!(h.fake.created().is_empty(), "no container yet");
    h.life.step();
    assert_eq!(providers.sent("prepare").len(), 2, "not asked again");
}

/// Asserts that a start whose provider answers `Mount` late (a call still out, then
/// unavailable, then the directory) starts as soon as it has the directory, and that the
/// lookups of its host paths are then asked about the directory: the layer is a host path
/// layer of it. Catches a start that fails at the first answer that is not the directory, and
/// one that looks up the ref and not what the provider made.
#[test]
fn a_mount_answered_late_still_starts_the_job() {
    let (mut h, providers) = rig("prov-late", |_| {});
    let path = tree(&h, "ro", "t1");
    let asked = mount("j1", 0);
    providers.then("tier", &asked, ProviderAnswer::Pending);
    providers.then(
        "tier",
        &asked,
        ProviderAnswer::Unavailable("not ready".to_owned()),
    );
    providers.mounts_at("tier", "j1", 0, &path);
    h.apply(vec![job_start(with_layer("j1"))]);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(
        h.fake.looked_up("j1"),
        Some(JobFs::default()),
        "the lookups begun at the admission leave the provider's layer out"
    );
    let events = steps(&mut h);
    assert_eq!(events, ["started j1"]);
    assert_eq!(
        providers.sent("mount").len(),
        2,
        "the call out is not sent again"
    );
    let looked_up = h.fake.looked_up("j1").expect("looked up");
    assert_eq!(looked_up.layers, [Layer::HostPath { path }]);
    assert!(looked_up.mounts.is_empty());
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
}

/// Asserts that a provider that never gives the directory fails the start
/// `input-unavailable` once `--mount-provider-wait` has passed since its admission, naming
/// the provider, the source and the last reason, and not a moment before; and that its
/// source is released. Catches a start held for ever by a provider that is down, and one
/// failed early.
#[test]
fn a_provider_that_never_mounts_fails_the_start_input_unavailable() {
    let (mut h, providers) = rig("prov-never", |_| {});
    let unavailable = ProviderAnswer::Unavailable("not ready".to_owned());
    providers.always("tier", &mount("j1", 0), unavailable);
    h.apply(vec![job_start(with_layer("j1"))]);
    h.life.step();
    h.clock.advance(WAIT_MS - 1);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    h.clock.advance(1);
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_UNAVAILABLE));
    assert_eq!(
        finding(&h, "j1", "start-failed"),
        "the provider tier did not mount source 0 within 30 s: not ready"
    );
    assert!(h.fake.created().is_empty());
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
}

/// Asserts that a provider that refuses a ref (at `Prepare`) or a tree (at `Mount`) fails the
/// start `provider-refused` at once, with the provider's reason, naming the provider and the
/// source, and that the source is released all the same. Catches a refusal that is waited
/// out as if the provider were down, and a refusal reported as the host's.
#[test]
fn a_refused_ref_fails_the_start_provider_refused() {
    for (n, at_mount) in [false, true].into_iter().enumerate() {
        let (mut h, providers) = rig(&format!("prov-refused-{n}"), |_| {});
        let refused = ProviderAnswer::Refused("no such tree".to_owned());
        match at_mount {
            false => providers.always(
                "tier",
                &prepare("j1", 0, "k1", MountMode::ReadOnly),
                refused,
            ),
            true => providers.always("tier", &mount("j1", 0), refused),
        }
        h.apply(vec![job_start(with_layer("j1"))]);
        h.life.step();
        let result = listing(&h, "j1").result.expect("a result");
        assert_eq!(result.outcome, Outcome::FailedToStart, "{n}");
        assert_eq!(
            result.detail.as_deref(),
            Some(refusal::PROVIDER_REFUSED),
            "{n}"
        );
        assert_eq!(
            finding(&h, "j1", "start-failed"),
            "the provider tier refused source 0: no such tree",
            "{n}"
        );
        assert_eq!(
            providers.sent("release"),
            ["provider release tier j1#0"],
            "{n}"
        );
        assert!(h.fake.created().is_empty(), "{n}");
    }
}

/// Asserts that the record of a start's provider sources (`providers/<run>.json` in the state
/// root: the provider and the index of each source, the directory `0700` and the file `0600`)
/// is on disk when the first `Prepare` is sent, and is gone once the sources are released.
/// Catches a crash between `Prepare` and the record that leaves a source nobody can release,
/// and a record that outlives its run.
#[test]
fn the_record_is_written_before_the_first_prepare_and_removed_after_release() {
    let (mut h, providers) = rig("prov-record", |_| {});
    providers.watch_records(h.dir.path("state/providers"));
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    providers.mounts_at("tier", "j1", 1, &tree(&h, "ro", "t2"));
    h.apply(vec![job_start(with_layer_and_mount("j1"))]);
    assert_eq!(
        providers.sent("prepare"),
        [
            "provider prepare tier j1#0 k1 ReadOnly record=yes",
            "provider prepare tier j1#1 k2 ReadOnly record=yes"
        ]
    );
    let record = h.dir.path("state/providers/j1.json");
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        r#"{"sources":[{"provider":"tier","index":0},{"provider":"tier","index":1}]}"#
    );
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&record), 0o600);
    assert_eq!(mode(&h.dir.path("state/providers")), 0o700);
    steps(&mut h);
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    assert!(record.is_file(), "kept while the run lives");
    h.fake.exit("j1", 0);
    steps(&mut h);
    assert!(!record.exists(), "removed once released");
    assert_eq!(providers.sent("release").len(), 2);
}

/// Asserts that a provider's directory is held to the agent's roots as a host path is: one
/// beneath no root, and one that is not an absolute path in normal form, fail the start
/// `input-not-allowed`, and so does a read-write mount under a read-only root; a read-only or
/// copy-on-write mount under a read-only root, and a read-write one under a read-write root,
/// start. Catches a provider that mounts a path outside what the operator listed, and a job
/// that writes through a root given read-only.
#[test]
fn a_directory_outside_the_roots_or_a_write_under_a_read_only_root_fails_input_not_allowed() {
    let cases = [
        (
            "outside",
            "/elsewhere/t",
            MountMode::ReadOnly,
            Some("lies beneath no --mount-root or --mount-root-ro"),
        ),
        (
            "relative",
            "t",
            MountMode::ReadOnly,
            Some("is not an absolute path"),
        ),
        (
            "write-ro",
            "ro",
            MountMode::ReadWrite,
            Some("which jobs may mount read-only only"),
        ),
        ("read-ro", "ro", MountMode::ReadOnly, None),
        ("cow-ro", "ro", MountMode::CopyOnWrite, None),
        ("write-data", "data", MountMode::ReadWrite, None),
    ];
    for (name, place, mode, failure) in cases {
        let (mut h, providers) = rig(&format!("prov-path-{name}"), |_| {});
        // A root's name stands for a tree in it; any other text is the directory itself.
        let path = match place {
            "ro" | "data" => tree(&h, place, "t"),
            other => other.to_owned(),
        };
        providers.mounts_at("tier", "j1", 0, &path);
        h.apply(vec![job_start(with_mount("j1", mode))]);
        steps(&mut h);
        let listed = listing(&h, "j1");
        match failure {
            Some(text) => {
                let result = listed.result.expect("a result");
                assert_eq!(result.outcome, Outcome::FailedToStart, "{name}");
                assert_eq!(
                    result.detail.as_deref(),
                    Some(refusal::INPUT_NOT_ALLOWED),
                    "{name}"
                );
                let message = finding(&h, "j1", "start-failed");
                assert!(message.contains(text), "{name}: {message}");
                assert!(h.fake.created().is_empty(), "{name}");
            }
            None => assert_eq!(listed.state, HeldState::Running, "{name}"),
        }
    }
}

/// Asserts that each source a provider made is, for the lookups of the start's host paths, a
/// host path of its directory in the place of the source: a layer among the layers, a mount
/// among the mounts with its mode, target and `sub_path` as named, and every other layer and
/// mount (an image, a host path) where it was; and that the sources are asked with their
/// place among the layers and then the mounts (a mount counts after the last layer, an image
/// layer counted). Catches a source moved, dropped, or given a neighbour's directory.
#[test]
fn the_providers_directory_is_the_host_path_the_lookups_open() {
    let (mut h, providers) = rig("prov-fs", |_| {});
    let (first, second) = (tree(&h, "ro", "t1"), tree(&h, "data", "t2"));
    providers.mounts_at("tier", "j1", 1, &first);
    providers.mounts_at("tier", "j1", 3, &second);
    let sdk = JobMount {
        source: MountSource::HostPath {
            path: tree(&h, "data", "sdk"),
        },
        ..provided_mount("unused", "/opt/sdk", MountMode::ReadOnly)
    };
    let writable = JobMount {
        sub_path: Some("sub".to_owned()),
        ..provided_mount("k2", "/opt/rw", MountMode::ReadWrite)
    };
    let spec = JobStartSpec {
        layers: vec![
            Layer::Image {
                image: LAYER.to_owned(),
            },
            Layer::Provider {
                provider: "tier".to_owned(),
                reference: "k1".to_owned(),
            },
        ],
        mounts: vec![sdk.clone(), writable.clone()],
        ..start("j1")
    };
    h.apply(vec![job_start(spec)]);
    steps(&mut h);
    assert_eq!(
        providers.sent("prepare"),
        [
            "provider prepare tier j1#1 k1 ReadOnly",
            "provider prepare tier j1#3 k2 ReadWrite"
        ]
    );
    let looked_up = h.fake.looked_up("j1").expect("looked up");
    assert_eq!(
        looked_up.layers,
        [
            Layer::Image {
                image: LAYER.to_owned()
            },
            Layer::HostPath { path: first }
        ]
    );
    let bound = JobMount {
        source: MountSource::HostPath { path: second },
        ..writable
    };
    assert_eq!(looked_up.mounts, [sdk, bound]);
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
}

/// Asserts that a start that names a provider the agent was not given (one the lifecycle was
/// never given a client for, or given others only) is refused `input-not-allowed` saying so,
/// creates nothing, writes no record and asks no provider; and that the same start is
/// admitted once the agent has the provider. Catches a source that waits for a provider that
/// is not there until the start timeout.
#[test]
fn a_start_naming_a_provider_the_agent_was_not_given_is_refused_at_admission() {
    let refused = |events: &[Event]| -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Refused { code, why, .. } => Some(((*code).to_owned(), why.clone())),
                _ => None,
            })
            .collect()
    };
    let want = (
        refusal::INPUT_NOT_ALLOWED.to_owned(),
        "the start names the provider tier, which this agent was not given".to_owned(),
    );
    let mut none = bare("prov-none");
    let events = none.apply(vec![job_start(with_layer("j1"))]);
    assert_eq!(refused(&events), std::slice::from_ref(&want));
    assert!(!none.dir.path("state/providers").exists(), "no record");
    assert!(none.fake.created().is_empty());

    let mut other = bare("prov-other");
    let given = FakeProviders::new(&["other"], &other.log);
    other
        .life
        .attach_providers(Box::new(given.clone()), WAIT_MS);
    let events = other.apply(vec![job_start(with_layer("j1"))]);
    assert_eq!(refused(&events), [want]);
    assert!(given.sent("prepare").is_empty());

    let (mut has, _) = rig("prov-given", |_| {});
    let events = has.apply(vec![job_start(with_layer("j1"))]);
    assert!(refused(&events).is_empty());
}

/// Asserts that a provider whose sweep at start is not answered is not asked for a source:
/// the start waits (as `starting`), is asked `Prepare` the step after the sweep answered, and
/// then runs; and that one that never answers fails the start `input-unavailable` at the
/// provider wait, saying the sweep is why. Catches a `Prepare` that a late sweep would
/// release, and a start held for ever by a sweep.
#[test]
fn a_provider_whose_sweep_is_unanswered_holds_the_start_until_it_answers() {
    let (mut h, providers) = rig("prov-sweep-wait", |providers| {
        providers.always("tier", &sweep(), ProviderAnswer::Pending);
    });
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    h.apply(vec![job_start(with_layer("j1"))]);
    steps(&mut h);
    assert!(providers.sent("prepare").is_empty());
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    providers.then("tier", &sweep(), ProviderAnswer::Done(None));
    let events = steps(&mut h);
    assert_eq!(events, ["started j1"]);
    assert_eq!(
        providers.sent("prepare"),
        ["provider prepare tier j1#0 k1 ReadOnly"]
    );
    assert_eq!(
        providers.sent("sweep").len(),
        1,
        "one sweep, still out until answered"
    );

    let (mut late, _) = rig("prov-sweep-late", |providers| {
        providers.always("tier", &sweep(), ProviderAnswer::Pending);
    });
    late.apply(vec![job_start(with_layer("j2"))]);
    late.life.step();
    late.clock.advance(WAIT_MS);
    late.life.step();
    let result = listing(&late, "j2").result.expect("a result");
    assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_UNAVAILABLE));
    assert_eq!(
        finding(&late, "j2", "start-failed"),
        "the provider tier did not mount source 0 within 30 s: the provider tier has not \
         answered its sweep since the agent started"
    );
}

/// Asserts that a sweep that fails is reported once (`provider-sweep-failed`, with the
/// reason), asked again ten seconds after each failure and not sooner, and no more once it
/// is answered. Catches a sweep given up after one try, one asked at every step, and a
/// finding repeated at every try.
#[test]
fn a_failed_sweep_is_asked_again_every_ten_seconds_and_reported_once() {
    let mut h = bare("prov-sweep-failed");
    let providers = FakeProviders::new(&["tier"], &h.log);
    let down = ProviderAnswer::Unavailable("socket missing".to_owned());
    providers.always("tier", &sweep(), down);
    let events = h
        .life
        .attach_providers(Box::new(providers.clone()), WAIT_MS);
    assert_eq!(names(&events), ["finding - provider-sweep-failed"]);
    let Event::Finding(found) = &events[0] else {
        panic!("a finding");
    };
    assert_eq!(
        found.message,
        "the provider tier has not answered the sweep at start: socket missing; asked again \
         every ten seconds, and a start that names it waits"
    );
    assert_eq!(providers.sent("sweep"), ["provider sweep tier keep=[]"]);
    h.clock.advance(9_999);
    assert!(names(&h.life.step()).is_empty());
    assert_eq!(providers.sent("sweep").len(), 1);
    h.clock.advance(1);
    assert!(names(&h.life.step()).is_empty(), "reported once");
    assert_eq!(providers.sent("sweep").len(), 2);
    providers.always("tier", &sweep(), ProviderAnswer::Done(None));
    h.clock.advance(10_000);
    h.life.step();
    assert_eq!(providers.sent("sweep").len(), 3);
    h.clock.advance(10_000);
    h.life.step();
    assert_eq!(
        providers.sent("sweep").len(),
        3,
        "answered: not asked again"
    );
}

/// Asserts that a start whose record cannot be written (the `providers` directory of the
/// state root is a file) fails `input-unavailable` with a `provider-record-failed` finding
/// and asks no provider. Catches a source prepared that a restart could not release.
#[test]
fn a_record_that_cannot_be_written_fails_the_start_before_any_provider_is_asked() {
    let (mut h, providers) = rig("prov-record-fails", |_| {});
    fs::write(h.dir.path("state/providers"), "not a directory").unwrap();
    h.apply(vec![job_start(with_layer("j1"))]);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_UNAVAILABLE));
    let message = finding(&h, "j1", "provider-record-failed");
    assert!(
        message.starts_with("the record of the run's provider sources cannot be written: "),
        "{message}"
    );
    steps(&mut h);
    assert!(providers.sent("prepare").is_empty());
    assert!(
        providers.sent("release").is_empty(),
        "nothing was asked, nothing is released"
    );
}

/// Asserts that a start that waits for its provider and an image together fails when the
/// image cannot be had, with the image's failure and not the provider's wait, and its
/// sources are released. Catches a failed pull that is noticed only when the provider has
/// answered.
#[test]
fn a_failed_image_ends_a_start_that_still_waits_for_its_provider() {
    let (mut h, providers) = rig("prov-image", |_| {});
    h.fake.pull_pending(IMAGE);
    let unavailable = ProviderAnswer::Unavailable("not ready".to_owned());
    providers.always("tier", &mount("j1", 0), unavailable);
    h.apply(vec![job_start(with_layer("j1"))]);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    h.fake.pull_fails(IMAGE, "no such image");
    h.life.step();
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("runtime-failed"));
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
}

/// Asserts that a provider that answers a start's `Mount` with no directory is asked again
/// until the wait is over, and then named as having given none. Catches an empty answer taken
/// for a directory.
#[test]
fn a_mount_that_answers_no_directory_is_asked_again() {
    let (mut h, providers) = rig("prov-nothing", |_| {});
    providers.always("tier", &mount("j1", 0), ProviderAnswer::Done(None));
    h.apply(vec![job_start(with_layer("j1"))]);
    h.life.step();
    h.clock.advance(WAIT_MS);
    h.life.step();
    assert_eq!(
        finding(&h, "j1", "start-failed"),
        "the provider tier did not mount source 0 within 30 s: the provider answered no \
         directory"
    );
}
