//! Run files and the `run-files` capability: a start that carries files goes only to a
//! host whose hello said it delivers them. An older agent ignores the field, so sending it
//! the start would run the job without the secrets it needs.

mod support;

use js_agent_wire::{Command, Hello, Isolation, JobSpec, Limits, RunFile};
use js_caps::{FLAG_SET, RUN_FILES};
use js_provider::Provider;
use js_registry::RunRefusal;
use support::*;

fn job_with_files(run: &str, files: Vec<RunFile>) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        native: None,
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        files,
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
    }
}

fn secret() -> Vec<RunFile> {
    vec![RunFile {
        name: "put-url".into(),
        contents: "https://store.example/put?sig=abc".into(),
    }]
}

fn hello_with(capability: Option<&str>) -> Hello {
    let mut hello = hello("a");
    if let Some(value) = capability {
        hello.capabilities.insert(RUN_FILES.into(), value.into());
    }
    hello
}

/// A fixture with host `a` leased by lease 1, its hello reporting `capability` for
/// `run-files`. Returns the fixture and the epoch.
fn leased(capability: Option<&str>) -> (Fixture, u64) {
    let fx = fixture(4);
    let epoch = fx.hello(&hello_with(capability), &cred("t")).unwrap().epoch;
    assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
    (fx, epoch)
}

fn beat(fx: &Fixture, epoch: u64) -> Vec<Command> {
    let message = fx.heartbeat("a", epoch);
    fx.registry
        .heartbeat(&message, &cred("t"))
        .unwrap()
        .commands
}

/// Asserts that a run with files is refused for a host that reported no `run-files`, and
/// for one that reported anything but the flag value, with nothing queued; and that the
/// same job without files, and the job with files for a host that reports the flag, are
/// queued. It catches a server that would hand a secret-carrying start to an agent that
/// ignores the field, which would run the task without its URL.
#[test]
fn files_are_queued_only_for_a_host_that_reports_run_files() {
    for capability in [None, Some("false"), Some("1")] {
        let (fx, _) = leased(capability);
        let refused = fx
            .registry
            .queue_run(&host("a"), &lease(1), job_with_files("r1", secret()));
        assert_eq!(refused.unwrap_err(), RunRefusal::FilesUnsupported);
        assert_eq!(RunRefusal::FilesUnsupported.code(), "files-unsupported");
        assert!(fx.registry.runs_of(&lease(1)).is_empty(), "{capability:?}");
        let plain = fx
            .registry
            .queue_run(&host("a"), &lease(1), job_with_files("r2", Vec::new()));
        assert!(plain.is_ok(), "{capability:?}");
    }
    let (fx, epoch) = leased(Some(FLAG_SET));
    let spec = job_with_files("r1", secret());
    fx.registry.queue_run(&host("a"), &lease(1), spec).unwrap();
    let sent = beat(&fx, epoch);
    let [Command::Start(start)] = &sent[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(start.files, secret());
}
