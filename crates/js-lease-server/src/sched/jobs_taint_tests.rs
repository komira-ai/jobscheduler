//! The taint a host job leaves on its host, on the server's side: written in the commit that
//! places the job and then on the registry and the host's binding, closing the host to every
//! other job while the host job itself starts and ends, kept across a restart and a hello,
//! and the capability that decides which hosts a host job may go to. Every clock is manual;
//! nothing sleeps.

use std::path::Path;
use std::sync::Arc;

use js_agent_wire::{Ack, HOST_JOBS_CAPABILITY, Hello, Isolation, Outcome, Taint};
use js_jobs::interfere::Interfere;
use js_jobs::{AllocationStep, HOST_JOB_TAINT, JobState};
use js_registry::Credential;
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use serde_json::json;

use super::tests::{clock, host, on_the_machine, queue_spec, sized};
use crate::adapt::SharedClock;
use crate::agent_api::job_rig::{FakeAgent, placing_rig, starts};
use crate::agent_state::{self, BindingStore};
use crate::agents::{Agents, assemble_placed, state_path};
use crate::hosts::HostList;
use crate::sched::engine_rig::{GIB, hello_of, pool};
use crate::sched::wiring_tests::{JOIN, config_of, scratch};

/// The taint a host job `job` leaves.
fn host_job_taint(job: &str) -> Taint {
    Taint {
        reason: HOST_JOB_TAINT.to_owned(),
        run_id: Some(job.to_owned()),
        check: None,
    }
}

/// The taint the registry holds for `name`.
fn registry_taint(agents: &Agents, name: &str) -> Option<Taint> {
    let id = host(name);
    let views = agents.registry.hosts();
    views
        .into_iter()
        .find(|view| view.id == id)
        .and_then(|view| view.taint)
}

/// A store for the host bindings that can fail a commit.
fn kept_store() -> Arc<Interfere<Fake>> {
    Arc::new(Interfere::new(Fake::with_flaw(Arc::default(), Flaw::None)))
}

/// The taint the binding of `name` holds in `kept`, read as a server reads it at start.
fn bound_taint(kept: &Arc<Interfere<Fake>>, name: &str) -> Option<Taint> {
    let dynamic: Arc<dyn Store> = kept.clone();
    let (_, bindings) = BindingStore::in_store(dynamic).unwrap();
    let binding = bindings.get(&host(name));
    binding.and_then(|binding| binding.taint().cloned())
}

/// Asserts that a host job's placement is one commit that holds the job's move to placed, its
/// booked allocation and the host's taint row, and that the taint reaches the registry at once
/// and the host's binding only after: a failed commit leaves the job queued with no taint row
/// and the host untainted; the commit that succeeds is one transaction, after which the
/// registry holds reason `host-job` with the job's id as the run id and the binding does not
/// yet; writing the binding then deletes the job store's taint row; the next beat carries the
/// start with host isolation. It catches a taint written in a second transaction, a taint
/// recorded for a placement that did not commit, a binding left without the taint, and a row
/// deleted before the binding holds it.
#[tokio::test]
async fn the_taint_is_recorded_in_the_commit_that_places_a_host_job() {
    let kept = kept_store();
    let mut rig = placing_rig("taint_commit").with_kept(kept.clone());
    rig.register_host_jobs("host-a", "true");
    let agent = FakeAgent::new("host-a");
    let id = queue_spec(&rig.jobs, "job-h", on_the_machine(1), 0);

    rig.store.fail_next();
    assert!(rig.agents.place_jobs().is_empty());
    assert_eq!(rig.job(&id).state, JobState::Queued);
    assert!(rig.store.get("taint/host-a").unwrap().is_none());
    assert_eq!(registry_taint(&rig.agents, "host-a"), None);

    let commits = rig.store.commit_stats().txns;
    let tainted = rig.agents.place_jobs();
    assert_eq!(tainted, [host("host-a")]);
    assert_eq!(rig.store.commit_stats().txns, commits + 1);
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    assert!(rig.store.get("taint/host-a").unwrap().is_some());
    let taint = host_job_taint("job-h");
    assert_eq!(registry_taint(&rig.agents, "host-a"), Some(taint.clone()));
    assert_eq!(bound_taint(&kept, "host-a"), None);

    rig.agents.keep_taints(tainted).await;
    assert_eq!(bound_taint(&kept, "host-a"), Some(taint));
    assert!(rig.store.get("taint/host-a").unwrap().is_none());

    let reply = rig.beat(&agent).await.unwrap();
    let sent = starts(&reply);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].job_id, "job-h");
    assert_eq!(sent[0].isolation, Isolation::Host);
}

/// Asserts that once a host job is placed its host takes no other job, though a contained job
/// that fits is queued, and that the host job itself still goes through its life there: its
/// start is sent, the host lists it starting and then running, its result is accepted and the
/// job ends with it; the contained job stays queued afterwards too. It catches a tainted host
/// that is still offered to jobs, and one that is cut off from the job it was tainted for.
#[tokio::test]
async fn a_tainted_host_takes_no_other_job_but_finishes_the_host_job() {
    let mut rig = placing_rig("taint_finishes");
    rig.register_host_jobs("host-a", "true");
    let mut agent = FakeAgent::new("host-a");
    let host_job = queue_spec(&rig.jobs, "job-h", on_the_machine(1), 0);
    rig.beat(&agent).await.unwrap();
    let other = queue_spec(&rig.jobs, "job-c", sized(1), 0);

    let reply = rig.beat(&agent).await.unwrap();
    let sent: Vec<String> = starts(&reply)
        .into_iter()
        .map(|start| start.job_id)
        .collect();
    assert_eq!(sent, ["job-h"]);
    assert_eq!(rig.job(&other).state, JobState::Queued);
    assert!(rig.jobs.get_allocation(&other).unwrap().is_none());

    agent.take(&reply);
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&host_job).state, JobState::Starting);
    agent.run("job-h");
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&host_job).state, JobState::Running);
    agent.end("job-h", Outcome::Succeeded, Some(0));
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.job_acks.len(), 1);
    assert_eq!(reply.job_acks[0].ack, Ack::Accepted);
    assert_eq!(rig.job(&host_job).state, JobState::Succeeded);

    agent.take(&reply);
    rig.beat(&agent).await.unwrap();
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&other).state, JobState::Queued);
    assert!(rig.jobs.get_allocation(&other).unwrap().is_none());
}

/// Asserts that a host job goes only to a host whose hello reported `host-jobs`: with a host
/// that did and one that did not, the job lands on the first and only that one is tainted;
/// with only the second the job stays queued, no start is sent, nothing is tainted and no
/// taint row is written. It catches a host job placed on a host that cannot run it, which
/// would taint a machine for a job that never starts.
#[tokio::test]
async fn a_host_job_is_sent_only_to_a_host_that_reports_host_jobs() {
    let mut rig = placing_rig("taint_capable");
    rig.register("host-a", true);
    rig.register_host_jobs("host-b", "true");
    let id = queue_spec(&rig.jobs, "job-h", on_the_machine(1), 0);
    assert_eq!(rig.agents.place_jobs(), [host("host-b")]);
    assert_eq!(rig.allocation(&id).host, host("host-b"));
    assert_eq!(registry_taint(&rig.agents, "host-a"), None);
    assert_eq!(
        registry_taint(&rig.agents, "host-b"),
        Some(host_job_taint("job-h"))
    );

    let mut plain = placing_rig("taint_incapable");
    plain.register("host-a", true);
    let agent = FakeAgent::new("host-a");
    let id = queue_spec(&plain.jobs, "job-h", on_the_machine(1), 0);
    for _ in 0..2 {
        let reply = plain.beat(&agent).await.unwrap();
        assert_eq!(starts(&reply), Vec::new());
    }
    assert_eq!(plain.job(&id).state, JobState::Queued);
    assert!(plain.jobs.get_allocation(&id).unwrap().is_none());
    assert!(plain.store.get("taint/host-a").unwrap().is_none());
    assert_eq!(registry_taint(&plain.agents, "host-a"), None);
}

/// Asserts that the first taint of a host stands: after a host job tainted it, a heartbeat that
/// reports another taint (`residue`) leaves `host-job` in the registry, in the host's binding
/// and in the body of `GET /v1/hosts`, which names the job as the run id. It catches a
/// heartbeat's report that replaces the reason a host job gave.
#[tokio::test]
async fn the_first_taint_reason_stands() {
    let kept = kept_store();
    let mut rig = placing_rig("taint_first").with_kept(kept.clone());
    rig.register_host_jobs("host-a", "true");
    let agent = FakeAgent::new("host-a");
    queue_spec(&rig.jobs, "job-h", on_the_machine(1), 0);
    rig.beat(&agent).await.unwrap();
    let residue = Taint {
        reason: "residue".to_owned(),
        run_id: Some("run-9".to_owned()),
        check: None,
    };
    rig.beat_reporting(&agent, Some(residue)).await.unwrap();

    let taint = host_job_taint("job-h");
    assert_eq!(registry_taint(&rig.agents, "host-a"), Some(taint.clone()));
    assert_eq!(bound_taint(&kept, "host-a"), Some(taint));
    let list = HostList::of(Vec::new(), rig.agents.registry.hosts(), None);
    let body = serde_json::to_value(&list).unwrap();
    let wanted = json!({ "reason": "host-job", "run_id": "job-h" });
    assert_eq!(body["hosts"][0]["taint"], wanted);
}

/// Asserts that a binding write that fails after the placement leaves the host tainted in the
/// registry, so no other job lands on it, keeps the job store's taint row, and that the next
/// save of the bindings writes the taint; that a failed delete of the row leaves it for the
/// next try; that a host with no binding keeps its row; and that no hosts to keep writes
/// nothing. It catches a failed write that undoes the taint or deletes the row the taint then
/// lives in alone, and one that is never tried again.
#[tokio::test]
async fn a_failed_binding_write_is_written_by_the_next_save() {
    let kept = kept_store();
    let mut rig = placing_rig("taint_unwritten").with_kept(kept.clone());
    rig.register_host_jobs("host-a", "true");
    queue_spec(&rig.jobs, "job-h", on_the_machine(1), 0);
    let tainted = rig.agents.place_jobs();
    assert_eq!(tainted, [host("host-a")]);

    let before = kept.commit_stats().txns;
    rig.agents.keep_taints(Vec::new()).await;
    assert_eq!(kept.commit_stats().txns, before);

    kept.fail_next();
    rig.agents.keep_taints(tainted).await;
    assert_eq!(bound_taint(&kept, "host-a"), None);
    assert_eq!(
        registry_taint(&rig.agents, "host-a"),
        Some(host_job_taint("job-h"))
    );
    assert!(rig.store.get("taint/host-a").unwrap().is_some());
    assert!(rig.agents.save_bindings().await);
    assert_eq!(bound_taint(&kept, "host-a"), Some(host_job_taint("job-h")));

    rig.store.fail_next();
    rig.agents.keep_taints(vec![host("host-a")]).await;
    assert!(rig.store.get("taint/host-a").unwrap().is_some());
    rig.agents.keep_taints(vec![host("host-z")]).await;
    assert!(rig.store.get("taint/host-a").unwrap().is_some());
    rig.agents.keep_taints(vec![host("host-a")]).await;
    assert!(rig.store.get("taint/host-a").unwrap().is_none());
    assert_eq!(bound_taint(&kept, "host-a"), Some(host_job_taint("job-h")));
}

/// A hello of `name` reporting `per-job` and `host-jobs`.
fn host_jobs_hello(name: &str) -> Hello {
    let mut hello = hello_of(name, 16_000, 32 * GIB);
    let capabilities = [("per-job", "true"), (HOST_JOBS_CAPABILITY, "true")];
    for (key, value) in capabilities {
        hello.capabilities.insert(key.to_owned(), value.to_owned());
    }
    hello
}

/// The agent side of a server over the files in `dir` (leases beside the job store and the
/// bindings), placing with `js-sched` and holding the job tables.
fn boot(dir: &Path) -> Arc<Agents> {
    let extra = ["--placement", "js-sched", "--job-listen", "127.0.0.1:0"];
    let config = config_of(dir, &pool(1, 8000, ""), &extra);
    let shared = SharedClock::new(clock());
    let assembled = assemble_placed(
        config.hosts,
        config.usage,
        &shared,
        &config.state_file,
        None,
    );
    let ((_, agents), _) = assembled.unwrap();
    agents.unwrap()
}

/// A server that ran a host job to its placement and stopped (`binding_written` says whether
/// the binding got the taint before it did), started again over its files: the host is
/// tainted `host-job` in the registry, and stays so through its own hello; the job store holds
/// no taint row after the start (the binding does); once the bindings are saved, the file
/// holds the taint.
async fn restarted_after_placing_a_host_job(name: &str, binding_written: bool) {
    let dir = scratch(name);
    let cred = Credential::of_token(JOIN);
    let hello = host_jobs_hello("h1");
    let first = boot(dir.path());
    first.registry.hello(&hello, &cred).unwrap();
    assert!(first.save_bindings().await);
    queue_spec(first.jobs.as_ref().unwrap(), "job-h", on_the_machine(2), 0);
    let tainted = first.place_jobs();
    assert_eq!(tainted, [host("h1")]);
    if binding_written {
        first.keep_taints(tainted).await;
    }
    drop(first);

    let second = boot(dir.path());
    let rows = second.jobs.as_ref().unwrap().host_taints().unwrap();
    assert!(rows.is_empty(), "{rows:?}");
    let taint = host_job_taint("job-h");
    assert_eq!(registry_taint(&second, "h1"), Some(taint.clone()));
    second.registry.hello(&hello, &cred).unwrap();
    assert_eq!(registry_taint(&second, "h1"), Some(taint.clone()));
    assert!(second.save_bindings().await);
    let path = state_path(&dir.path().join("leases.toml"), ".agents");
    let bindings = agent_state::load(&path).unwrap();
    assert_eq!(bindings[&host("h1")].taint(), Some(&taint));
}

/// Asserts that a host job's taint survives a server restart over the same files and the
/// host's own hello, both when the binding got the taint and when the server stopped between
/// the placement and the binding's write (the job store's taint row is put on the binding at
/// start). It catches a taint held in memory only, and a crash between the two writes that
/// frees the host for other jobs.
#[tokio::test]
async fn the_taint_survives_a_server_restart_and_a_hello() {
    restarted_after_placing_a_host_job("taint_restart", true).await;
    restarted_after_placing_a_host_job("taint_restart_crash", false).await;
}
