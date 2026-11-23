//! A server's start does not wait for the teaching of the job sizes from the ended jobs
//! ([`crate::job_sizes`]): it answers ready with the teaching held, and the teaching runs after.
//! Nothing waits on time: the clocks are manual, and the teaching is held by a channel.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{Booked, EndReason, JobEnd, JobId, JobState, NewJob, Output, ResourceUsage};
use js_testkit::dir::TestDir;
use js_types::{HostId, ManualClock};
use tokio::sync::oneshot;

use super::{Running, start_held};
use crate::agent_api::job_rig::spec;
use crate::clock::SharedClock;
use crate::config::{Cli, Config};
use crate::ready::Phase;
use crate::ticker::channel_ticker;

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/server_sizes"),
        name,
    )
}

/// The configuration of a server with the job service on, its files in `dir`, its job store's
/// clock `store_clock`.
fn config(dir: &TestDir, store_clock: &Arc<js_state::ManualClock>) -> Config {
    let write = |file: &str, text: &str, mode: u32| {
        let path = dir.path().join(file);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    };
    let pool = "name = \"build\"\nclass = \"build-host\"\nmax_machines = 2\n[shape]\ncpu_millis = 8000\nmemory_bytes = 17179869184\n[capabilities]\narch = \"x86_64\"\n";
    let token = write("token", "test-token-0123456789-abcdefghijkl", 0o600);
    let joins = write(
        "agent-tokens",
        "join-token-A-0123456789-abcdefghijklmnop\n",
        0o600,
    );
    let pool = write("agent-pool.toml", pool, 0o644);
    let args = [
        "js-lease-server",
        "--listen",
        "127.0.0.1:0",
        "--boot",
        "resume",
        "--job-listen",
        "127.0.0.1:0",
    ]
    .map(String::from)
    .into_iter()
    .chain(
        [
            ("--state-file", dir.path().join("state")),
            ("--token-file", token),
            ("--agent-token-file", joins),
            ("--agent-pool-config", pool),
        ]
        .into_iter()
        .flat_map(|(flag, path)| [flag.to_owned(), path.display().to_string()]),
    );
    Config::from_cli(Cli::try_parse_from(args).unwrap())
        .unwrap()
        .with_job_sources(store_clock.clone(), Arc::new(js_jobs::RandomIds))
}

/// A server of `config` started with its teaching of the job sizes held until `held` is sent
/// or dropped (`None`: not held).
async fn started(config: Config, held: Option<oneshot::Receiver<()>>) -> Running {
    let clock = SharedClock::new(ManualClock::default());
    let (_ticks, ticker) = channel_ticker();
    let (_checks, checks) = channel_ticker();
    let (_retries, retry) = channel_ticker();
    start_held(config, clock, (ticker, checks, retry), held)
        .await
        .unwrap()
}

/// Runs the job `name` on the tables of `running` to its exit, with a measured 512 MiB peak.
fn ran(running: &Running, store_clock: &js_state::ManualClock, name: &str) {
    let jobs = running.shared.agents().jobs.clone().unwrap();
    let id = JobId::parse(name).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    let booked = Booked::Shape {
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    };
    jobs.place(&id, HostId::new("a").unwrap(), booked).unwrap();
    for (from, to) in [
        (JobState::Placed, JobState::Starting),
        (JobState::Starting, JobState::Running),
    ] {
        jobs.transition(&id, from, to).unwrap();
    }
    store_clock.advance(Duration::from_secs(60));
    let usage = ResourceUsage {
        cpu_time: Duration::from_secs(40),
        peak_memory_bytes: 1 << 29,
        cpu_throttle: None,
        cpu_pressure_some: None,
        cpu_peak: None,
    };
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(usage),
            ..Output::default()
        },
    };
    jobs.end_and_free(&id, end, EndReason::Exited).unwrap();
}

/// Asserts that a server over a job store that holds two measured ended jobs answers ready
/// while its teaching of the job sizes has not run (it is held), that the teaching runs once it
/// is let go and teaches both runs, and that the server then stops in order. The first start,
/// over an empty store, teaches none without being held. Catches a start that reads the ended
/// jobs before it answers ready, which held a server's start for the minutes its reads of a
/// long history took, and a teaching that never runs after the start.
#[tokio::test]
async fn a_server_answers_ready_before_its_sizes_are_taught_and_teaches_them_after() {
    let dir = scratch("ready_before_taught");
    let store_clock = Arc::new(js_state::ManualClock::default());
    let mut first = started(config(&dir, &store_clock), None).await;
    let taught = first.sizes.take().unwrap().await.unwrap();
    assert_eq!(taught, 0, "an empty store");
    ran(&first, &store_clock, "one");
    ran(&first, &store_clock, "two");
    first.shutdown().await.unwrap();
    let (hold, held) = oneshot::channel();
    let mut running = started(config(&dir, &store_clock), Some(held)).await;
    assert_eq!(running.gate.phase(), Phase::Ready);
    let sizes = running.sizes.take().unwrap();
    assert!(!sizes.is_finished(), "ready with the sizes untaught");
    hold.send(()).unwrap();
    assert_eq!(sizes.await.unwrap(), 2);
    running.shutdown().await.unwrap();
}
