//! A held agent against a real server in the test process: the hold file in the state root
//! makes the agent cordon its host through the lease server's own-host route, with the host
//! credential its first hello was answered with, and refuse the starts placed before the
//! cordon is in force. The agent's waits are opened one at a time by the test, the container
//! runtime is in memory, and the server's clock is moved by the test: nothing here waits for
//! a time to pass.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::StateRoot;
use js_agent::lifecycle::hold::{self, HOLD_FILE, HOLD_TEXT};
use js_agent::{Dialer, Handle};
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{private, user};
use crate::server::{Agent, POOL_RUNS_QUICK_TOML, Server, T0};
use crate::support::{Host, Ran, Scratch, code, linux_host};

/// The cordon the keeper asks for, in milliseconds: an hour.
const HOUR_MS: u64 = 3_600_000;

/// The state of a job that is running, as the job service names it.
const RUNNING: &str = "JOB_STATE_RUNNING";

/// A Linux host whose instance file says it runs on `lambdalabs:i-1`, so that its first
/// hello, made with a one-time token, is answered with a host credential.
fn launched() -> Host {
    let mut host = linux_host();
    host.files = host.files.with("/instance", "lambdalabs:i-1\n");
    host
}

/// A server and the host `h1` of a container agent admitted to it with a one-time token.
struct Held {
    server: Server,
    state: PathBuf,
    flags: Vec<String>,
    token: PathBuf,
    _files: Scratch,
}

impl Held {
    /// A server over the quick pool and the files of an agent, with the hold file written
    /// in its state root when `hold` is set; no agent is started.
    fn new(name: &str, hold: bool) -> Self {
        let server = Server::start_pool(name, &[], POOL_RUNS_QUICK_TOML);
        let files = Scratch::new(&format!("{name}-files"));
        let instance = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
        let minted = server.post_body("/v1/agent/join-tokens", &instance);
        assert_eq!(minted.status, 201, "{}", minted.body);
        let text = minted.body["token"].as_str().unwrap();
        let token = files.write("one-time-token", text, 0o600);
        let exec = files.write("docker", "#!/bin/sh\n", 0o755);
        let (user, _, _) = user();
        let state = private(&files, "state");
        let scratch = private(&files, "run-scratch");
        let flags = [
            "--host-id",
            "h1",
            "--instance-file",
            "/instance",
            "--allow-container-engine",
            "--container-bin",
            &exec.display().to_string(),
            "--container-user",
            &user,
            "--container-subuid",
            "jobmap",
            "--state-root",
            &state.display().to_string(),
            "--scratch-root",
            &scratch.display().to_string(),
            "--drain-wait",
            "0",
        ]
        .map(str::to_owned)
        .to_vec();
        let held = Self {
            server,
            state,
            flags,
            token,
            _files: files,
        };
        if hold {
            held.write_hold();
        }
        held
    }

    /// Writes the hold file of the state root, as the step that drains the host does.
    fn write_hold(&self) {
        hold::write(&StateRoot::open(&self.state).unwrap()).unwrap();
    }

    fn args(&self) -> Vec<&str> {
        self.flags.iter().map(String::as_str).collect()
    }

    /// Starts the agent with the one-time token and waits for its hello and what the agent
    /// does after it.
    fn start(&self) -> (Agent, Handle, Arc<FakeMachine>) {
        let machine = Arc::new(FakeMachine::new());
        let agent = Agent::spawn_on(
            self.server.url(),
            self.token.clone(),
            &self.args(),
            launched,
            Dialer,
            machine.clone(),
        );
        let worker = agent.worker();
        assert_eq!(agent.next_wait(), Duration::from_millis(250), "the hello");
        (agent, worker, machine)
    }

    /// What the server lists of the cordon of `h1`.
    fn cordon(&self) -> Value {
        self.server
            .host("h1")
            .get("cordon")
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// What the server lists of the taint of `h1`.
    fn taint(&self) -> Value {
        self.server
            .host("h1")
            .get("taint")
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// One heartbeat of `agent`, and its worker done with the reply to it.
    #[track_caller]
    fn beat(&self, agent: &Agent, worker: &Handle) {
        agent.elapse();
        agent.next_wait();
        assert!(worker.settle());
    }
}

/// The stop of an agent started with `--drain-wait 0` that holds one running job: the signal,
/// the heartbeat that stops the job, the one that sends its killed result, and the end.
/// Returns what the agent printed.
fn drain_now(agent: Agent, worker: &Handle) -> Ran {
    agent.signal();
    agent.next_wait();
    assert!(worker.settle());
    agent.elapse();
    agent.next_wait();
    assert!(worker.settle());
    agent.elapse();
    agent.finish()
}

/// Asserts that the cordon the server lists is the keeper's: it lapses an hour after the
/// server's clock reading, and names no holder.
#[track_caller]
fn assert_keepers_cordon(cordon: &Value) {
    assert_eq!(cordon["expires_at_ms"], T0 + HOUR_MS, "{cordon}");
    assert!(cordon["holder"].is_null(), "{cordon}");
}

/// Asserts that a host whose state root holds the hold file is cordoned right after its
/// first hello: with the hold file written before the agent starts, the server lists the
/// host cordoned for an hour with no holder once the hello's exchange is done; a job
/// submitted then stays queued across heartbeats and the runtime creates nothing; and the
/// agent's log says the hold began and the cordon was placed, in its own words. It catches a
/// keeper that is never stepped by the loop, one that asks the server before the host is
/// known, one that connects with a credential the own-host route refuses, and a held host
/// that is placed work.
#[test]
fn a_held_agent_cordons_its_host_right_after_its_hello() {
    let held = Held::new("hold_e2e_cordons", true);
    let (agent, worker, machine) = held.start();
    assert_keepers_cordon(&held.cordon());

    let job = held.server.submit(1000);
    assert_eq!(job.status, 201, "{}", job.body);
    let job = job.body["job"]["id"].as_str().unwrap().to_owned();
    for _ in 0..3 {
        held.beat(&agent, &worker);
    }
    assert_eq!(held.server.job_state(&job), "JOB_STATE_QUEUED");
    assert!(machine.world.created().is_empty());

    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let lines: Vec<&str> = ran.err.lines().collect();
    let begun = "js-agent: hold: this host is held (its hold file is there): it starts no work \
                 and keeps itself cordoned";
    assert_eq!(
        lines.iter().filter(|line| **line == begun).count(),
        1,
        "{}",
        ran.err
    );
    let placed = "js-agent: hold: cordoned host=h1 expires_in_ms=3600000";
    assert_eq!(
        lines.iter().filter(|line| **line == placed).count(),
        1,
        "{}",
        ran.err
    );
}

/// Asserts that the hold outlives the agent: after the first agent, held and cordoned, is
/// stopped and the cordon is lifted on the server by the operator route, an agent started
/// again over the same state root and credential (no token file) cordons the host again
/// after its hello, and the hold file is as it was. It catches a hold that lives in the
/// process and not in the state root, and a keeper that does not ask again at the first
/// hello of a new process.
#[test]
fn the_hold_survives_an_agent_restart_and_the_host_is_cordoned_again() {
    let held = Held::new("hold_e2e_restart", true);
    let (agent, _worker, _machine) = held.start();
    assert_keepers_cordon(&held.cordon());
    let ran: Ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);

    let lifted = held.server.job_post("/v1/hosts/h1:uncordon", &json!({}));
    assert_eq!(lifted.status, 200, "{}", lifted.body);
    assert!(held.cordon().is_null());
    held.server.clock.advance(Duration::from_secs(120));

    let again = Agent::spawn_tokenless(held.server.url(), &held.args(), launched, Dialer);
    assert_eq!(again.next_wait(), Duration::from_millis(250), "the hello");
    let cordon = held.cordon();
    assert_eq!(cordon["expires_at_ms"], T0 + 120_000 + HOUR_MS, "{cordon}");
    assert!(cordon["holder"].is_null(), "{cordon}");
    let text = fs::read_to_string(held.state.join(HOLD_FILE)).unwrap();
    assert_eq!(text, HOLD_TEXT);
    let ran = again.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts that a start placed before the cordon is in force is refused and does not taint:
/// with the agent serving, the hold file is written while it waits and a job is queued (the
/// server places it on the host); the beat that carries the start makes the agent refuse
/// it, creating nothing, and cordon the host in the same exchange; the next beat carries the
/// refusal, which the job API shows as a job failed to start with the detail `host-held`,
/// and the host is not tainted. It catches a hold file that is read only at the hello, a
/// start run on a held host, and a refusal that taints the host as `host-tainted` does.
#[test]
fn a_start_placed_before_the_cordon_is_refused_host_held_without_a_taint() {
    let held = Held::new("hold_e2e_refuses", false);
    let (agent, worker, machine) = held.start();
    assert!(held.cordon().is_null());
    held.write_hold();
    let job = held.server.submit(1000);
    assert_eq!(job.status, 201, "{}", job.body);
    let job = job.body["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(held.server.job_state(&job), "JOB_STATE_PLACED");

    held.beat(&agent, &worker);
    assert!(machine.world.created().is_empty());
    assert_keepers_cordon(&held.cordon());
    held.beat(&agent, &worker);
    assert!(machine.world.created().is_empty());
    let seen = held.server.job(&job);
    assert_eq!(seen["state"], "JOB_STATE_FAILED_TO_START", "{seen}");
    assert_eq!(seen["startFailureDetail"], "host-held", "{seen}");
    assert!(held.taint().is_null());
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts that a host that was never held is never cordoned: with no hold file, the server
/// lists no cordon after the hello and after more heartbeats, a job queued is started and
/// reads running, and the agent's log has no line about a hold. It catches a keeper that
/// cordons a host that is not held.
#[test]
fn a_host_that_was_never_held_is_never_cordoned() {
    let held = Held::new("hold_e2e_free", false);
    let (agent, worker, machine) = held.start();
    assert!(held.cordon().is_null());
    let job = held.server.submit(1000);
    assert_eq!(job.status, 201, "{}", job.body);
    let job = job.body["job"]["id"].as_str().unwrap().to_owned();
    for _ in 0..3 {
        held.beat(&agent, &worker);
    }
    assert_eq!(held.server.job_state(&job), RUNNING);
    assert_eq!(machine.world.created(), [job.as_str()]);
    assert!(held.cordon().is_null());
    let ran = drain_now(agent, &worker);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran.err.contains("js-agent: hold:"), "{}", ran.err);
}
