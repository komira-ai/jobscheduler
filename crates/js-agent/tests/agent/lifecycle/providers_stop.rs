//! A run whose input is a provider's tree while its agent stops: drains, is parked for the
//! next process, and is adopted by it. The tree of a provider is read lazily (a file is
//! filled when the job first opens it), so the run needs its source held at the provider and
//! its mounts in place for as long as it runs, whatever its agent does meanwhile.

use js_agent::lifecycle::ProviderAnswer;

use super::fake_providers::{WAIT_MS, rig, run_one, tree, with_layer};
use super::per_job::job_start;
use super::{Harness, names};

/// The lines of `h`'s log that ask its runtime to take apart the file system of `run`, or to
/// remove it.
fn touched(h: &Harness, run: &str) -> Vec<String> {
    let discard = format!("discard {run}");
    h.log
        .borrow()
        .iter()
        .filter(|line| **line == discard || line.starts_with("remove ") && line.contains(run))
        .cloned()
        .collect()
}

/// Asserts that a run reading a provider's tree keeps its source and its mounts through all
/// that an agent's stop does to it: half an hour of a drain's steps (the drain waits for the
/// run and asks nothing of it), the park for a handover (the run counts as quiet), the
/// restart, the adoption, the sweep of the next process (which keeps the run), and a start
/// the next process takes right after the sweep; and that its source is released, once, only
/// when it ends. Catches a release, a sweep that misses the run, or a removal of its file
/// system made while it still runs and has inputs left to open.
#[test]
fn a_run_keeps_its_provider_source_through_its_agents_drain_park_and_adoption() {
    let (mut h, providers) = rig("prov-stop", |_| {});
    // A job of four hours, so the half hour below is the drain's and not its wall limit.
    let path = tree(&h, "ro", "j1-0");
    providers.mounts_at("tier", "j1", 0, &path);
    let mut long = with_layer("j1");
    long.wall_ms = 4 * 3_600_000;
    h.apply(vec![job_start(long)]);
    for _ in 0..4 {
        h.life.step();
    }
    assert_eq!(h.fake.existing().len(), 1, "it runs");

    // The drain: the agent takes no start and steps until its runs end.
    for _ in 0..30 {
        h.clock.advance(60_000);
        h.life.step();
    }
    assert!(h.life.quiet_for_handover(), "parked: nothing under way");
    assert!(providers.sent("release").is_empty(), "drain");
    assert!(touched(&h, "j1").is_empty(), "drain");

    // The process ends; the next one adopts the run and is given its providers.
    let adopted = h.restart().expect("the runtime lists its containers");
    assert_eq!(names(&adopted), ["adopted j1"]);
    assert!(
        h.life
            .attach_providers(Box::new(providers.clone()), WAIT_MS)
            .is_empty()
    );
    assert_eq!(
        providers.sent("sweep").last().map(String::as_str),
        Some("provider sweep tier keep=[\"j1\"]")
    );

    // A start right after the restart, while the adopted run still reads its tree.
    let path = tree(&h, "ro", "j2-0");
    providers.mounts_at("tier", "j2", 0, &path);
    h.apply(vec![job_start(with_layer("j2"))]);
    for _ in 0..4 {
        h.life.step();
    }
    assert!(providers.sent("release").is_empty(), "adoption");
    assert!(touched(&h, "j1").is_empty(), "adoption");
    assert_eq!(h.fake.existing().len(), 2, "both run");

    h.fake.exit("j1", 0);
    for _ in 0..4 {
        h.life.step();
    }
    assert_eq!(providers.sent("release"), ["provider release tier j1#0"]);
}

/// Asserts that a start the next process takes while its provider has not answered the sweep
/// yet sends nothing to the provider until the sweep is answered, so the sweep, which names
/// only the adopted runs, cannot release the source the start then prepares. Catches a start
/// right after an agent's restart whose tree the restart's own sweep takes away.
#[test]
fn a_start_right_after_a_restart_is_prepared_only_after_the_sweep() {
    let (mut h, providers) = rig("prov-stop-sweep", |_| {});
    run_one(&mut h, &providers, "j1");
    h.restart().expect("the runtime lists its containers");
    let keep = js_agent::lifecycle::ProviderCall::Sweep {
        keep: vec![("j1".to_owned(), 1)],
    };
    providers.then("tier", &keep, ProviderAnswer::Pending);
    h.life
        .attach_providers(Box::new(providers.clone()), WAIT_MS);
    let path = tree(&h, "ro", "j2-0");
    providers.mounts_at("tier", "j2", 0, &path);
    h.apply(vec![job_start(with_layer("j2"))]);
    h.life.step();
    assert!(
        !providers
            .sent("prepare")
            .iter()
            .any(|line| line.contains("j2#0")),
        "{:?}",
        providers.sent("prepare")
    );
    // The sweep is answered: the start goes on.
    providers.then("tier", &keep, ProviderAnswer::Done(None));
    for _ in 0..4 {
        h.life.step();
    }
    assert!(
        providers
            .sent("prepare")
            .iter()
            .any(|line| line.contains("j2#0"))
    );
    let sweep_at = h
        .log
        .borrow()
        .iter()
        .rposition(|line| line.starts_with("provider sweep tier"))
        .expect("a sweep was sent");
    let prepare_at = h
        .log
        .borrow()
        .iter()
        .position(|line| line.starts_with("provider prepare tier j2#0"))
        .expect("the start was prepared");
    assert!(sweep_at < prepare_at, "{:?}", h.log.borrow());
    assert!(providers.sent("release").is_empty());
}
