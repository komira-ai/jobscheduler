//! A restart over the same files with registered hosts: the leases, the bindings, and
//! the file the bindings are kept in.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::Duration;

use js_lease_server::{SharedClock, StartupError, channel_ticker, start};
use js_registry::Credential;
use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, JOIN_B, agent_args, beat, entry, grant, hello_body, hello_with, hosts, join, listed,
    post, restarted, session, start_agents,
};
use crate::support::{
    Harness, LogCapture, Scratch, base_args, config, ids, lease_body, lease_file, replace,
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

/// Asserts what a restart does to a lease on a registered host. The lease stays held on
/// the same host and is not freed by ticks while the host is not heard; the host reads
/// `awaiting` and takes no new lease; the old session is dead (409 `unknown-host`, and a
/// secret from before the restart does not open the new session); the agent's hello is
/// admitted, the lease then lives on the live host and can be renewed and released. It
/// catches a restart that drops the lease, one that lets a new lease take the awaiting
/// host, and an old session that survives the process.
#[tokio::test]
async fn restart_keeps_lease_on_registered_host_as_unknown_until_rehello() {
    let first = start_agents("agents_restart_lease", false).await;
    let old = join(&first, "burst-1").await;
    let granted = grant(&first, &[&old]).await;
    assert_eq!(granted["host"], "burst-1");

    let h = restarted(first, false).await;
    let leases = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(leases["leases"][0]["id"], "1-1");
    assert_eq!(leases["leases"][0]["state"], "held");
    assert_eq!(leases["leases"][0]["host"], "burst-1");
    let waiting = entry(&hosts(&h).await, "burst-1");
    assert_eq!(
        (waiting["state"].clone(), waiting["lost"].clone()),
        (json!("awaiting"), json!(false))
    );
    assert_eq!(waiting["lease"], "1-1");
    h.ticks.step().await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), ["1-1"]);
    let none = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(none.json(), json!({ "error": "no-capacity" }));

    let heard = beat(&h, &old).await;
    assert_eq!(heard.status, 409, "{}", heard.body);
    assert_eq!(heard.json(), json!({ "error": "unknown-host" }));
    let back = join(&h, "burst-1").await;
    assert_eq!(back.epoch, 1);
    assert_ne!(back.secret, old.secret);
    let replayed = beat(&h, &old).await;
    assert_eq!(replayed.json(), json!({ "error": "session-mismatch" }));
    assert_eq!(beat(&h, &back).await.status, 200);
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "live");

    h.ticks.step().await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), ["1-1"]);
    let renewed = h.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    let released = h.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
    // The host books again after two heartbeats that follow the release.
    beat(&h, &back).await;
    beat(&h, &back).await;
    let again = grant(&h, &[&back]).await;
    assert_eq!(again["host"], "burst-1");
}

/// Asserts that a lease on a registered host keeps its binding across a restart: after
/// it, a hello for the host from another join token is refused 403 `host-bound` and the
/// lease stays with the host that awaits its own agent, whose hello (the token the host
/// was admitted with) is then admitted. It catches a restart that forgets the binding, so
/// that the first hello from any holder of a join token takes the lease.
#[tokio::test]
async fn restart_keeps_the_binding_so_another_token_cannot_take_the_lease() {
    let first = start_agents("agents_restart_binding", false).await;
    let me = join(&first, "burst-1").await;
    grant(&first, &[&me]).await;

    let h = restarted(first, false).await;
    let intruder = hello_with(&h, JOIN_B, "burst-1").await;
    assert_eq!(intruder.status, 403, "{}", intruder.body);
    assert_eq!(intruder.json(), json!({ "error": "host-bound" }));
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "awaiting");

    let owner = hello_with(&h, JOIN, "burst-1").await;
    assert_eq!(owner.status, 200, "{}", owner.body);
    assert_eq!(entry(&hosts(&h).await, "burst-1")["lease"], "1-1");
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), ["1-1"]);
}

/// Asserts that an instance identity is kept with the binding: a host admitted with an
/// instance is, after a restart, refused for the same token under another instance and
/// under none (403 `host-bound`), and admitted under its own. It catches a binding
/// persisted without its instance, which a token holder could then step around.
#[tokio::test]
async fn restart_keeps_the_instance_of_a_binding() {
    let first = start_agents("agents_restart_instance", false).await;
    let mut body = hello_body("burst-1");
    body["instance"] = json!({ "provider": "aws", "instance_id": "i-0abc" });
    let admitted = post(&first, "/v1/agent/hello", Some(JOIN), &body).await;
    let me = session(&admitted, "burst-1");
    grant(&first, &[&me]).await;

    let h = restarted(first, false).await;
    let mut other = body.clone();
    other["instance"]["instance_id"] = json!("i-0def");
    for refused in [other, hello_body("burst-1")] {
        let reply = post(&h, "/v1/agent/hello", Some(JOIN), &refused).await;
        assert_eq!(reply.status, 403, "{}", reply.body);
        assert_eq!(reply.json(), json!({ "error": "host-bound" }));
    }
    let own = post(&h, "/v1/agent/hello", Some(JOIN), &body).await;
    assert_eq!(own.status, 200, "{}", own.body);
}

/// Asserts that an awaiting host whose agent never returns is forgotten once its lease
/// has ended and a grace period and the retention have passed since the restart that
/// seated it: it leaves the listing, its binding leaves the agent file at the tick, and
/// its place in the pool is free for new hosts. Before that it is still listed, bound and
/// counted. It catches an awaiting host that keeps its record, binding and `max_machines`
/// slot until the next restart.
#[tokio::test]
async fn an_awaiting_host_that_never_returns_is_forgotten_with_its_binding() {
    let first = start_agents("agents_awaiting_forgotten", false).await;
    let me = join(&first, "burst-1").await;
    grant(&first, &[&me]).await;

    let h = restarted(first, false).await;
    let path = agent_file(&h.scratch);
    h.clock.advance(secs(601));
    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    assert_eq!(listed(&hosts(&h).await), ["burst-1"]);
    assert!(fs::read_to_string(&path).unwrap().contains("burst-1"));

    h.clock.advance(secs(86_400));
    h.ticks.step().await;
    assert!(listed(&hosts(&h).await).is_empty());
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("burst-1"), "{text}");
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

/// Asserts that the lease file is the same file with or without agents: a lease on a
/// registered host is recorded in it as on a static host, with nothing of the bindings
/// (no credential, no digest), so nothing in it needs the agent file to be read; and that
/// a server started without the agent flags over those same files starts, leaves the
/// agent file as it was, and (not knowing the host) drops the lease the table cannot
/// find. It catches a lease file that carries the agent side's data and a server that
/// cannot be started back without agents.
#[tokio::test]
async fn the_lease_file_is_unchanged_by_agents_in_both_directions() {
    let first = start_agents("agents_compat", false).await;
    let me = join(&first, "burst-1").await;
    grant(&first, &[&me]).await;
    let lease_text = fs::read_to_string(first.scratch.state()).unwrap();
    assert!(lease_text.contains("version = 1"), "{lease_text}");
    assert!(lease_text.contains("host = \"burst-1\""), "{lease_text}");
    let digest = Credential::of_token(JOIN).to_hex();
    assert!(!lease_text.contains(&digest), "{lease_text}");
    assert!(!lease_text.contains("credential"), "{lease_text}");
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let before = fs::read(agent_file(&scratch)).unwrap();

    let args = replace(base_args(&scratch), "--boot", "resume");
    let plain = Harness::with(scratch, args, None).await;
    assert!(ids(&plain.call("GET", "/v1/leases", None).await).is_empty());
    assert_eq!(fs::read(agent_file(&plain.scratch)).unwrap(), before);
    assert_eq!(plain.call("GET", "/v1/hosts", None).await.status, 200);
}

/// Asserts what is done with bookings of the lease file that no source can seat: two
/// leases on one static host (the lower id keeps it) and two on one unknown host (seated
/// on the registry as an awaiting host, where the lower id keeps it) leave two leases,
/// log the other two by id, and list the unknown host as awaiting. It catches a booking
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
    assert_eq!(
        ids(&h.call("GET", "/v1/leases", None).await),
        ["1-1", "1-3"]
    );
    let listing: Value = hosts(&h).await;
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
