//! The end of a run that holds sources of node-local providers
//! (`docs/design/mount-providers.md`, section 4, step 6): each source is released once, after
//! the run's file system is taken apart, on every path a run can end by, and a release that
//! fails never taints the host or holds the run's slot.

use js_agent::lifecycle::{Event, ProviderAnswer};
use js_agent_wire::{Command, HeldState, MountMode, Outcome};

use super::fake_providers::{
    IMAGE, mount, prepare, release, rig, run_one, tree, with_layer, with_layer_and_mount,
};
use super::per_job::{job_start, listing};
use super::{Harness, names};

/// Steps `h` six times and returns every event they gave, as `names` has them.
fn steps(h: &mut Harness) -> Vec<String> {
    (0..6).flat_map(|_| names(&h.life.step())).collect()
}

/// Stops the job `job_id`.
fn stop(h: &mut Harness, job_id: &str) {
    h.apply(vec![Command::JobStop {
        job_id: job_id.to_owned(),
        reason: None,
    }]);
}

/// Asserts that a run's one source is prepared once and released once, after the file system
/// of the run was discarded, and that its record is gone, whichever way the run ends: its
/// container exits, its start fails (a container that cannot be created), it is stopped while
/// its provider still has not given the directory, its image cannot be had, it runs past its
/// wall time and is killed, and it is stopped while it runs. Catches a source left held by an
/// end path that skips the cleanup, a release asked twice, and a release before the mounts
/// that use the tree are taken apart.
#[test]
fn release_follows_every_end_path_once() {
    let paths = [
        "exit",
        "create-fails",
        "stop-while-providing",
        "image-fails",
        "wall-time",
        "stop-while-running",
    ];
    for path in paths {
        let (mut h, providers) = rig(&format!("prov-end-{path}"), |_| {});
        let record = h.dir.path("state/providers/j1.json");
        match path {
            "exit" => {
                run_one(&mut h, &providers, "j1");
                assert!(record.is_file());
                h.fake.exit("j1", 0);
            }
            "create-fails" => {
                h.fake.fail("create", 1);
                providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t"));
                h.apply(vec![job_start(with_layer("j1"))]);
            }
            "stop-while-providing" => {
                let down = ProviderAnswer::Unavailable("not ready".to_owned());
                providers.always("tier", &mount("j1", 0), down);
                h.apply(vec![job_start(with_layer("j1"))]);
                h.life.step();
                assert!(record.is_file());
                stop(&mut h, "j1");
            }
            "image-fails" => {
                h.fake.pull_fails(IMAGE, "no such image");
                h.apply(vec![job_start(with_layer("j1"))]);
            }
            "wall-time" => {
                run_one(&mut h, &providers, "j1");
                h.clock.advance(60_000);
            }
            _ => {
                run_one(&mut h, &providers, "j1");
                stop(&mut h, "j1");
            }
        }
        let events = steps(&mut h);
        assert!(
            events.contains(&"freed j1".to_owned()),
            "{path}: {events:?}"
        );
        assert_eq!(providers.sent("prepare").len(), 1, "{path}: prepared once");
        assert_eq!(
            providers.sent("release"),
            ["provider release tier j1#0"],
            "{path}: released once"
        );
        let log = h.log.borrow().clone();
        let discard = log.iter().position(|line| line == "discard j1");
        let released = log
            .iter()
            .position(|line| line == "provider release tier j1#0");
        assert!(discard < released, "{path}: {log:?}");
        assert!(discard.is_some(), "{path}: {log:?}");
        assert!(!record.exists(), "{path}: the record is removed");
        assert!(h.life.busy_slots().is_empty(), "{path}");
    }
}

/// Asserts that a start stopped while its provider has not even answered `Prepare` ends at
/// once, without waiting for the call in flight, with the stop's result and no container, and
/// that its source is released all the same (the provider ends what its `Prepare` began).
/// Catches a stop that waits for a provider that is down, and a source left prepared.
#[test]
fn a_stop_while_preparing_releases_the_source() {
    let (mut h, providers) = rig("prov-stop-preparing", |_| {});
    let call = prepare("j1", 0, "k1", MountMode::ReadOnly);
    providers.always("tier", &call, ProviderAnswer::Pending);
    h.apply(vec![job_start(with_layer("j1"))]);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert!(
        providers.sent("release").is_empty(),
        "nothing to release yet"
    );
    stop(&mut h, "j1");
    h.life.step();
    let result = listing(&h, "j1").result.expect("ended at the next step");
    assert_eq!(result.outcome, Outcome::Killed);
    assert!(h.fake.created().is_empty());
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
    let log = h.log.borrow().clone();
    let prepared = log
        .iter()
        .position(|line| line.starts_with("provider prepare"));
    let released = log
        .iter()
        .position(|line| line.starts_with("provider release"));
    assert!(prepared < released, "{log:?}");
}

/// Asserts that a release a provider does not answer never taints the host and never holds
/// the run's slot or its checks (the run is freed in the step it ended in), is asked again
/// for a minute from the end of the run (after a wait that grows with each failure, so not at
/// every step), and is then given up with one `provider-release-failed` finding that names
/// the sources left and is not repeated, after which it is not asked again and its record is
/// gone. Catches a release that taints, one that holds up the next job, one that is retried
/// for ever or at every step, and a finding repeated at every step.
#[test]
fn a_failed_release_never_taints_the_host() {
    let (mut h, providers) = rig("prov-release-fails", |_| {});
    run_one(&mut h, &providers, "j1");
    let down = ProviderAnswer::Unavailable("socket missing".to_owned());
    providers.always("tier", &release("j1", 0), down);
    h.fake.exit("j1", 0);
    let events = (0..4)
        .flat_map(|_| names(&h.life.step()))
        .collect::<Vec<_>>();
    assert!(
        events.contains(&"checks passed j1".to_owned()),
        "{events:?}"
    );
    assert!(events.contains(&"freed j1".to_owned()), "{events:?}");
    assert!(
        events.contains(&"finding j1 provider-backoff".to_owned()),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| event.starts_with("tainted")),
        "{events:?}"
    );
    assert!(!h.life.is_tainted());
    assert!(h.life.busy_slots().is_empty(), "the slot is free");
    assert_eq!(
        providers.sent("release").len(),
        1,
        "asked once: the steps that follow wait"
    );
    assert!(h.dir.path("state/providers/j1.json").is_file());

    h.clock.advance(59_999);
    assert_eq!(names(&h.life.step()), ["finding j1 provider-backoff"]);
    assert_eq!(providers.sent("release").len(), 2);
    h.clock.advance(1);
    let events = h.life.step();
    assert_eq!(names(&events), ["finding j1 provider-release-failed"]);
    let Event::Finding(found) = &events[0] else {
        panic!("a finding");
    };
    assert_eq!(
        found.message,
        "not released within a minute: tier source 0; left to the providers' own collection"
    );
    assert_eq!(
        providers.sent("release").len(),
        2,
        "the last failure's wait has not passed"
    );
    assert!(!h.dir.path("state/providers/j1.json").exists());
    assert!(names(&h.life.step()).is_empty(), "reported once");
    h.clock.advance(60_000);
    assert!(names(&h.life.step()).is_empty());
    assert_eq!(providers.sent("release").len(), 2, "given up");
    assert!(!h.life.is_tainted());
}

/// Asserts that a run whose two sources both release is released source by source: one that
/// answers is not asked again while the other still fails, and the record stays until the
/// last is released, and a release that answers `refused` is retried like one that is
/// unavailable. Catches a record removed with a source still held, and a source released
/// twice.
#[test]
fn a_run_releases_its_sources_one_by_one_and_keeps_its_record_until_the_last() {
    let (mut h, providers) = rig("prov-release-partial", |_| {});
    providers.mounts_at("tier", "j1", 0, &tree(&h, "ro", "t1"));
    providers.mounts_at("tier", "j1", 1, &tree(&h, "ro", "t2"));
    let refused = ProviderAnswer::Refused("busy".to_owned());
    providers.then("tier", &release("j1", 1), refused);
    providers.then("tier", &release("j1", 1), ProviderAnswer::Pending);
    h.apply(vec![job_start(with_layer_and_mount("j1"))]);
    steps(&mut h);
    h.fake.exit("j1", 0);
    let record = h.dir.path("state/providers/j1.json");
    h.life.step();
    assert_eq!(
        providers.sent("release"),
        ["provider release tier j1#0", "provider release tier j1#1"]
    );
    assert!(record.is_file(), "source 1 is still held");
    h.life.step();
    assert_eq!(
        providers.sent("release").len(),
        2,
        "the refusal has it wait"
    );
    // The first wait is at most 1 s.
    h.clock.advance(1_000);
    h.life.step();
    assert!(record.is_file(), "its call is still out");
    h.life.step();
    assert!(!record.exists());
    assert_eq!(
        providers.sent("release"),
        [
            "provider release tier j1#0",
            "provider release tier j1#1",
            "provider release tier j1#1"
        ],
        "source 0 was released once"
    );
}
