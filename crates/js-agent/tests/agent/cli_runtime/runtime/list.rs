//! The listing of the agent's containers: what it finds, the age it reports, and its
//! failures.

use js_agent::lifecycle::{Found, Runtime, label};

use super::super::scripted::Scripted;
use super::super::{HOST, docker, open, open_at, open_with, settings, thirty_seconds_on};
use super::{start, strings};

fn by_run(found: &[Found]) -> Vec<(String, u64)> {
    let mut runs: Vec<(String, u64)> = found
        .iter()
        .map(|f| (f.labels[label::RUN].clone(), f.running_ms))
        .collect();
    runs.sort();
    runs
}

/// Asserts that the list finds only the containers that carry the run label and this
/// agent's label: another agent's container on the same runtime, a container with only
/// the run label (no agent) and one with only the agent label are all left out; and the
/// `ps` call asks the runtime to filter on both labels. It catches an agent that adopts
/// and then stops or removes another agent's job.
#[test]
fn ps_filter_finds_only_labelled() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "mine");
    let others = [
        &[(label::RUN, "theirs"), (label::AGENT, "h-other")][..],
        &[(label::RUN, "unowned")][..],
        &[(label::AGENT, HOST)][..],
        &[("team", "ml")][..],
    ];
    for labels in others {
        world.leak(labels);
    }
    let found = rt.list_labelled().unwrap();
    assert_eq!(by_run(&found), [("mine".to_owned(), 0)]);
    let ps = world.calls_of("ps").pop().unwrap();
    assert_eq!(
        ps,
        strings(&[
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            "label=jobscheduler.dev/run-id",
            "--filter",
            "label=jobscheduler.dev/agent=h-0123456789abcdef",
        ])
    );
    assert_eq!(found[0].labels["team"], "ml", "the labels come back whole");
    assert_eq!(found[0].id.0, world.id_of("mine").unwrap());
    let inspect = world.calls_of("inspect").pop().unwrap();
    assert_eq!(inspect[4], "{{.State.Status}} {{json .Config.Labels}}");
}

/// Asserts that listing never removes anything: a container that was created and never
/// started is listed beside the running ones, even while every removal fails, and no `rm`
/// is made. (The lifecycle evicts it, which books it while the removal fails; see the
/// lifecycle tests.) Podman's own name for the state is treated the same. It catches a
/// listing that one container that cannot be removed makes fail, so that nothing is
/// adopted at a restart.
#[test]
fn a_container_never_started_is_listed_and_not_removed() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "live");
    let orphan = world.leak_created(&[
        (label::RUN, "orphan"),
        (label::AGENT, HOST),
        (label::CREATED_MS, "1000000"),
    ]);
    world.fail("rm", 1000);
    let found = rt.list_labelled().unwrap();
    assert_eq!(
        by_run(&found),
        [("live".to_owned(), 0), ("orphan".to_owned(), 0)]
    );
    let marked = |run: &str| found.iter().find(|f| f.labels[label::RUN] == run);
    assert!(!marked("live").unwrap().never_started);
    assert!(marked("orphan").unwrap().never_started);
    assert!(world.ids().contains(&orphan));
    assert!(world.calls_of("rm").is_empty());

    let podman = Scripted::new("podman version 5.2.1");
    let mut rt = open_with(&podman, "linux", settings());
    let id = podman.leak(&[(label::RUN, "p"), (label::AGENT, HOST)]);
    podman.say("inspect", "configured {}\n");
    let listed = rt.list_labelled().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].never_started);
    assert!(podman.ids().contains(&id));
    assert!(podman.calls_of("rm").is_empty());
}

/// Asserts that the age of a listed container is the time since the agent made it, on
/// the wall clock: a runtime opened 30 seconds later reports 30000 ms; a clock that went
/// back reports 0 and not a huge number, however far back (a creation time of the
/// largest number) and a clock far ahead does not overflow; and a container without a
/// creation time (or with one that is not a number) is as old as it can be, so its job is
/// over its limit and not given a fresh one. The stamp is the wall clock because the
/// engines print their own times in formats that differ; a step of the clock therefore
/// moves the age (documented). It catches a restart that gives every job a new wall time
/// and arithmetic that wraps.
#[test]
fn the_age_of_a_listed_container_comes_from_its_creation_time() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "r1");
    let later = open_at(&world, "linux", thirty_seconds_on)
        .list_labelled()
        .unwrap();
    assert_eq!(by_run(&later), [("r1".to_owned(), 30_000)]);
    fn back() -> u64 {
        10
    }
    let earlier = open_at(&world, "linux", back).list_labelled().unwrap();
    assert_eq!(by_run(&earlier), [("r1".to_owned(), 0)]);
    fn far_ahead() -> u64 {
        u64::MAX
    }
    let ahead = open_at(&world, "linux", far_ahead).list_labelled().unwrap();
    assert_eq!(by_run(&ahead), [("r1".to_owned(), u64::MAX - 1_000_000)]);
    let max = u64::MAX.to_string();
    world.leak(&[
        (label::RUN, "from-the-future"),
        (label::AGENT, HOST),
        (label::CREATED_MS, &max),
    ]);
    world.leak(&[(label::RUN, "no-time"), (label::AGENT, HOST)]);
    world.leak(&[
        (label::RUN, "bad-time"),
        (label::AGENT, HOST),
        (label::CREATED_MS, "soon"),
    ]);
    let all = rt.list_labelled().unwrap();
    assert_eq!(
        by_run(&all),
        [
            ("bad-time".to_owned(), u64::MAX),
            ("from-the-future".to_owned(), 0),
            ("no-time".to_owned(), u64::MAX),
            ("r1".to_owned(), 0),
        ]
    );
}

/// Asserts the list's failures: `ps` failing is an error (the agent must not say it
/// holds nothing), an id that `ps` prints which is not an id is an error and never goes
/// into an argument, an answer that is not a state and the labels as JSON, or labels
/// that are not a JSON object of text, are an error, and `null` (a container with no
/// labels) reads as none. A container that disappears between the two calls is skipped.
/// It catches a list that silently drops containers it could not read and so lets the
/// server requeue running work.
#[test]
fn list_failures_are_errors_and_a_vanished_container_is_skipped() {
    let world = docker();
    let mut rt = open(&world);
    start(&mut rt, &world, "r1");
    world.fail("ps", 1);
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
    world.say_once("ps", "--all\n");
    let said = rt.list_labelled().unwrap_err();
    assert_eq!(said.0, "not a container id");

    for printed in [
        "running [1,2]",
        "running {\"a\":1}",
        "running garbage",
        "running ",
        "garbage",
        "",
    ] {
        world.say("inspect", printed);
        let said = rt.list_labelled().unwrap_err();
        assert_eq!(said.0, "inspect: unreadable output", "{printed:?}");
    }
    world.say("inspect", "running null\n");
    let found = rt.list_labelled().unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].labels.is_empty());

    let world = docker();
    let mut rt = open(&world);
    let id2 = start(&mut rt, &world, "r2");
    world.fail("inspect", 1);
    // The listing saw the container; it is gone by the time it is read.
    world.say_once("ps", &format!("{}\n", id2.0));
    world.vanish("r2");
    assert_eq!(rt.list_labelled().unwrap(), []);
}

/// Asserts that the ids-only listing asks `ps` with the same two label filters, reads no
/// container (so a container whose inspect output is unreadable is still listed), and
/// fails when `ps` fails. It catches a rescue listing that depends on every inspect.
#[test]
fn list_ids_reads_nothing_and_fails_with_ps() {
    let world = docker();
    let mut rt = open(&world);
    let one = start(&mut rt, &world, "r1");
    let two = start(&mut rt, &world, "r2");
    world.leak(&[(label::RUN, "theirs"), (label::AGENT, "h-other")]);
    let reads = world.calls_of("inspect").len();
    world.say("inspect", "garbage");
    assert!(rt.list_labelled().is_err());
    let mut ids = rt.list_ids().unwrap();
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want = vec![one, two];
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(ids, want);
    let after = world.calls_of("inspect").len();
    assert_eq!(after, reads + 1, "only the failing list_labelled read");
    world.fail("ps", 1);
    let said = rt.list_ids().unwrap_err();
    assert_eq!(said.0, "ps: /bin/sh: exit status: 1");
}
