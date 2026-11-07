//! A restart over the same files with registered hosts: the jobs and the restored leases on
//! them, the bindings, and the file the bindings are kept in.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::Duration;

use js_lease_server::{SharedClock, StartupError, channel_ticker};
use js_registry::Credential;
use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, JOIN_B, Session, agent_args, beat, entry, hello_with, hosts, job_hello, join, listed,
    post, restarted_job_hosts, session, start_agents, start_agents_in, start_job_hosts_in,
};
use crate::jobs_support::{
    JobBody, beat_with, commands, exited, job_agent, job_state, one_start, running, submit_body,
};
use crate::support::{
    Harness, Kept, LogCapture, Scratch, base_args, config, kept_bindings, lease_file, replace,
    start,
};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The file of bindings beside the lease file.
fn agent_file(scratch: &Scratch) -> PathBuf {
    let mut path = scratch.state().into_os_string();
    path.push(".agents");
    PathBuf::from(path)
}

/// Submits the job `id` (6 of a host's 8 cores), ticks, and returns the allocation the next
/// heartbeat of `host` is told to start it under; the job is then running on the host.
async fn running_on(h: &Harness, host: &Session, id: &str) -> String {
    let made = submit_body(h, &JobBody::new(id).needs(6000, 1 << 30).json()).await;
    assert_eq!(made.status, 201, "{}", made.body);
    h.ticks.step().await;
    let (job, allocation) = one_start(&beat_with(h, host, &[]).await);
    assert_eq!(job, id);
    beat_with(h, host, &[running(id)]).await;
    allocation
}

/// A host of the pool that speaks the per-job messages, admitted with the pool's join token.
async fn job_host(h: &Harness, host: &str) -> Session {
    job_agent(h, &job_hello(host)).await
}

/// The heartbeat of `host` that reports a taint, which the server acknowledges. A tainted
/// host is what a restart seats again from its binding.
async fn taint(h: &Harness, host: &Session) {
    let mut body = host.beat();
    body["taint"] = json!({ "reason": "residue", "check": "tmp" });
    let sent = post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    assert_eq!(sent.json()["taint_ack"], true, "{}", sent.body);
}

/// Asserts what a restart does to a tainted registered host that runs a job. The job stays
/// running and is not ended by ticks while the host is not heard; the host reads `awaiting`;
/// the old session is dead (409 `unknown-host`, and a secret from
/// before the restart does not open the new session); the agent's hello is admitted, and the
/// job then lives on the live host and its end, reported by the agent, is accepted. It
/// catches a restart that drops the host, one that lets a new job take the awaiting host,
/// and an old session that survives the process. It runs with the bindings in files and in
/// the state store, and catches in the store a restart that loses the host or its session
/// handling because the binding was not read back.
#[tokio::test]
async fn restart_keeps_a_registered_host_as_unknown_until_rehello() {
    for kept in Kept::BOTH {
        restart_keeps_a_registered_host_as_unknown_until_rehello_in(kept).await;
    }
}

async fn restart_keeps_a_registered_host_as_unknown_until_rehello_in(kept: Kept) {
    let first = start_job_hosts_in("agents_restart_lease", kept).await;
    let old = job_host(&first, "burst-1").await;
    let allocation = running_on(&first, &old, "job-1").await;
    taint(&first, &old).await;

    let h = restarted_job_hosts(first).await;
    assert_eq!(Kept::of(&h), kept);
    assert!(kept_bindings(&h).contains("burst-1"));
    if kept == Kept::Store {
        assert!(!agent_file(&h.scratch).exists());
    }
    let waiting = entry(&hosts(&h).await, "burst-1");
    assert_eq!(
        (waiting["state"].clone(), waiting["lost"].clone()),
        (json!("awaiting"), json!(false))
    );
    h.ticks.step().await;
    assert_eq!(job_state(&h, "job-1").await, "JOB_STATE_RUNNING");

    let heard = beat(&h, &old).await;
    assert_eq!(heard.status, 409, "{}", heard.body);
    assert_eq!(heard.json(), json!({ "error": "unknown-host" }));
    let back = job_host(&h, "burst-1").await;
    assert_eq!(back.epoch, 1);
    assert_ne!(back.secret, old.secret);
    let replayed = beat(&h, &old).await;
    assert_eq!(replayed.json(), json!({ "error": "session-mismatch" }));
    let listed_again = beat_with(&h, &back, &[running("job-1")]).await;
    assert_eq!(commands(&listed_again), (vec![], vec![]), "{listed_again}");
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "live");

    h.ticks.step().await;
    assert_eq!(job_state(&h, "job-1").await, "JOB_STATE_RUNNING");
    let ended = beat_with(&h, &back, &[exited("job-1", &allocation)]).await;
    assert_eq!(
        ended["job_acks"],
        json!([{ "job_id": "job-1", "ack": "ACCEPTED" }])
    );
    assert_eq!(job_state(&h, "job-1").await, "JOB_STATE_SUCCEEDED");
}

/// Asserts that a tainted registered host keeps its binding across a restart: after it, a
/// hello for the host from another join token is refused 403 `host-bound` and the host
/// awaits its own agent, whose hello (the token the host was admitted with) is then
/// admitted. It catches a restart that forgets the binding, so that the first hello from
/// any holder of a join token takes the host. It runs with the bindings in files and in the
/// state store, and catches in the store a binding that is not read back at the start.
#[tokio::test]
async fn restart_keeps_the_binding_so_another_token_cannot_take_the_host() {
    for kept in Kept::BOTH {
        restart_keeps_the_binding_so_another_token_cannot_take_the_host_in(kept).await;
    }
}

async fn restart_keeps_the_binding_so_another_token_cannot_take_the_host_in(kept: Kept) {
    let first = start_job_hosts_in("agents_restart_binding", kept).await;
    let host = job_host(&first, "burst-1").await;
    taint(&first, &host).await;

    let h = restarted_job_hosts(first).await;
    if kept == Kept::Store {
        assert!(!agent_file(&h.scratch).exists());
    }
    let intruder = post(&h, "/v1/agent/hello", Some(JOIN_B), &job_hello("burst-1")).await;
    assert_eq!(intruder.status, 403, "{}", intruder.body);
    assert_eq!(intruder.json(), json!({ "error": "host-bound" }));
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "awaiting");

    let owner = post(&h, "/v1/agent/hello", Some(JOIN), &job_hello("burst-1")).await;
    assert_eq!(owner.status, 200, "{}", owner.body);
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "live");
}

/// Asserts that an instance identity is kept with the binding: a host admitted with an
/// instance is, after a restart, refused for the same token under another instance and
/// under none (403 `host-bound`), and admitted under its own. It catches a binding
/// persisted without its instance, which a token holder could then step around. It runs
/// with the bindings in files and in the state store, and catches in the store a binding
/// row that leaves out the instance.
#[tokio::test]
async fn restart_keeps_the_instance_of_a_binding() {
    for kept in Kept::BOTH {
        restart_keeps_the_instance_of_a_binding_in(kept).await;
    }
}

async fn restart_keeps_the_instance_of_a_binding_in(kept: Kept) {
    let first = start_job_hosts_in("agents_restart_instance", kept).await;
    let mut body = job_hello("burst-1");
    body["instance"] = json!({ "provider": "aws", "instance_id": "i-0abc" });
    let admitted = post(&first, "/v1/agent/hello", Some(JOIN), &body).await;
    let host = session(&admitted, "burst-1");
    taint(&first, &host).await;

    assert!(kept_bindings(&first).contains("i-0abc"));
    let h = restarted_job_hosts(first).await;
    if kept == Kept::Store {
        assert!(!agent_file(&h.scratch).exists());
    }
    let mut other = body.clone();
    other["instance"]["instance_id"] = json!("i-0def");
    for refused in [other, job_hello("burst-1")] {
        let reply = post(&h, "/v1/agent/hello", Some(JOIN), &refused).await;
        assert_eq!(reply.status, 403, "{}", reply.body);
        assert_eq!(reply.json(), json!({ "error": "host-bound" }));
    }
    let own = post(&h, "/v1/agent/hello", Some(JOIN), &body).await;
    assert_eq!(own.status, 200, "{}", own.body);
}

/// Asserts that an awaiting host whose agent never returns is forgotten once the restored
/// lease on it has ended and a grace period and the retention have passed since the restart that
/// seated it: it leaves the listing, its binding leaves the agent file at the tick, and
/// its place in the pool is free for new hosts. Before that it is still listed, bound and
/// counted. It catches an awaiting host that keeps its record, binding and `max_machines`
/// slot until the next restart. It runs with the bindings in files and in the state store
/// (where the binding row is deleted at the tick and no agent file is ever written), and
/// catches in the store a binding row that outlives its host.
#[tokio::test]
async fn an_awaiting_host_that_never_returns_is_forgotten_with_its_binding() {
    for kept in Kept::BOTH {
        an_awaiting_host_that_never_returns_is_forgotten_with_its_binding_in(kept).await;
    }
}

async fn an_awaiting_host_that_never_returns_is_forgotten_with_its_binding_in(kept: Kept) {
    let first = start_agents_in("agents_awaiting_forgotten", false, kept).await;
    join(&first, "burst-1").await;
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    std::fs::write(scratch.state(), lease_file(&[(1, "held", Some("burst-1"))])).unwrap();
    let args = kept.args(&scratch, agent_args(&scratch, false));
    let h = Harness::with(scratch, replace(args, "--boot", "resume"), None).await;
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "awaiting");
    h.clock.advance(secs(7201));
    h.ticks.step().await;
    assert_eq!(listed(&hosts(&h).await), ["burst-1"]);
    assert!(kept_bindings(&h).contains("burst-1"));

    h.clock.advance(secs(86_400));
    h.ticks.step().await;
    assert!(listed(&hosts(&h).await).is_empty());
    let text = kept_bindings(&h);
    assert!(!text.contains("burst-1"), "{text}");
    if kept == Kept::Store {
        assert!(!agent_file(&h.scratch).exists());
    }
    join(&h, "burst-2").await;
    join(&h, "burst-3").await;
}

/// Asserts what the file of bindings is: written when a hello admits a host, mode 0600,
/// holding a digest of the token and never the token, rewritten (a new file) only when
/// the bindings change, and left alone by a tick or a heartbeat. It catches a file other
/// users can read, one that holds the secret, and a rewrite on every tick or heartbeat.
#[tokio::test]
async fn the_binding_file_is_private_and_written_only_on_change() {
    let h = start_agents("agents_binding_file", false).await;
    let path = agent_file(&h.scratch);
    assert!(!path.exists());
    let a = join(&h, "burst-1").await;
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("version = 1"), "{text}");
    assert!(text.contains("id = \"burst-1\""), "{text}");
    assert!(
        text.contains(&Credential::of_token(JOIN).to_hex()),
        "{text}"
    );
    assert!(!text.contains(JOIN), "{text}");

    h.ticks.step().await;
    assert_eq!(beat(&h, &a).await.status, 200);
    h.ticks.step().await;
    assert_eq!(fs::metadata(&path).unwrap().ino(), meta.ino());

    let second = hello_with(&h, JOIN_B, "burst-2").await;
    assert_eq!(second.status, 200);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("burst-2"), "{text}");
    assert!(
        text.contains(&Credential::of_token(JOIN_B).to_hex()),
        "{text}"
    );
    assert_ne!(fs::metadata(&path).unwrap().ino(), meta.ino());
    assert!(!fs::read_dir(h.scratch.dir.as_path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".agents.tmp")
    }));
}

/// Asserts that a binding that cannot be written does not undo the hello: the host stays
/// admitted, the failure is counted (and retried at each tick, counted again), and once
/// the cause is gone the next tick writes the file without another error. It catches a
/// failed write that is silent, one that refuses a host that was admitted, and one that
/// is never tried again.
#[tokio::test]
async fn a_failed_binding_write_is_counted_and_retried_at_the_next_tick() {
    let h = start_agents("agents_binding_failure", false).await;
    let blocker = h.scratch.path("leases.toml.agents.tmp");
    fs::create_dir(&blocker).unwrap();
    let me = join(&h, "burst-1").await;
    assert_eq!(h.running.agent_errors(), 1);
    assert!(!agent_file(&h.scratch).exists());
    assert_eq!(beat(&h, &me).await.status, 200);
    h.ticks.step().await;
    assert_eq!(h.running.agent_errors(), 2);

    fs::remove_dir(&blocker).unwrap();
    h.ticks.step().await;
    assert_eq!(h.running.agent_errors(), 2);
    let text = fs::read_to_string(agent_file(&h.scratch)).unwrap();
    assert!(text.contains("id = \"burst-1\""), "{text}");
}

/// Starts a server over `args` and returns the refusal.
async fn refused(args: Vec<std::ffi::OsString>) -> StartupError {
    let (_, ticker) = channel_ticker();
    let clock = SharedClock::new(js_lease::ManualClock::default());
    let started = start(config(args).unwrap(), clock, ticker).await;
    started.err().expect("the start should be refused")
}

/// Asserts that a file of bindings that is there and cannot be trusted refuses the start,
/// naming `--state-file`, instead of starting without it: one other users can read, one
/// that is not text, one that is not TOML of this shape, one that cannot even be examined
/// (a link that loops). A missing file, and one with keys and a version from the future,
/// start. It catches a start that quietly drops the protection of the bindings.
#[tokio::test]
async fn a_binding_file_that_cannot_be_trusted_refuses_the_start() {
    let scratch = Scratch::new("agents_binding_refused");
    let args = agent_args(&scratch, false);
    let path = agent_file(&scratch);
    let digest = Credential::of_token(JOIN).to_hex();
    let good = format!("version = 1\n[[host]]\nid = \"burst-1\"\ncredential = \"{digest}\"\n");

    fs::write(&path, &good).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let loose = refused(args.clone()).await;
    assert_eq!(loose.flag(), "--state-file");
    assert!(loose.reason().contains("0600"), "{}", loose.reason());

    for bytes in [
        &[0xff, 0xfe, 0x00][..],
        b"version = [",
        b"version = 1\n[[host]]\nid = 7\n",
    ] {
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let err = refused(args.clone()).await;
        assert_eq!(err.flag(), "--state-file");
        assert!(
            err.reason().contains("not a valid agent state file"),
            "{}",
            err.reason()
        );
    }

    fs::remove_file(&path).unwrap();
    symlink(&path, &path).unwrap();
    assert_eq!(refused(args.clone()).await.flag(), "--state-file");
    fs::remove_file(&path).unwrap();

    let future =
        format!("{good}note = \"x\"\n").replace("version = 1", "version = 7\nfuture = true");
    fs::write(&path, future).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let _h = Harness::with(scratch, args, None).await;
}

/// Asserts that a server started without the agent flags over the files of one that had them
/// starts, leaves the agent file as it was, and serves its host list. It catches a server
/// that cannot be started back without agents.
#[tokio::test]
async fn a_server_started_without_agents_leaves_the_agent_file_as_it_was() {
    let first = start_agents("agents_compat", false).await;
    join(&first, "burst-1").await;
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let before = fs::read(agent_file(&scratch)).unwrap();

    let args = replace(base_args(&scratch), "--boot", "resume");
    let plain = Harness::with(scratch, args, None).await;
    assert_eq!(fs::read(agent_file(&plain.scratch)).unwrap(), before);
    assert_eq!(plain.call("GET", "/v1/hosts", None).await.status, 200);
}

/// Asserts what is done with bookings of the lease file that no source can seat: two
/// leases on one static host (the lower id keeps it) and two on one unknown host (seated
/// on the registry as an awaiting host, where the lower id keeps it) leave two leases on
/// the hosts, log the other two by id, and list the unknown host as awaiting. It catches a booking
/// handed to the wrong source, and one lost without a word.
#[tokio::test]
async fn bookings_nobody_can_seat_are_logged_and_dropped() {
    let logs = LogCapture::start();
    let scratch = Scratch::new("agents_unseatable");
    let leases = [
        (1, "held", Some("gpu-01")),
        (2, "held", Some("gpu-01")),
        (3, "held", Some("ghost-1")),
        (4, "held", Some("ghost-1")),
    ];
    fs::write(scratch.state(), lease_file(&leases)).unwrap();
    let args = replace(agent_args(&scratch, true), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    let listing: Value = hosts(&h).await;
    assert!(entry(&listing, "gpu-02").get("lease").is_none());
    assert_eq!(entry(&listing, "gpu-01")["lease"], "1-1");
    assert_eq!(entry(&listing, "ghost-1")["state"], "awaiting");
    assert_eq!(entry(&listing, "ghost-1")["lease"], "1-3");
    let text = logs.text();
    for dropped in ["lease=1-2", "lease=1-4"] {
        assert!(text.contains(dropped), "{dropped}: {text}");
    }
    for kept in ["lease=1-1", "lease=1-3"] {
        assert!(!text.contains(kept), "{kept}: {text}");
    }
}
