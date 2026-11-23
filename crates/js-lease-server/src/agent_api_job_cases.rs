//! The per-job heartbeat against a fake agent, scenario by scenario (design section 5.3 and
//! section 8, rows 37 and 38): starts resent until listed, unknown and ended jobs told to
//! stop, a cancel answered by a retired id, a late start after a cancel, and a queued job
//! placed by a heartbeat under `--placement js-sched`.

use std::collections::BTreeMap;

use js_agent_wire::{Ack, BookedResources, HeldState, JobStartSpec, Layer, Outcome};
use js_jobs::{AllocationStep, EndReason, JobEnd, JobId, JobState, NewJob};

use crate::job_api::read::view::millis;

use super::job_rig::{BOOKED, FakeAgent, image, placing_rig, rig, spec, starts, stops};
use super::*;

/// A placed job's start is in every reply, the same each time, with the allocation's booked
/// resources, the job's wall limit and the allocation id, until the host lists the job; then
/// it is not sent again. It catches a start sent once only, one resent after the host listed
/// the job, and one that carries other resources than the allocation booked.
#[tokio::test]
async fn a_start_is_resent_until_listed() {
    let mut rig = rig("start_resent", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    let expected = JobStartSpec {
        job_id: "job-a".to_owned(),
        allocation_id: rig.allocation(&id).allocation_id.to_string(),
        image: image(),
        command: vec!["run".to_owned(), "--fast".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: BTreeMap::new(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1500,
            memory_bytes: 1 << 30,
            disk_bytes: Some(2 << 30),
            gpu_indices: Vec::new(),
        },
        wall_ms: 600_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: Some(millis(rig.job(&id).placed_at.unwrap())),
        fuse: false,
        xcode: None,
        sizing: Some(js_agent_wire::StartSizing {
            class: Some(js_sched_placer::sizes::class_name(&rig.job(&id))),
            cpu: js_agent_wire::BookedFrom::Named,
            memory: js_agent_wire::BookedFrom::Named,
        }),
    };
    let mut last = None;
    for _ in 0..3 {
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(starts(&reply), std::slice::from_ref(&expected));
        assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
        last = Some(reply);
    }
    agent.take(&last.unwrap());
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Starting, AllocationStep::Booked)
    );
    assert!(starts(&rig.beat(&agent).await.unwrap()).is_empty());
}

/// A host that lists a job id the store never made, an id that is no job id, and a job that
/// was lost (its allocation freed) is told to stop each of them; the store gains no job and no
/// allocation, the lost job stays lost and its allocation freed, and the host's listing is not
/// clean until it lists none of them. It catches a server that accepts such a listing, makes a
/// row for it, revives the lost job, or says nothing.
#[tokio::test]
async fn a_listed_unknown_job_is_told_to_stop() {
    let mut rig = rig("unknown_listed", true);
    rig.register("host-a", true);
    let host = HostId::new("host-a").unwrap();
    let mut agent = FakeAgent::new("host-a");
    let lost = rig.place("job-lost", "host-a");
    let allocation = rig.allocation(&lost).allocation_id.to_string();
    rig.jobs
        .end_and_free(&lost, JobEnd::Lost, EndReason::HostAbsent)
        .unwrap();
    let long = "x".repeat(70);
    agent.hold("ghost", &allocation, HeldState::Running);
    agent.hold("job-lost", &allocation, HeldState::Running);
    agent.hold(&long, &allocation, HeldState::Starting);
    let before = rig.commits();

    let reply = rig.beat(&agent).await.unwrap();
    let mut told = vec!["ghost".to_owned(), "job-lost".to_owned(), long.clone()];
    told.sort();
    assert_eq!(stops(&reply), told);
    assert!(starts(&reply).is_empty());
    // Catches a beat that writes more or fewer than its two row commits (the beat's, then the
    // listing's settle) when its listing changes no job.
    assert_eq!(rig.commits(), before + 2);
    assert!(!rig.agents.listing_clean(&host));
    let ghost = JobId::parse("ghost").unwrap();
    assert_eq!(rig.jobs.get_job(&ghost).unwrap(), None);
    assert_eq!(rig.jobs.get_allocation(&ghost).unwrap(), None);
    assert_eq!(rig.states(&lost), (JobState::Lost, AllocationStep::Freed));
    assert!(rig.jobs.live_on(&host).unwrap().is_empty());

    // The agent stops them and lists their results: each is acknowledged stale, nothing
    // moves, and once it lists none of them its listing is clean again.
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    let verdicts: Vec<_> = reply.job_acks.iter().map(|ack| ack.ack).collect();
    assert_eq!(verdicts, [Ack::Stale; 3]);
    assert_eq!(rig.states(&lost), (JobState::Lost, AllocationStep::Freed));
    agent.take(&reply);
    assert!(agent.listing().is_empty());
    rig.beat(&agent).await.unwrap();
    assert!(rig.agents.listing_clean(&host));
    assert_eq!(rig.commits(), before + 6);
}

/// A running job is cancelled, ends cancelled and its allocation is freed; when the host
/// lists it again as running (it kept the container across a partition), the job stays
/// cancelled, the host is told to stop it and the allocation stays freed under the same id. It
/// catches a late listing that moves the job's state or books it again.
#[tokio::test]
async fn a_cancelled_job_is_never_revived_by_a_late_listing() {
    let mut rig = rig("never_revived", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-a");
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.states(&id), (JobState::Running, AllocationStep::Booked));
    rig.jobs.request_cancel(&id).unwrap();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-a"]);
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Releasing)
    );
    agent.take(&reply);
    rig.beat(&agent).await.unwrap();
    let freed = rig.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Cancelled))
    );

    agent.hold(
        "job-a",
        &freed.allocation_id.to_string(),
        HeldState::Running,
    );
    let before = rig.commits();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-a"]);
    assert!(starts(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert_eq!(rig.allocation(&id), freed);
    // Catches a beat that writes more or fewer than its two row commits (the beat's, then the
    // listing's settle) when the listing changes no job.
    assert_eq!(rig.commits(), before + 2);
}

/// A placed job is cancelled before the host listed it; the host answers the stop with the
/// retired id, and the job becomes cancelled and its allocation freed in exactly one commit. A
/// failure injected at that commit leaves both as they were (and the answer is a 500), and
/// the next heartbeat applies it once. It catches the two changes made as two commits, and
/// one applied without the other.
#[tokio::test]
async fn retired_id_answer_cancels_and_frees_in_one_commit() {
    let mut rig = rig("retired_one_commit", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    rig.jobs.request_cancel(&id).unwrap();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(
        (stops(&reply), starts(&reply)),
        (vec!["job-a".to_owned()], vec![])
    );
    agent.take(&reply);
    assert!(!agent.holds("job-a"));

    // The beat's own row write fails first: 503, and nothing of the job changes.
    rig.store.fail_next();
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    // Let the beat's row write through; the listing's commit fails: 500.
    rig.store.fail_after(1);
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (500, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));

    let before = rig.commits();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 3);
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert_eq!(rig.allocation(&id).end_reason, Some(EndReason::Cancelled));
    assert_eq!(reply.retired_acks, ["job-a"]);
    assert!(reply.commands.is_empty());
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 5);
    assert!(reply.retired_acks.is_empty());
}

/// A job's start reaches the agent, then the job is cancelled: no reply from then on carries
/// its start, the agent stops it before it made a container, and a copy of the start that
/// arrives after the stop is not run. The reply that carries the stop also starts another job,
/// so a start and a stop travel together. It catches a start resent after the cancel intent
/// is committed.
#[tokio::test]
async fn a_late_start_after_a_cancel_does_not_run() {
    let mut rig = rig("late_start", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    let first = rig.beat(&agent).await.unwrap();
    assert_eq!(starts(&first).len(), 1);
    agent.take(&first);
    rig.jobs.request_cancel(&id).unwrap();
    rig.place("job-b", "host-a");

    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-a"]);
    let started: Vec<_> = starts(&reply).into_iter().map(|spec| spec.job_id).collect();
    assert_eq!(started, ["job-b"]);
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&id).state, JobState::Cancelled);
    assert!(starts(&reply).is_empty());
    agent.take(&reply);
    agent.take(&first);
    assert!(!agent.holds("job-a"));
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert!(agent.launched.is_empty());
    agent.run("job-b");
    agent.end("job-b", Outcome::Succeeded, Some(0));
    agent.take(&rig.beat(&agent).await.unwrap());
    assert_eq!(agent.launched, ["job-b"]);
}

/// Under `--placement js-sched`, a heartbeat through the route, with the poller's turn the
/// server runs right after it, places a job only queued (no test places it): after the beat the
/// job is placed on the beating host with its allocation booked, in the one commit the pass
/// makes; the reply to that beat, built before the pass, carries no start, and the next beat's
/// reply carries the job's start. Under the registry's placement the same beat places nothing.
/// It catches a heartbeat that does not poke the poller, a pass that does not commit, and a
/// start sent before the placement is committed.
#[tokio::test]
async fn a_heartbeat_places_a_queued_job_and_the_next_one_starts_it() {
    let queue = |rig: &super::job_rig::Rig| {
        let new = NewJob {
            id: Some(JobId::parse("job-q").unwrap()),
            spec: spec(),
            priority: 0,
            client: None,
        };
        rig.jobs.insert_queued(new).unwrap().record.id
    };
    let mut placing = placing_rig("beat_places");
    placing.register("host-a", true);
    let agent = FakeAgent::new("host-a");
    let id = queue(&placing);
    let commits = placing.commits();
    let first = placing.beat(&agent).await.unwrap();
    assert_eq!(
        placing.states(&id),
        (JobState::Placed, AllocationStep::Booked)
    );
    assert_eq!(placing.allocation(&id).host.as_str(), "host-a");
    // The placement's commit plus the beat's two row writes (beat_node, settle_node).
    assert_eq!(placing.commits(), commits + 3);
    assert_eq!(starts(&first), Vec::new());
    let second = placing.beat(&agent).await.unwrap();
    let started: Vec<String> = starts(&second).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-q"]);

    let mut unplaced = rig("beat_places_not", true);
    unplaced.register("host-a", true);
    let id = queue(&unplaced);
    unplaced.beat(&agent).await.unwrap();
    assert_eq!(unplaced.job(&id).state, JobState::Queued);
}

/// Under `--placement js-sched`, a host whose listing names a job it holds no live allocation
/// for takes no start while it lists it, though it has room: neither the beat that lists it,
/// nor the one that lists its result, nor a pass of its own places the queued job there. The
/// first beat whose listing is clean places it (its start rides on the next reply). It
/// catches a placement that counts the capacity of a host still running a job the store does
/// not know (design section 8, row 37).
#[tokio::test]
async fn a_host_that_lists_an_unknown_job_takes_no_start_until_its_listing_is_clean() {
    let mut rig = placing_rig("unclean_no_start");
    rig.register("host-a", true);
    let host = HostId::new("host-a").unwrap();
    let mut agent = FakeAgent::new("host-a");
    agent.hold("ghost", "00", HeldState::Running);
    let new = NewJob {
        id: Some(JobId::parse("job-q").unwrap()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    let queued = rig.jobs.insert_queued(new).unwrap().record.id;

    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["ghost"]);
    assert!(!rig.agents.listing_clean(&host));
    assert_eq!(rig.job(&queued).state, JobState::Queued);
    rig.agents.place_jobs();
    assert_eq!(rig.job(&queued).state, JobState::Queued);

    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["ghost"]);
    assert_eq!(rig.job(&queued).state, JobState::Queued);

    agent.take(&reply);
    assert!(agent.listing().is_empty());
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert!(rig.agents.listing_clean(&host));
    assert_eq!(
        rig.states(&queued),
        (JobState::Placed, AllocationStep::Booked)
    );
    let reply = rig.beat(&agent).await.unwrap();
    let started: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-q"]);
}

/// A job that names a layer, queued under `--placement js-sched` with the capability the job
/// service records for it (`layered-rootfs=true`), which the placer asks a host for.
fn queue_layered(rig: &super::job_rig::Rig, id: &str) -> JobId {
    let mut layered = spec();
    layered
        .constraints
        .capabilities
        .insert("layered-rootfs".to_owned(), "true".to_owned());
    layered.layers = vec![Layer::HostPath {
        path: "/opt/tools".to_owned(),
    }];
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec: layered,
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap().record.id
}

/// A job with layers is placed on, and started on, only the host whose hello reported
/// `layered-rootfs` as `true`: with one host that did and one that did not (and one that
/// reported `false`), the job lands on the first and its start, resent until listed, carries
/// the layer; the others are sent no start. With only hosts that did not, the job stays
/// queued, no allocation is made and no start is sent. A layered job already placed on a host
/// that does not report it (a placement from before the host's agent changed) is sent no start
/// either and stays placed. It catches a layered job run by an agent that would ignore its
/// layers and run it on the image alone.
#[tokio::test]
async fn a_job_with_layers_is_started_only_on_a_host_that_reports_layered_rootfs() {
    let mut rig = placing_rig("layered_capable");
    rig.register("host-a", true);
    rig.register_layered_rootfs("host-c", "false");
    rig.register_layered_rootfs("host-b", "true");
    let id = queue_layered(&rig, "job-l");
    let mut capable = FakeAgent::new("host-b");
    for host in ["host-a", "host-c", "host-b"] {
        let reply = rig.beat(&FakeAgent::new(host)).await.unwrap();
        assert!(reply.commands.is_empty() | (host == "host-b"), "{host}");
    }
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
    let mut last = None;
    for _ in 0..2 {
        let reply = rig.beat(&capable).await.unwrap();
        let sent = starts(&reply);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].job_id, "job-l");
        assert_eq!(
            sent[0].layers,
            [Layer::HostPath {
                path: "/opt/tools".to_owned()
            }]
        );
        last = Some(reply);
    }
    capable.take(&last.unwrap());
    for host in ["host-a", "host-c"] {
        assert!(starts(&rig.beat(&FakeAgent::new(host)).await.unwrap()).is_empty());
    }

    let mut plain = placing_rig("layered_incapable");
    plain.register("host-a", true);
    plain.register_layered_rootfs("host-c", "false");
    let id = queue_layered(&plain, "job-l");
    for host in ["host-a", "host-c", "host-a"] {
        let reply = plain.beat(&FakeAgent::new(host)).await.unwrap();
        assert_eq!(starts(&reply), Vec::new(), "{host}");
    }
    assert_eq!(plain.job(&id).state, JobState::Queued);
    assert!(plain.jobs.get_allocation(&id).unwrap().is_none());

    let stale = rig_with_placed_layered_job();
    let reply = stale.0.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert_eq!(starts(&reply), Vec::new());
    assert_eq!(
        stale.0.states(&stale.1),
        (JobState::Placed, AllocationStep::Booked)
    );
}

/// A rig whose host `host-a` reports no `layered-rootfs`, with a job of layers placed on it.
fn rig_with_placed_layered_job() -> (super::job_rig::Rig, JobId) {
    let mut rig = rig("layered_placed_elsewhere", true);
    rig.register("host-a", true);
    let mut layered = spec();
    layered.layers = vec![Layer::HostPath {
        path: "/opt/tools".to_owned(),
    }];
    let (job, _) = super::job_rig::placed(
        &rig.jobs,
        "job-l",
        "host-a",
        layered,
        super::job_rig::BOOKED,
    );
    (rig, job.id)
}

/// A job spec with a layer from each of the providers `names`, with the capabilities the job
/// service records for it (`layered-rootfs` and one `mount-provider.<name>` each).
fn provider_spec(names: &[&str]) -> js_jobs::JobSpec {
    let mut provided = spec();
    let capabilities = &mut provided.constraints.capabilities;
    capabilities.insert("layered-rootfs".to_owned(), "true".to_owned());
    for name in names {
        capabilities.insert(js_caps::mount_provider_key(name), "true".to_owned());
        provided.layers.push(Layer::Provider {
            provider: (*name).to_owned(),
            reference: format!("trees/{name}"),
        });
    }
    provided
}

/// A job that names a provider is placed on, and started on, only a host whose hello reported
/// `mount-provider.<name>` as `true`: with a host that reports nothing of providers, one that
/// reports another provider and one that reports it, the job lands on the last and its start
/// carries the provider layer, and the others are sent no start. A job already placed on a host
/// that does not report the provider (a placement from before the host's agent changed) is
/// sent no start and stays placed, while a job placed on a host that reports it is started.
/// It catches a provider job placed on a host that cannot make its tree, and a start sent to
/// an agent that would not read the source.
#[tokio::test]
async fn a_job_naming_a_provider_is_started_only_on_a_host_that_reports_it() {
    let mut placing = placing_rig("provider_capable");
    placing.register_layered_rootfs("host-a", "true");
    placing.register_mount_providers("host-b", &["cache"]);
    placing.register_mount_providers("host-c", &["cache", "tierstore"]);
    let new = NewJob {
        id: Some(JobId::parse("job-p").unwrap()),
        spec: provider_spec(&["tierstore"]),
        priority: 0,
        client: None,
    };
    let id = placing.jobs.insert_queued(new).unwrap().record.id;
    for host in ["host-a", "host-b", "host-c"] {
        let reply = placing.beat(&FakeAgent::new(host)).await.unwrap();
        assert!(reply.commands.is_empty() | (host == "host-c"), "{host}");
    }
    assert_eq!(placing.allocation(&id).host.as_str(), "host-c");
    let reply = placing.beat(&FakeAgent::new("host-c")).await.unwrap();
    let sent = starts(&reply);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].layers,
        [Layer::Provider {
            provider: "tierstore".to_owned(),
            reference: "trees/tierstore".to_owned(),
        }]
    );
    for host in ["host-a", "host-b"] {
        assert!(starts(&placing.beat(&FakeAgent::new(host)).await.unwrap()).is_empty());
    }

    let mut stale = rig("provider_placed_elsewhere", true);
    stale.register_layered_rootfs("host-a", "true");
    stale.register_mount_providers("host-c", &["tierstore"]);
    let provided = || provider_spec(&["tierstore"]);
    let (elsewhere, _) = super::job_rig::placed(&stale.jobs, "job-w", "host-a", provided(), BOOKED);
    let (fitting, _) = super::job_rig::placed(&stale.jobs, "job-f", "host-c", provided(), BOOKED);
    let reply = stale.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert_eq!(starts(&reply), Vec::new());
    assert_eq!(
        stale.states(&elsewhere.id),
        (JobState::Placed, AllocationStep::Booked)
    );
    let reply = stale.beat(&FakeAgent::new("host-c")).await.unwrap();
    let sent = starts(&reply);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].job_id, fitting.id.to_string());
}

/// A job spec that names the Xcode build `build`, as a submit records it.
fn xcode_spec(build: &str) -> js_jobs::JobSpec {
    let mut named = spec();
    let capabilities = &mut named.constraints.capabilities;
    capabilities.insert(js_caps::XCODE.to_owned(), build.to_owned());
    named
}

/// A job that names an Xcode build is placed on, and started on, only a host whose hello lists
/// that build among its comma-joined builds: of a host that lists another build and one that
/// lists it among two, the job lands on the second and its start carries `xcode`, and the first
/// is sent no start; with only the first registered the job stays queued and nothing is sent.
/// A job already placed on a host that does not list the build (a placement from before the
/// host's agent changed) is sent no start and stays placed. It catches a job run with a
/// host's selected Xcode, and one placed where the build is not installed.
#[tokio::test]
async fn a_job_naming_an_xcode_is_started_only_on_a_host_that_lists_the_build() {
    let mut placing = placing_rig("xcode_capable");
    placing.register_xcodes("host-a", "15F31d");
    placing.register_xcodes("host-b", "16C5032a,16E140");
    let new = NewJob {
        id: Some(JobId::parse("job-x").unwrap()),
        spec: xcode_spec("16E140"),
        priority: 0,
        client: None,
    };
    let id = placing.jobs.insert_queued(new).unwrap().record.id;
    for host in ["host-a", "host-b"] {
        let reply = placing.beat(&FakeAgent::new(host)).await.unwrap();
        assert!(reply.commands.is_empty() | (host == "host-b"), "{host}");
    }
    assert_eq!(placing.allocation(&id).host.as_str(), "host-b");
    let sent = starts(&placing.beat(&FakeAgent::new("host-b")).await.unwrap());
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].xcode.as_deref(), Some("16E140"));
    assert!(starts(&placing.beat(&FakeAgent::new("host-a")).await.unwrap()).is_empty());

    let mut lacking = placing_rig("xcode_lacking");
    lacking.register_xcodes("host-a", "15F31d");
    let new = NewJob {
        id: Some(JobId::parse("job-y").unwrap()),
        spec: xcode_spec("16E140"),
        priority: 0,
        client: None,
    };
    let queued = lacking.jobs.insert_queued(new).unwrap().record.id;
    for _ in 0..2 {
        let reply = lacking.beat(&FakeAgent::new("host-a")).await.unwrap();
        assert!(reply.commands.is_empty());
    }
    assert_eq!(lacking.job(&queued).state, JobState::Queued);

    let mut stale = rig("xcode_placed_elsewhere", true);
    stale.register_xcodes("host-a", "15F31d");
    let (elsewhere, _) =
        super::job_rig::placed(&stale.jobs, "job-w", "host-a", xcode_spec("16E140"), BOOKED);
    let reply = stale.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert_eq!(starts(&reply), Vec::new());
    assert_eq!(
        stale.states(&elsewhere.id),
        (JobState::Placed, AllocationStep::Booked)
    );
}

/// Through the heartbeats alone, results that carry the usage their agent counted teach the
/// job's class, and the next job of it that books nothing is placed booked from that history:
/// after clean runs of a class that reported 2 GiB peaks, its next job books less memory
/// than the cold size (the first jobs booked), while a class of another client whose five clean
/// runs reported no usage books the cold size again. It catches a result whose usage is lost
/// between the wire and the stored job (every class then stays cold however often it ran).
#[tokio::test]
async fn results_that_report_usage_size_the_next_job_of_the_class_from_history() {
    let mut rig = placing_rig("usage_sizes");
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let counted = js_agent_wire::JobUsage {
        peak_memory_bytes: 2 << 30,
        cpu_usage_usec: 30_000_000,
        cpu_throttle: Some(js_agent_wire::CpuThrottle {
            periods: 0,
            throttled_periods: 0,
        }),
        cpu_pressure_some_usec: Some(0),
        cpu_peak: None,
        peak_swap_bytes: None,
    };
    let mut memory_of = Vec::new();
    for (class, usage) in [("measured", Some(counted)), ("bare", None)] {
        let labels = BTreeMap::from([("size-class".to_owned(), class.to_owned())]);
        for round in 0..6 {
            let id = format!("{class}-{round}");
            let new = NewJob {
                id: Some(JobId::parse(&id).unwrap()),
                spec: js_jobs::JobSpec {
                    needs: js_jobs::Needs::Shape(js_types::Resources::new(0, 0, 0, None).unwrap()),
                    labels: js_types::Labels::from_caller(&labels).unwrap(),
                    ..spec()
                },
                priority: 0,
                client: Some(js_jobs::ClientName::parse(class).unwrap()),
            };
            let queued = rig.jobs.insert_queued(new).unwrap().record.id;
            rig.beat(&agent).await.unwrap();
            agent.take(&rig.beat(&agent).await.unwrap());
            agent.run(&id);
            rig.beat(&agent).await.unwrap();
            rig.advance(std::time::Duration::from_secs(10));
            match usage {
                Some(usage) => {
                    agent.end(&id, Outcome::Succeeded, Some(0));
                    agent.report(&id, usage);
                }
                None => agent.end(&id, Outcome::Succeeded, Some(0)),
            }
            agent.take(&rig.beat(&agent).await.unwrap());
            rig.beat(&agent).await.unwrap();
            assert_eq!(rig.allocation(&queued).step, AllocationStep::Freed, "{id}");
            if round == 0 || round == 5 {
                let booked = rig.allocation(&queued).booked;
                memory_of.push(crate::job_alloc::booked_of(booked).memory_bytes);
            }
        }
    }
    assert_eq!(memory_of.len(), 4, "two classes, two samples each");
    let (cold, learned, first_bare, last_bare) =
        (memory_of[0], memory_of[1], memory_of[2], memory_of[3]);
    assert_eq!(
        first_bare, cold,
        "a class with no history books the cold size"
    );
    assert_eq!(last_bare, cold, "no usage reported: still the cold size");
    assert!(
        learned < cold,
        "five runs with usage size the sixth from history below the cold size: {learned} < {cold}"
    );
}

/// One job service and two agents during a roll of the agents: one of the release now deployed,
/// which holds a job at its memory booking (1 GiB, [`BOOKED`]) and reports it killed there, out
/// of memory with its peak at the booking; and one of this release, which holds a job of the same
/// booking to nothing, so that it uses 1.5 GiB and part of the host's swap, and reports it
/// succeeded with that peak and a swap peak. The server takes both results from the same
/// heartbeats, as each agent sent it: the deployed agent's job fails for memory with its 1 GiB
/// peak, this release's succeeds with its whole 1.5 GiB peak, above its booking. It catches a
/// result of either release that the server refuses or leaves unanswered, a swap peak the server
/// cannot take, and a peak above the booking the server does not record as sent.
#[tokio::test]
async fn a_deployed_agent_and_this_one_report_their_jobs_to_one_server_as_each_measured() {
    let mut rig = rig("deployed_and_new_agents", true);
    rig.register("host-deployed", true);
    rig.register("host-new", true);
    let mut deployed = FakeAgent::new("host-deployed");
    let mut new = FakeAgent::new("host-new");
    let held = rig.place("held", "host-deployed");
    let over = rig.place("over", "host-new");
    let usage = |peak_memory_bytes, peak_swap_bytes| js_agent_wire::JobUsage {
        peak_memory_bytes,
        cpu_usage_usec: 30_000_000,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes,
    };
    for (agent, name, id) in [(&mut deployed, "held", &held), (&mut new, "over", &over)] {
        let allocation = rig.allocation(id).allocation_id.to_string();
        agent.hold(name, &allocation, HeldState::Running);
        rig.beat(agent).await.unwrap();
        assert_eq!(rig.job(id).state, JobState::Running, "{name}");
    }
    deployed.end("held", Outcome::OutOfMemory, None);
    deployed.report("held", usage(1 << 30, None));
    new.end("over", Outcome::Succeeded, Some(0));
    new.report("over", usage(3 << 29, Some(256 << 20)));
    for (agent, name) in [(&deployed, "held"), (&new, "over")] {
        let reply = rig.beat(agent).await.unwrap();
        let acks: Vec<Ack> = reply.job_acks.iter().map(|ack| ack.ack).collect();
        assert_eq!(acks, [Ack::Accepted], "{name}");
    }
    let ended = |id: &JobId| {
        let job = rig.job(id);
        let result = job.result.expect("a result");
        let peak = result.usage.map(|usage| usage.peak_memory_bytes);
        (job.state, result.reason, peak)
    };
    assert_eq!(
        ended(&held),
        (
            JobState::Failed,
            js_jobs::ResultReason::Memory,
            Some(1 << 30)
        )
    );
    assert_eq!(
        ended(&over),
        (
            JobState::Succeeded,
            js_jobs::ResultReason::Exit,
            Some(3 << 29)
        )
    );
}
