//! A restart of an agent that was given node-local providers
//! (`docs/design/mount-providers.md`, section 4, step 7): the sources of the runs it adopted
//! stay with them, those of every other run it wrote a record of are released, and each
//! provider is swept.

use std::fs;

use js_agent::lifecycle::{Event, ProviderAnswer, label};

use super::fake_providers::{FakeProviders, WAIT_MS, mount, rig, run_one, with_layer};
use super::per_job::job_start;
use super::{Harness, names};

/// Steps `h` four times and returns every event they gave, as `names` has them.
fn steps(h: &mut Harness) -> Vec<String> {
    (0..4).flat_map(|_| names(&h.life.step())).collect()
}

/// Gives the restarted lifecycle of `h` the providers `providers` once more, as the worker
/// does after adopting, and returns what it found.
fn attach(h: &mut Harness, providers: &FakeProviders) -> Vec<Event> {
    h.life
        .attach_providers(Box::new(providers.clone()), WAIT_MS)
}

/// The labels of a container that books 1 CPU and 1 GiB for the run `run`, with the CPU
/// said in words when `broken`, so that no job can stand for it.
fn labels(run: &str, broken: bool) -> Vec<(&'static str, &str)> {
    let cpu = if broken { "lots" } else { "1000" };
    vec![
        (label::RUN, run),
        (label::LEASE, "lease-old"),
        (label::CPU_MILLIS, cpu),
        (label::MEMORY_BYTES, "1073741824"),
        (label::DISK_BYTES, "0"),
        (label::GPUS, ""),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
    ]
}

/// Asserts that an agent that crashed with one run running and another still waiting on its
/// provider, and restarted, sweeps its provider keeping the run it adopted, releases the
/// source of the run it did not (and removes its record), keeps the adopted run's record, and
/// releases the adopted run's source when that run ends; and that a record that cannot be read
/// and one that names a provider the agent was not given are removed and reported, a record
/// whose write did not finish is removed without a word, and a start that comes after the
/// restart is not held by the sweep once it is answered. Catches a source of a crashed run
/// held at the provider for ever, a run released while it still runs, and a sweep that keeps
/// nothing.
#[test]
fn a_restart_releases_the_sources_of_runs_it_did_not_adopt() {
    let (mut h, providers) = rig("prov-restart", |_| {});
    run_one(&mut h, &providers, "j1");
    let down = ProviderAnswer::Unavailable("not ready".to_owned());
    providers.always("tier", &mount("j2", 0), down);
    h.apply(vec![job_start(with_layer("j2"))]);
    let dir = h.dir.path("state/providers");
    assert!(dir.join("j1.json").is_file() && dir.join("j2.json").is_file());
    fs::write(dir.join("garbage.json"), "not json").unwrap();
    fs::write(dir.join("stale.json.tmp"), "{").unwrap();
    let elsewhere = r#"{"sources":[{"provider":"elsewhere","index":0}]}"#;
    fs::write(dir.join("ghost.json"), elsewhere).unwrap();

    // The agent's process ends; the new one adopts what it finds and then is given its
    // providers.
    let adopted = h.restart().expect("the runtime lists its containers");
    assert_eq!(names(&adopted), ["adopted j1"]);
    let events = attach(&mut h, &providers);
    assert_eq!(
        names(&events),
        [
            "finding - provider-record-failed",
            "finding - provider-record-failed"
        ]
    );
    let Event::Finding(unreadable) = &events[0] else {
        panic!("a finding");
    };
    assert!(
        unreadable
            .message
            .starts_with("the provider record garbage.json: "),
        "{}",
        unreadable.message
    );
    let Event::Finding(unknown) = &events[1] else {
        panic!("a finding");
    };
    assert_eq!(
        unknown.message,
        "the provider record of run ghost names elsewhere, which this agent was not given: \
         those sources are left to the provider's own collection"
    );
    assert_eq!(
        providers.sent("sweep"),
        [
            "provider sweep tier keep=[]",
            "provider sweep tier keep=[\"j1\"]"
        ]
    );
    assert_eq!(
        providers.sent("release"),
        ["provider release tier j2#0"],
        "the run that was not adopted; the adopted one still holds its source"
    );
    assert!(dir.join("j1.json").is_file());
    for gone in ["j2.json", "garbage.json", "stale.json.tmp", "ghost.json"] {
        assert!(!dir.join(gone).exists(), "{gone}");
    }

    // The adopted run keeps its source until it ends.
    h.fake.exit("j1", 0);
    steps(&mut h);
    assert_eq!(
        providers.sent("release"),
        ["provider release tier j2#0", "provider release tier j1#0"]
    );
    assert!(!dir.join("j1.json").exists());
}

/// Asserts that a container the restart could not remove keeps the sources of its run (the
/// sweep names the run, no release is asked), and releases them once a later attempt has
/// removed the container. Catches a source released from under a container that may still
/// run, and one never released because no job stands for the container.
#[test]
fn a_container_that_could_not_be_removed_keeps_its_sources_until_it_is_gone() {
    let (mut h, providers) = rig("prov-stuck", |_| {});
    let dir = h.dir.path("state/providers");
    fs::create_dir_all(&dir).unwrap();
    let record = r#"{"sources":[{"provider":"tier","index":0}]}"#;
    fs::write(dir.join("stuck.json"), record).unwrap();
    h.fake.leak(&labels("stuck", true), 0);
    h.fake.fail("remove", 1);
    h.restart().expect("the runtime lists its containers");
    attach(&mut h, &providers);
    assert_eq!(
        providers.sent("sweep").last().map(String::as_str),
        Some("provider sweep tier keep=[\"stuck\"]")
    );
    assert!(providers.sent("release").is_empty());
    assert!(dir.join("stuck.json").is_file());
    h.life.step();
    assert_eq!(providers.sent("release"), ["provider release tier stuck#0"]);
    assert!(!dir.join("stuck.json").exists());
}

/// Asserts that a container the restart could not remove, whose labels name no run, holds no
/// source, and that one that is removed later asks nothing of any provider. Catches a release
/// asked for a run nobody named.
#[test]
fn a_container_that_names_no_run_is_removed_without_a_release() {
    let (mut h, providers) = rig("prov-nameless", |_| {});
    let mut nameless = labels("x", true);
    nameless.retain(|(key, _)| *key != label::RUN);
    h.fake.leak(&nameless, 0);
    h.fake.fail("remove", 1);
    h.restart().expect("the runtime lists its containers");
    attach(&mut h, &providers);
    h.life.step();
    assert!(h.fake.existing().is_empty(), "removed at the next step");
    assert!(providers.sent("release").is_empty());
}

/// Asserts that a state root whose `providers` is not a directory is reported when the
/// providers are given (`provider-record-failed`) and the agent goes on, sweeping as usual.
/// Catches an agent that stops, or says nothing, when it cannot list its records.
#[test]
fn records_that_cannot_be_listed_are_reported_and_the_agent_goes_on() {
    let (mut h, providers) = rig("prov-unlisted", |_| {});
    fs::write(h.dir.path("state/providers"), "not a directory").unwrap();
    h.restart().expect("the runtime lists its containers");
    let events = attach(&mut h, &providers);
    assert_eq!(names(&events), ["finding - provider-record-failed"]);
    let Event::Finding(found) = &events[0] else {
        panic!("a finding");
    };
    assert!(
        found.message.starts_with("the provider records: "),
        "{}",
        found.message
    );
    assert_eq!(providers.sent("sweep").len(), 2);
}
