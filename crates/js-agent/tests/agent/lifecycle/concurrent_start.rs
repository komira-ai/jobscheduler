//! A start with layers or mounts runs its independent waits side by side: at its admission
//! its providers are asked, the runtime begins the lookups of the host paths that wait for no
//! provider and its directories are made; at each step the providers, the images and the
//! lookups are followed together, and the assembly is asked for once all three are done.
//! The providers and the runtime answer as a test scripts them; the clock moves only when a
//! test moves it.

use std::fs;

use js_agent::lifecycle::{Event, FsPrepared, JobFs, NativeRefusal, ProviderAnswer, StartTimeline};
use js_agent_wire::{Command, HeldState, JobStartSpec, MountMode, MountSource, Outcome, refusal};

use super::fake::UNIX_ORIGIN_MS;
use super::fake_providers::{FakeProviders, IMAGE, mount, provided_mount, rig, tree, with_mount};
use super::per_job::{job_start, listing};
use super::rootfs::layered;
use super::{Harness, cleanup, job, names};

/// The lines the shared log holds that begin with one of `kinds`, in order.
fn logged(h: &Harness, kinds: &[&str]) -> Vec<String> {
    let log = h.log.borrow();
    log.iter()
        .filter(|line| kinds.iter().any(|kind| line.starts_with(kind)))
        .cloned()
        .collect()
}

/// A start of `job_id` with a host path layer of `data`, a mount of a host path of it, and a
/// read-only mount `k2` of the provider `tier` (source 2, after the one layer and the one
/// host path mount).
fn mixed(h: &Harness, job_id: &str) -> JobStartSpec {
    let mut spec = layered(h, job_id);
    spec.mounts
        .push(provided_mount("k2", "/opt/tier", MountMode::ReadOnly));
    spec
}

/// A rig whose provider `tier` has taken the ref of `j1` source 2 but not answered its
/// `Mount`, whose runtime's lookups of `j1` are pending, and which has admitted [`mixed`].
fn admitted(name: &str) -> (Harness, FakeProviders) {
    let (mut h, providers) = rig(name, |_| {});
    providers.always("tier", &mount("j1", 2), ProviderAnswer::Pending);
    h.fake.resolve_answers("j1", FsPrepared::Pending);
    let spec = mixed(&h, "j1");
    h.apply(vec![job_start(spec)]);
    (h, providers)
}

/// The refusal a lookup gives.
fn missing() -> FsPrepared {
    FsPrepared::Refused(NativeRefusal {
        code: refusal::INPUT_UNAVAILABLE,
        why: "missing".to_owned(),
    })
}

/// The detail code of the failed result of `job_id`.
fn failed_with(h: &Harness, job_id: &str) -> Option<String> {
    let result = listing(h, job_id).result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart, "{job_id}");
    result.detail
}

/// Asserts that when the worker has admitted a start with a provider mount and a host path
/// layer, all of it pending, the provider has been asked to prepare and mount its source, the
/// runtime has begun the lookups of the fs without the provider's mount (one layer and one
/// mount), and the run's scratch and lease directories exist, in that order in the log, and
/// no assembly is asked. Then asserts that with the lookups ready and the provider still
/// pending the start waits without asking for its assembly, and that with the provider's
/// directory answered the runtime is asked about the fs with that directory bound, and the
/// assembly is asked for at that step. Catches a start whose lookups or directories wait for
/// its providers, and an assembly asked for before the providers have answered.
#[test]
fn the_waits_of_a_start_begin_at_its_admission_and_the_assembly_waits_for_all() {
    let (mut h, providers) = admitted("conc-admit");
    assert_eq!(
        logged(
            &h,
            &["provider prepare", "provider mount", "resolve", "assemble"]
        ),
        [
            "provider prepare tier j1#2 k2 ReadOnly".to_owned(),
            "provider mount tier j1#2".to_owned(),
            format!("resolve j1 (1 layers, 1 mounts, image {IMAGE:?})"),
        ]
    );
    assert!(h.scratch_of("j1").is_dir());
    assert!(h.dir.path("scratch/.lease-fake-j1").is_dir());
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    let before = h.fake.looked_up("j1").expect("looked up");
    assert_eq!(before.layers.len(), 1);
    assert_eq!(before.mounts.len(), 1);
    assert!(
        before
            .mounts
            .iter()
            .all(|mount| matches!(mount.source, MountSource::HostPath { .. }))
    );
    // The lookups are ready; the provider has not answered.
    h.fake.resolve_answers("j1", FsPrepared::Ready);
    assert!(h.life.step().is_empty());
    assert!(
        logged(&h, &["assemble"]).is_empty(),
        "waits for the provider"
    );
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    // The provider answers: the runtime is asked about the bound fs, and the assembly at once.
    let path = tree(&h, "ro", "t1");
    providers.mounts_at("tier", "j1", 2, &path);
    assert!(h.life.step().is_empty());
    let bound = h.fake.looked_up("j1").expect("looked up");
    assert_eq!(bound.layers, before.layers);
    assert_eq!(bound.mounts.len(), 2);
    assert_eq!(
        bound.mounts[1].source,
        MountSource::HostPath { path },
        "the provider's directory, last"
    );
    assert_eq!(logged(&h, &["assemble"]), ["assemble j1"]);
    assert_eq!(names(&h.life.step()), ["started j1"]);
}

/// Asserts the timeline of a start with a provider mount on the test's clock: the directories
/// at the admission (0), `provided` when the provider's directory is answered (20),
/// `resolved` when the lookups of the bound fs are ready (50), and `ready` and `created` at
/// the step after the assembly was asked (70), in that order. Catches a `provided` step
/// missing or measured at the wrong step, and directories made after the lookups.
#[test]
fn the_timeline_of_a_start_with_a_provider_shows_provided_between_dirs_and_resolved() {
    let (mut h, providers) = admitted("conc-timeline");
    h.clock.advance(20);
    let path = tree(&h, "ro", "t1");
    providers.mounts_at("tier", "j1", 2, &path);
    assert!(h.life.step().is_empty());
    h.clock.advance(30);
    h.fake.resolve_answers("j1", FsPrepared::Ready);
    assert!(h.life.step().is_empty());
    h.clock.advance(20);
    let timeline = StartTimeline {
        received_ms: 0,
        placed_at_ms: None,
        received_at_ms: Some(UNIX_ORIGIN_MS),
        steps: vec![
            ("taken", 0),
            ("admitted", 0),
            ("dirs", 0),
            ("provided", 20),
            ("resolved", 50),
            ("ready", 70),
            ("created", 70),
        ],
        runtime: Vec::new(),
        lookup: None,
    };
    assert_eq!(
        h.life.step(),
        vec![Event::Started {
            run_id: "j1".to_owned(),
            timeline,
            booked: js_agent::lifecycle::JobBooking {
                cpu_millis: 100,
                memory_bytes: 1 << 30,
                sizing: None,
            },
        }]
    );
}

/// Asserts that a lookup refused while the provider is still pending fails the start
/// `input-unavailable` at that step without waiting for the provider, and that its cleanup
/// removes the scratch and lease directories and releases the provider's source. Catches a
/// refusal held back until the provider answers, and directories or a source left behind.
#[test]
fn a_lookup_refused_while_a_provider_is_pending_fails_the_start_at_once() {
    let (mut h, providers) = admitted("conc-refused");
    h.fake.resolve_answers("j1", missing());
    h.life.step();
    assert_eq!(
        failed_with(&h, "j1").as_deref(),
        Some(refusal::INPUT_UNAVAILABLE)
    );
    for _ in 0..3 {
        h.life.step();
    }
    assert!(!h.scratch_of("j1").exists());
    assert!(!h.dir.path("scratch/.lease-fake-j1").exists());
    assert_eq!(providers.sent("release"), ["provider release tier j1#2"]);
    assert!(logged(&h, &["assemble"]).is_empty());
    assert!(h.fake.created().is_empty());
}

/// Asserts that a lookup refused at the admission itself, with a provider asked before it,
/// fails the start with the refusal's code, makes no directory, and releases the provider's
/// source. Catches directories made for a start whose lookups were refused at once.
#[test]
fn a_lookup_refused_at_the_admission_fails_the_start_before_its_directories() {
    let (mut h, providers) = rig("conc-refused-first", |_| {});
    h.fake.resolve_answers("j1", missing());
    let spec = mixed(&h, "j1");
    h.apply(vec![job_start(spec)]);
    assert_eq!(
        failed_with(&h, "j1").as_deref(),
        Some(refusal::INPUT_UNAVAILABLE)
    );
    assert!(!h.scratch_of("j1").exists());
    for _ in 0..3 {
        h.life.step();
    }
    assert_eq!(providers.sent("release"), ["provider release tier j1#2"]);
}

/// Asserts that a provider that refuses its source after the directories were made fails the
/// start `provider-refused` with the provider's reason, and that the cleanup removes the
/// directories and releases the source. Catches directories left behind by a start that failed while it waited.
#[test]
fn a_provider_refusal_after_the_directories_were_made_removes_them() {
    let (mut h, providers) = admitted("conc-provider-refused");
    assert!(h.scratch_of("j1").is_dir());
    providers.always(
        "tier",
        &mount("j1", 2),
        ProviderAnswer::Refused("no such tree".to_owned()),
    );
    h.life.step();
    assert_eq!(
        failed_with(&h, "j1").as_deref(),
        Some("provider-refused: no such tree")
    );
    for _ in 0..3 {
        h.life.step();
    }
    assert!(!h.scratch_of("j1").exists());
    assert!(!h.dir.path("scratch/.lease-fake-j1").exists());
    assert_eq!(providers.sent("release"), ["provider release tier j1#2"]);
}

/// Asserts that a stop while the providers are pending ends the start killed with no
/// container, removes its directories, releases the source, and never asks for an assembly.
/// Catches a stopped start whose directories, made at the admission, are left behind.
#[test]
fn a_stop_while_the_providers_are_pending_removes_the_directories() {
    let (mut h, providers) = admitted("conc-stop");
    h.apply(vec![Command::JobStop {
        job_id: "j1".to_owned(),
    }]);
    for _ in 0..4 {
        h.life.step();
    }
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::Killed);
    assert!(!h.scratch_of("j1").exists());
    assert!(!h.dir.path("scratch/.lease-fake-j1").exists());
    assert_eq!(providers.sent("release"), ["provider release tier j1#2"]);
    assert!(logged(&h, &["assemble"]).is_empty());
    assert!(h.fake.created().is_empty());
}

/// Asserts that directories that cannot be made at the admission (a file is in the way of
/// the scratch directory) fail the start `scratch-failed` with nothing asked to assemble and
/// no container made. Catches a start that goes on to its assembly without its directories.
#[test]
fn directories_that_cannot_be_made_at_the_admission_fail_the_start() {
    let (mut h, _providers) = rig("conc-dirs", |_| {});
    fs::write(h.scratch_of("j1"), "in the way").unwrap();
    let spec = mixed(&h, "j1");
    h.apply(vec![job_start(spec)]);
    assert_eq!(failed_with(&h, "j1").as_deref(), Some("scratch-failed"));
    h.life.step();
    assert!(logged(&h, &["assemble"]).is_empty());
    assert!(h.fake.created().is_empty());
}

/// Asserts the start of a job whose only source is a provider's mount, so that it has no host
/// path: the runtime is first asked about an empty fs, the directories are made at the
/// admission, and once the provider answers the runtime is asked about the one mount, the
/// assembly is asked for and the job starts with `provided` in its timeline. Catches a start
/// with no host path that waits for a lookup that never begins.
#[test]
fn a_start_with_providers_and_no_host_paths_starts_when_they_answer() {
    let (mut h, providers) = rig("conc-only-providers", |_| {});
    providers.always("tier", &mount("j1", 0), ProviderAnswer::Pending);
    h.apply(vec![job_start(with_mount("j1", MountMode::ReadOnly))]);
    assert_eq!(h.fake.looked_up("j1"), Some(JobFs::default()));
    assert!(h.scratch_of("j1").is_dir());
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    h.clock.advance(10);
    let path = tree(&h, "ro", "t1");
    providers.mounts_at("tier", "j1", 0, &path);
    assert!(h.life.step().is_empty());
    let bound = h.fake.looked_up("j1").expect("looked up");
    assert!(bound.layers.is_empty());
    assert_eq!(bound.mounts.len(), 1);
    let events = h.life.step();
    let [Event::Started { timeline, .. }] = events.as_slice() else {
        panic!("one start: {events:?}");
    };
    assert_eq!(
        timeline.steps,
        [
            ("taken", 0),
            ("admitted", 0),
            ("dirs", 0),
            ("provided", 10),
            ("resolved", 10),
            ("ready", 10),
            ("created", 10),
        ]
    );
}

/// Asserts that when a job that ended and awaits its cleanup is listed before a start whose
/// lookups have answered, one step asks the runtime to resolve and then to assemble the
/// start before it removes the older job's container, that the removal and the rest of the
/// older job's cleanup still complete in that step, and that the start is created at the
/// next step. Catches a waiting start held behind the cleanup of the jobs that were
/// admitted before it.
#[test]
fn a_waiting_start_is_advanced_before_the_cleanup_of_older_jobs() {
    let (mut h, _providers) = rig("conc-before-cleanup", |_| {});
    h.start(job("old"));
    h.fake.exit("old", 0);
    h.fake.resolve_answers("j1", FsPrepared::Pending);
    let spec = layered(&h, "j1");
    h.apply(vec![job_start(spec)]);
    h.fake.resolve_answers("j1", FsPrepared::Ready);
    let before = h.log.borrow().len();
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result old".to_owned()], cleanup("old")].concat(),
        "the older job is cleaned in the same step"
    );
    let log = h.log.borrow()[before..].to_vec();
    let position = |prefix: &str| {
        log.iter()
            .position(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix} is not in {log:?}"))
    };
    assert!(position("resolve j1") < position("assemble j1"), "{log:?}");
    assert!(position("assemble j1") < position("remove"), "{log:?}");
    assert!(h.fake.gone("old"));
    assert!(!h.scratch_of("old").exists());
    assert_eq!(names(&h.life.step()), ["started j1"]);
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
}
