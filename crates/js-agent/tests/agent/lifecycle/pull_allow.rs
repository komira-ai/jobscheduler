//! `--container-pull-allow` at admission: a start whose image is outside the list is
//! refused before anything is created.

use js_agent::pull_allow::PullAllow;
use js_agent_wire::Outcome;

use super::{Harness, Options, job, names};

const DIGEST: &str = "@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn with_list(name: &str, entries: &[&str]) -> Harness {
    let mut h = Harness::new(name);
    let owned: Vec<String> = entries.iter().map(|entry| (*entry).to_owned()).collect();
    h.config.pull_allow = PullAllow::parse(&owned).unwrap();
    h.restart().unwrap();
    h
}

fn job_of(run: &str, image: &str) -> js_agent_wire::JobSpec {
    let mut spec = job(run);
    spec.image = format!("{image}{DIGEST}");
    spec
}

/// Asserts that a start whose image is under an entry is created, and that one outside the
/// list is refused `invalid-image` (a refusal that is not retried) with a `failed_to_start`
/// result, no container and no scratch directory. It catches an allow-list that is read
/// but not enforced, and one that refuses only after the engine was called (the pull).
#[test]
fn an_image_outside_the_list_is_refused_and_nothing_is_created() {
    let mut h = with_list("pull-allow-deny", &["registry.example/job"]);
    let events = names(&h.start(job_of("out", "evil.example/job/app")));
    assert_eq!(events, ["refused out invalid-image"]);
    assert!(h.fake.created().is_empty(), "no create, so no pull");
    assert!(!h.scratch_of("out").exists(), "no scratch directory either");
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("invalid-image"));
    // The neighbour that a text prefix would admit is outside too.
    let near = names(&h.start(job_of("near", "registry.example/job-evil/app")));
    assert_eq!(near, ["refused near invalid-image"]);
    assert!(h.fake.created().is_empty());
    let inside = names(&h.start(job_of("in", "registry.example/job/app")));
    assert_eq!(inside, ["started in"]);
    assert_eq!(h.fake.created().len(), 1);
}

/// Asserts that with no entry any image is created, as an agent without the flag does. It
/// catches a default that refuses.
#[test]
fn without_entries_every_image_is_created() {
    let mut h = Harness::new("pull-allow-none");
    for (run, image) in [("a", "evil.example/anything/at/all"), ("b", "ubuntu")] {
        assert_eq!(
            names(&h.start(job_of(run, image))),
            [format!("started {run}")]
        );
    }
    assert_eq!(h.fake.created().len(), 2);
}

/// Asserts that a refused image books nothing: starts that each ask for the whole machine
/// are refused for their image, one after another, and a start from the list that asks for
/// the whole machine is then admitted. It catches a refusal taken after the booking or the
/// slot was made, which would leave the machine full of runs that never existed.
#[test]
fn a_refused_image_books_nothing() {
    let mut h = with_list("pull-allow-books", &["registry.example/job"]);
    let whole = |run: &str, image: &str| {
        let mut spec = job_of(run, image);
        spec.limits.cpu_millis = 4000;
        spec
    };
    for run in ["r1", "r2", "r3"] {
        let events = names(&h.start(whole(run, "evil.example/x")));
        assert_eq!(events, [format!("refused {run} invalid-image")]);
    }
    let events = names(&h.start(whole("ok", "registry.example/job/app")));
    assert_eq!(events, ["started ok"]);
}

/// Asserts that a host-isolation job is not subject to the list: with a list that does not
/// cover its image it is not refused `invalid-image`. It catches the check moved ahead of
/// the isolation test, which would refuse jobs that pull nothing.
#[test]
fn a_host_job_is_not_subject_to_the_list() {
    let mut h = Harness::with(
        "pull-allow-host",
        Options {
            allow_host_jobs: true,
            ..Options::default()
        },
    );
    h.config.pull_allow = PullAllow::parse(&["registry.example/job".to_owned()]).unwrap();
    h.restart().unwrap();
    let mut spec = job_of("h", "evil.example/x");
    spec.isolation = js_agent_wire::Isolation::Host;
    let events = names(&h.start(spec));
    assert!(
        events.iter().all(|event| !event.contains("invalid-image")),
        "{events:?}"
    );
}

/// Asserts that a short name is refused `invalid-image` at admission when the list is not
/// empty, and a qualified `docker.io` name that is listed is created. It catches a short
/// name that the engine would resolve elsewhere being created.
#[test]
fn a_short_name_is_refused_with_a_list_and_a_qualified_one_is_created() {
    let mut h = with_list("pull-allow-short", &["docker.io/library/"]);
    let events = names(&h.start(job_of("short", "ubuntu")));
    assert_eq!(events, ["refused short invalid-image"]);
    assert!(h.fake.created().is_empty());
    let events = names(&h.start(job_of("full", "docker.io/library/ubuntu")));
    assert_eq!(events, ["started full"]);
}
