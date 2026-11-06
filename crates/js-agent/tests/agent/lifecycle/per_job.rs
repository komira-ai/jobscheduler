//! Starts, stops and listings keyed by job id: the job start runs once on the run start's
//! path with the allocation's booked resources and the job's own wall limit, a stop for a
//! job the agent does not hold is answered with the retired id, and nothing of the job
//! keying shows in the lease and run listing or the other way round.

use js_agent::lifecycle::{Clock, RETIRED_LISTED, label};
use js_agent_wire::{BookedResources, Command, HeldJob, HeldState, JobStartSpec, Outcome, refusal};
use serde_json::json;

use super::{Harness, job, names, reply, reply_json};

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A valid job start for `job_id`: one CPU, 1 GiB, a minute, no GPU.
fn start(job_id: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: format!("alloc-{job_id}"),
        image: IMAGE.to_owned(),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
    }
}

fn job_start(spec: JobStartSpec) -> Command {
    Command::JobStart(Box::new(spec))
}

fn job_stop(job_id: &str) -> Command {
    Command::JobStop {
        job_id: job_id.to_owned(),
    }
}

/// The ids of the jobs listed, in order.
fn listed(h: &Harness) -> Vec<String> {
    h.life
        .held_jobs()
        .into_iter()
        .map(|job| job.job_id)
        .collect()
}

/// The listing of `job_id`, which must be there.
fn listing(h: &Harness, job_id: &str) -> HeldJob {
    let jobs = h.life.held_jobs();
    let found = jobs.into_iter().find(|job| job.job_id == job_id);
    found.expect("the job is listed")
}

/// Applies a reply that acknowledges the result of `job_id` with `ack`.
fn ack_job(h: &mut Harness, job_id: &str, ack: &str) {
    let reply = reply_json(&json!({ "job_acks": [{ "job_id": job_id, "ack": ack }] }));
    h.life.apply(&reply);
}

/// Asserts that the same job start delivered in three consecutive replies creates one
/// container and one listing, and that after the job ended and its result was acknowledged
/// a fourth delivery is skipped as retired and creates nothing. It catches idempotence
/// keyed on anything but the job id, and a retired set that job ids do not reach.
#[test]
fn a_repeated_start_does_not_run_the_job_twice() {
    let mut h = Harness::new("job-repeat");
    let mut events = Vec::new();
    for _ in 0..3 {
        events.extend(names(&h.apply(vec![job_start(start("j1"))])));
    }
    assert_eq!(events, ["started j1"]);
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(listed(&h), ["j1"]);
    h.fake.exit("j1", 0);
    h.life.step();
    ack_job(&mut h, "j1", "ACCEPTED");
    assert!(listed(&h).is_empty());
    let events = h.apply(vec![job_start(start("j1"))]);
    assert_eq!(names(&events), ["skipped j1 FenceRetired"]);
    assert_eq!(h.fake.created().len(), 1);
    assert!(listed(&h).is_empty());
}

/// Asserts that a stop for a job the agent never held puts the id in `retired_jobs`, and
/// keeps it there across steps, a second stop and a reply that acknowledges nothing (a lost
/// heartbeat must not lose the answer); that a start of that job afterwards creates nothing;
/// and that a reply naming the id in `retired_acks` takes it out, while one naming another
/// id leaves it. It catches a stop that is ignored, an id that is not retired, and an answer
/// that is reported once only.
#[test]
fn a_stop_for_an_unlisted_job_retires_the_id() {
    let mut h = Harness::new("job-stop-unlisted");
    assert!(h.apply(vec![job_stop("j9")]).is_empty());
    assert_eq!(h.life.retired_jobs(), ["j9"]);
    h.life.step();
    h.apply(vec![job_stop("j9")]);
    h.life.apply(&reply_json(&json!({ "taint_ack": false })));
    assert_eq!(h.life.retired_jobs(), ["j9"]);
    let events = h.apply(vec![job_start(start("j9"))]);
    assert_eq!(names(&events), ["skipped j9 FenceRetired"]);
    assert!(h.fake.created().is_empty());
    assert!(listed(&h).is_empty());
    h.life
        .apply(&reply_json(&json!({ "retired_acks": ["j8"] })));
    assert_eq!(h.life.retired_jobs(), ["j9"]);
    h.life
        .apply(&reply_json(&json!({ "retired_acks": ["j9"] })));
    assert!(h.life.retired_jobs().is_empty());
    assert!(h.fake.created().is_empty());
}

/// Asserts that two stops for a running job, and a third after the next step, make one
/// stop call to the runtime and one `killed` result, in the job's listing and not in the
/// run results, and that the id is not answered as retired (the job was held). It catches
/// a stop that signals the container on every repeat, a second result, and a held job
/// answered as gone.
#[test]
fn a_stop_for_a_held_job_ends_it_killed_once() {
    let mut h = Harness::new("job-stop-held");
    h.fake.stop_exits(false);
    h.apply(vec![job_start(start("j1"))]);
    h.apply(vec![job_stop("j1"), job_stop("j1")]);
    h.life.step();
    h.apply(vec![job_stop("j1")]);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 1);
    assert_eq!(listing(&h, "j1").result, None, "not exited yet");
    h.fake.exit("j1", 143);
    h.life.step();
    h.apply(vec![job_stop("j1")]);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 1);
    let jobs = h.life.held_jobs();
    assert_eq!(jobs.len(), 1);
    let result = jobs[0].result.clone().expect("a result");
    assert_eq!(result.outcome, Outcome::Killed);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.allocation_id, "alloc-j1");
    assert!(h.life.results().is_empty());
    assert!(h.life.retired_jobs().is_empty());
}

/// Asserts that a finished job's result is in its listing at every heartbeat across five
/// heartbeats with no acknowledgement for it (only for a stranger), that an acknowledgement
/// of a verdict this version does not know keeps it, and that an `ACCEPTED` one takes the job
/// out of the listing; the slot is free once the cleanup passed, as for a run. It catches a
/// result sent once, and one dropped on an unknown verdict.
#[test]
fn a_result_is_resent_until_acknowledged() {
    let mut h = Harness::new("job-resend");
    h.apply(vec![job_start(start("j1"))]);
    h.fake.exit("j1", 3);
    for _ in 0..5 {
        h.life.step();
        ack_job(&mut h, "stranger", "ACCEPTED");
        let listed = listing(&h, "j1");
        assert_eq!(listed.state, HeldState::ExitedUnacked);
        let result = listed.result.expect("the result is sent again");
        assert_eq!(
            (result.outcome, result.exit_code),
            (Outcome::Failed, Some(3))
        );
    }
    ack_job(&mut h, "j1", "A_VERDICT_FROM_THE_FUTURE");
    assert!(
        listing(&h, "j1").result.is_some(),
        "an unknown verdict keeps it"
    );
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(h.life.free().cpu_millis, 4000);
    ack_job(&mut h, "j1", "ACCEPTED");
    assert!(listed(&h).is_empty());
    assert!(h.life.busy_slots().is_empty());
}

/// Asserts that a job start gives the container exactly the booked CPU, memory, disk and
/// GPUs and the job's wall limit, labelled with the job id, the allocation id and the job
/// keying; that the start is applied although the clock moved between the reply's arrival
/// and its application; and that the job is stopped `timed_out` when the wall limit has
/// passed counted from the container's creation (one millisecond earlier it is left alone).
/// It catches `valid_for_ms` applied to a job start, a wall clock started at the reply's
/// arrival, and limits taken from anywhere but the allocation.
#[test]
fn a_job_start_uses_the_booked_resources_and_wall_limit() {
    let mut h = Harness::new("job-booked");
    h.fake.create_takes(5_000);
    let mut spec = start("j1");
    spec.booked = BookedResources {
        cpu_millis: 1500,
        memory_bytes: 2 << 30,
        disk_bytes: Some(1 << 30),
        gpu_indices: vec![1],
    };
    spec.wall_ms = 10_000;
    let received = h.clock.now_ms();
    h.clock.advance(3_000);
    let events = h.life.apply_at(&reply(vec![job_start(spec)]), received);
    assert_eq!(names(&events), ["started j1"]);
    let made = &h.fake.created()[0];
    assert_eq!(made.run_id, "j1");
    assert_eq!((made.cpu_millis, made.memory_bytes), (1500, 2 << 30));
    assert_eq!(made.gpu_indices, [1]);
    assert_eq!(made.pids, None);
    let labels = &made.labels;
    assert_eq!(labels[label::RUN], "j1");
    assert_eq!(labels[label::LEASE], "alloc-j1");
    assert_eq!(labels[label::WALL_MS], "10000");
    assert_eq!(labels[label::DISK_BYTES], (1u64 << 30).to_string());
    assert_eq!(labels[label::GPUS], "1");
    assert_eq!(labels[label::KEYING], "job");
    assert_eq!(h.life.free().cpu_millis, 2500);
    // Made at 8 s (3 s of delivery, 5 s of creation): the limit ends at 18 s.
    h.clock.advance(9_999);
    h.life.step();
    assert!(h.fake.stops().is_empty());
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    h.clock.advance(1);
    h.life.step();
    assert_eq!(h.fake.stops().len(), 1);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::TimedOut);
}

/// Asserts that after a job start and a run start the run listing has only the run and the
/// job listing only the job, with each result only in its own keying; that a `Stop`,
/// `Cancel` or `result_acks` entry naming the job, and a `JobStop` or `job_acks` entry naming
/// the run, leave the other alone. It catches the two keyings leaking into each other (a
/// lease server would see an unknown run id and answer `Cancel`).
#[test]
fn job_keyed_jobs_do_not_appear_in_the_run_listing() {
    let mut h = Harness::new("job-keyings");
    h.apply(vec![
        job_start(start("j1")),
        Command::Start(Box::new(job("r1"))),
    ]);
    assert_eq!(h.held(), ["r1"]);
    assert_eq!(listed(&h), ["j1"]);
    assert!(
        !h.fake.created()[1].labels.contains_key(label::KEYING),
        "a run start's container is not labelled as a job's"
    );
    h.stop("j1");
    h.cancel("j1");
    h.apply(vec![job_stop("r1")]);
    h.life.step();
    assert!(h.fake.stops().is_empty(), "neither stop crossed over");
    h.fake.exit("j1", 0);
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.results()[0].run_id, "r1");
    assert_eq!(
        listing(&h, "j1").result.map(|r| r.outcome),
        Some(Outcome::Succeeded)
    );
    h.ack("j1", "ACCEPTED");
    ack_job(&mut h, "r1", "ACCEPTED");
    assert_eq!(h.held(), ["r1"]);
    assert_eq!(listed(&h), ["j1"]);
    ack_job(&mut h, "j1", "ACCEPTED");
    h.ack("r1", "ACCEPTED");
    assert!(h.held().is_empty());
    assert!(listed(&h).is_empty());
}

/// Asserts that a job start whose id is the run id of a held run, and a run start whose id
/// is the job id of a held job, are not taken: each is skipped as a clash and creates
/// nothing, and the work already held runs on. It catches two pieces of work sharing one
/// scratch directory and one set of labels.
#[test]
fn a_start_that_names_work_of_the_other_keying_is_not_taken() {
    let mut h = Harness::new("job-clash");
    h.apply(vec![Command::Start(Box::new(job("x")))]);
    let events = h.apply(vec![job_start(start("x"))]);
    assert_eq!(names(&events), ["skipped x Clash"]);
    h.apply(vec![job_start(start("y"))]);
    let events = h.apply(vec![Command::Start(Box::new(job("y")))]);
    assert_eq!(names(&events), ["skipped y Clash"]);
    assert_eq!(h.fake.created().len(), 2);
    assert_eq!(h.held(), ["x"]);
    assert_eq!(listed(&h), ["y"]);
}

/// Asserts that a job start the agent cannot honour creates nothing and is answered with a
/// `failed_to_start` result carrying the refusal code, in the job's listing and not in the
/// run results. It catches a refused job start reported where a lease server reads it.
#[test]
fn a_refused_job_start_is_answered_in_the_job_listing() {
    let mut h = Harness::new("job-refused");
    let mut spec = start("j1");
    spec.booked.cpu_millis = 64_000;
    let events = h.apply(vec![job_start(spec)]);
    assert_eq!(
        names(&events),
        [format!("refused j1 {}", refusal::LIMITS_EXCEED_SHAPE)]
    );
    assert!(h.fake.created().is_empty());
    assert!(h.life.results().is_empty());
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::LIMITS_EXCEED_SHAPE));
}

/// Asserts that a job start whose image is still being fetched is listed as a starting job
/// (and not as a run), and that the container made when the image is here is labelled as a
/// job's and listed running. It catches a job that loses its keying while it waits.
#[test]
fn a_job_waiting_for_its_image_stays_a_job() {
    let mut h = Harness::new("job-pull");
    h.fake.pull_pending(IMAGE);
    h.apply(vec![job_start(start("j1"))]);
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert!(h.held().is_empty());
    assert!(h.fake.created().is_empty());
    h.fake.pull_done(IMAGE);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
    assert_eq!(h.fake.created()[0].labels[label::KEYING], "job");
    assert!(h.held().is_empty());
}

/// Asserts that a restarted agent that adopts the container of a job start lists it as a
/// job again, not as a run, and that a container without the label is adopted as a run as
/// before. It catches an adopted job that a lease server would see as an unknown run.
#[test]
fn a_restart_lists_an_adopted_job_as_a_job() {
    let mut h = Harness::new("job-adopt");
    h.apply(vec![
        job_start(start("j1")),
        Command::Start(Box::new(job("r1"))),
    ]);
    h.restart().unwrap();
    assert_eq!(h.held(), ["r1"]);
    assert_eq!(listed(&h), ["j1"]);
    assert_eq!(listing(&h, "j1").state, HeldState::Running);
}

/// Asserts that `job_acks` and `retired_acks` that do not validate (an id twice) are
/// ignored while the commands beside them are acted on. It catches an agent that trusts a
/// malformed acknowledgement list.
#[test]
fn malformed_job_acknowledgements_are_ignored() {
    let mut h = Harness::new("job-bad-acks");
    h.apply(vec![job_start(start("j1")), job_stop("j2")]);
    h.fake.exit("j1", 0);
    h.life.step();
    let start_j3 = serde_json::to_value(job_start(start("j3"))).unwrap();
    let reply = reply_json(&json!({
        "commands": [start_j3],
        "job_acks": [
            { "job_id": "j1", "ack": "ACCEPTED" },
            { "job_id": "j1", "ack": "ACCEPTED" }
        ],
        "retired_acks": ["j2", "j2"]
    }));
    h.life.apply(&reply);
    assert!(listing(&h, "j1").result.is_some(), "not dropped");
    assert_eq!(h.life.retired_jobs(), ["j2"]);
    assert_eq!(listed(&h), ["j1", "j3"]);
}

/// Asserts that the list of retired job ids holds the newest [`RETIRED_LISTED`] (a list
/// whose bytes fit the heartbeat whatever the ids), dropping the oldest, that a dropped id
/// stays retired (its start runs nothing), and that a stop resent for it lists it again. It
/// catches a list that grows past what a heartbeat body holds (the heartbeat would be
/// refused and the agent would end), and a dropped answer that can never come back.
#[test]
fn the_retired_job_list_keeps_the_newest_ids() {
    let mut h = Harness::new("job-retired-bound");
    let ids: Vec<String> = (0..=RETIRED_LISTED).map(|n| format!("j{n}")).collect();
    h.apply(ids.iter().map(|id| job_stop(id.as_str())).collect());
    let retired = h.life.retired_jobs();
    assert_eq!(retired.len(), RETIRED_LISTED);
    assert_eq!(retired[0], "j1");
    assert_eq!(retired[RETIRED_LISTED - 1], format!("j{RETIRED_LISTED}"));
    let events = h.apply(vec![job_start(start("j0"))]);
    assert_eq!(names(&events), ["skipped j0 FenceRetired"]);
    h.apply(vec![job_stop("j0")]);
    let retired = h.life.retired_jobs();
    assert_eq!(retired.len(), RETIRED_LISTED);
    assert_eq!(retired.last().map(String::as_str), Some("j0"));
    assert_eq!(retired[0], "j2");
    assert!(h.fake.created().is_empty());
}

/// The image of a job start whose pull the fake runtime holds pending.
const COLD: &str = "registry.example/cold:1@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Asserts that a job start that names an environment runs: its container is created once
/// and carries the environment by name in order, and no `failed_to_start` result is
/// listed; a job start that first waits for its image to be pulled carries it as well when
/// its container is made; a run start beside them carries none. It catches the
/// `env-unsupported` refusal left in the lifecycle, an environment dropped on the way to
/// the runtime (at once, or kept for a start that waits), and one put on a run start.
#[test]
fn a_job_start_with_env_no_longer_fails_to_start_with_env_unsupported() {
    let mut h = Harness::new("job-env");
    let mut ready = start("j1");
    ready.env.insert("B".to_owned(), "two words".to_owned());
    ready.env.insert("A".to_owned(), "1".to_owned());
    let mut waits = ready.clone();
    waits.job_id = "j2".to_owned();
    waits.allocation_id = "alloc-j2".to_owned();
    waits.image = COLD.to_owned();
    h.fake.pull_pending(COLD);
    let events = h.apply(vec![job_start(ready), job_start(waits)]);
    assert_eq!(names(&events), ["started j1"]);
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    h.fake.pull_done(COLD);
    assert_eq!(names(&h.life.step()), ["started j2"]);

    let want = vec![
        ("A".to_owned(), "1".to_owned()),
        ("B".to_owned(), "two words".to_owned()),
    ];
    let created: Vec<(String, Vec<(String, String)>)> = h
        .fake
        .created()
        .into_iter()
        .map(|run| (run.run_id, run.env))
        .collect();
    assert_eq!(
        created,
        [
            ("j1".to_owned(), want.clone()),
            ("r1".to_owned(), Vec::new()),
            ("j2".to_owned(), want),
        ]
    );
    for job_id in ["j1", "j2"] {
        let held = listing(&h, job_id);
        assert_eq!(held.state, HeldState::Running, "{job_id}");
        assert_eq!(held.result, None, "{job_id}");
    }
    assert!(h.life.results().is_empty());
}

/// Asserts that on a runtime that carries no job start's environment (as the Mac one) a
/// job start that names one is answered `failed_to_start` with `env-unsupported` in the
/// job's listing and creates nothing (no container, no scratch directory, no booking), that
/// the same start delivered again adds no second result, and that a start with an empty
/// environment beside it runs. It catches a job run without the environment it names, and a
/// runtime's refusal the lifecycle does not ask for.
#[test]
fn a_runtime_that_cannot_carry_env_refuses_the_job_start_and_creates_nothing() {
    let mut h = Harness::new("job-env-refused");
    h.fake.refuse_env();
    let mut spec = start("j1");
    spec.env.insert("LANG".to_owned(), "C".to_owned());
    let events = h.apply(vec![job_start(spec.clone()), job_start(start("j2"))]);
    assert_eq!(
        names(&events),
        [
            format!("refused j1 {}", refusal::ENV_UNSUPPORTED),
            "started j2".to_owned()
        ]
    );
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(h.fake.created()[0].run_id, "j2");
    assert!(h.scratch_of("j1").metadata().is_err());
    assert_eq!(h.life.free().cpu_millis, 3000);
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::ENV_UNSUPPORTED));
    assert!(h.apply(vec![job_start(spec)]).is_empty());
    assert_eq!(listed(&h), ["j1", "j2"]);
    assert_eq!(h.fake.created().len(), 1);
}

/// Asserts that a job start with a `run` body is answered `failed_to_start` with
/// `run-unsupported` in the job's listing and creates nothing, as a run start with one is:
/// this agent runs an image's own command and no run body yet. It catches a run body
/// dropped and the image's own command run instead.
#[test]
fn a_job_start_with_a_run_body_is_refused_and_runs_nothing() {
    let mut h = Harness::new("job-run");
    let mut spec = start("j1");
    spec.command.clear();
    spec.run = Some(js_agent_wire::RunSpec {
        program: "/usr/bin/make".to_owned(),
        args: Vec::new(),
        env: Default::default(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: js_agent_wire::NetworkLevel::Off,
    });
    let events = h.apply(vec![job_start(spec)]);
    assert_eq!(
        names(&events),
        [format!("refused j1 {}", refusal::RUN_UNSUPPORTED)]
    );
    assert!(h.fake.created().is_empty());
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.detail.as_deref(), Some(refusal::RUN_UNSUPPORTED));
}

/// Proves a job start is held to the runtime's network ceiling as a run start is: under the
/// ceiling `none` a job start that asks for `allow-list` is answered `failed_to_start` with
/// `network-above-ceiling` and a text naming both levels, one that declares a port is refused
/// `ports-not-supported` (the ceiling `none` forwards no port), neither creates anything, and
/// both results are listed in `jobs`; a job start at the ceiling `allow-list` that asks for it
/// is created at that level. Catches a
/// job start whose network the agent drops on the way to the shared admission (it would run
/// with loopback only) or never compares with the ceiling.
#[test]
fn a_job_start_is_held_to_the_network_ceiling() {
    use js_agent::lifecycle::Event;
    use js_agent_wire::NetworkCeiling;
    use js_types::{JobNetwork, PortMap};

    let mut h = Harness::new("job-network");
    let mut wide = start("j1");
    wide.network = JobNetwork::AllowList;
    let events = h.apply(vec![job_start(wide)]);
    assert_eq!(
        events,
        [Event::Refused {
            run_id: "j1".to_owned(),
            code: refusal::NETWORK_ABOVE_CEILING,
            why: "network allow-list asked, agent ceiling is none".to_owned(),
        }]
    );
    let mut ported = start("j2");
    ported.ports = vec![PortMap {
        host: 8080,
        container: 80,
    }];
    assert_eq!(
        names(&h.apply(vec![job_start(ported)])),
        [format!("refused j2 {}", refusal::PORTS_NOT_SUPPORTED)]
    );
    assert!(
        h.fake.created().is_empty(),
        "a refused job start was created"
    );
    for job_id in ["j1", "j2"] {
        let result = listing(&h, job_id).result.expect("a result");
        assert_eq!(result.outcome, Outcome::FailedToStart);
    }

    let mut h = Harness::new("job-network-within");
    h.fake.set_ceiling(NetworkCeiling {
        level: JobNetwork::AllowList,
        ports: Vec::new(),
    });
    let mut listed = start("j3");
    listed.network = JobNetwork::AllowList;
    assert_eq!(names(&h.apply(vec![job_start(listed)])), ["started j3"]);
    let created: Vec<JobNetwork> = h.fake.created().iter().map(|run| run.network).collect();
    assert_eq!(created, [JobNetwork::AllowList]);
}
