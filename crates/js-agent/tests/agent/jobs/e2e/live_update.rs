//! An update of the agent on a busy host against the real server, as hosts are rolled: jobs
//! run and one waits for room, the update hook's request (the handover file) and its signal
//! hand the runs over, and the next process of the agent starts over the same flags, machine
//! and state root. In place, the program runs again over the same process (the exec, which the
//! harness cannot do in a test process); here the old run ends and the next one is started by
//! the test, which is the same start of the program. The server's grace is the default 15 s,
//! and the server's clock moves only by hand: by the seconds the exec and the next start take,
//! and by every wait the next process asks for before its hello is taken, as the farm's would.
//! The host is read from the server at each of those points.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::Handle;
use js_agent::lifecycle::handover;
use serde_json::Value;

use super::{Announcing, INTERVAL, Poll, RUNNING, Rig, rig_over};
use crate::jobs::fake::FakeMachine;
use crate::server::{Agent, Redirect, Server};
use crate::support::{Scratch, code, linux_host};

/// [`crate::server::POOL_RUNS_QUICK_TOML`] with the default grace, 15 s, the one the farm's
/// pool has: a host not heard for longer reads lost.
const POOL_GRACE_15S_TOML: &str = r#"
heartbeat_interval = "250ms"
grace = "15s"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296

[capabilities]
os = "linux"
arch = "x86_64"
"#;

/// The grace of [`POOL_GRACE_15S_TOML`].
const GRACE: Duration = Duration::from_secs(15);

/// The seconds between the old program's last heartbeat and the next program's first hello:
/// the longest the farm's hand-overs in place took, 6.0 s (from the old agent's `handed over`
/// to the next process's first hello, over 39 hand-overs of five hosts).
const EXEC_GAP: Duration = Duration::from_secs(6);

/// The most waits the next process may ask for before its hello is taken: a guard against a
/// hang, far past the backoff that outlasts the grace.
const HELLOS: usize = 12;

/// How the update hook signals the agent: SIGTERM after it wrote the handover file (stop and
/// start), or SIGHUP (in place).
#[derive(Debug, Clone, Copy)]
enum Way {
    StopStart,
    InPlace,
}

/// The host's state and phase in the server's listing.
fn read(server: &Server) -> (Value, Value) {
    let host = server.host("h1");
    (host["state"].clone(), host["phase"].clone())
}

/// A server with the farm's grace and one agent that runs containers on the pool's 4 cores
/// (the flag lowers what is detected), said hello.
fn busy_rig(name: &str) -> Rig {
    let server = Server::start_pool(name, &[], POOL_GRACE_15S_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    rig_over(server, address, name, &["--cpu-millis", "4000"], Redirect)
}

/// Fills the host: three jobs of a core run, and one of two cores waits for room. Returns the
/// running jobs and the waiting one.
fn load(rig: &Rig) -> (Vec<String>, String) {
    let running: Vec<String> = (0..3)
        .map(|_| {
            let job = rig.queued(1000);
            rig.until_running(&job);
            job
        })
        .collect();
    let waiting = rig.queued(2000);
    rig.tick();
    assert_eq!(rig.view(&waiting).0, "JOB_STATE_QUEUED", "no room for it");
    (running, waiting)
}

/// What is left of a rig once its agent has handed its runs over and ended: the server, the
/// host's machine and files, and the flags its next process is started with.
struct Handed {
    server: Server,
    address: Arc<Mutex<String>>,
    flags: Vec<String>,
    machine: Arc<FakeMachine>,
    files: Scratch,
}

/// Hands the runs of the rig's agent over the `way` the update hook does, and waits for that
/// process to end.
fn hand_over(rig: Rig, way: Way) -> Handed {
    handover::write(&rig.files.path("state")).unwrap();
    match way {
        Way::StopStart => rig.agent.signal(),
        Way::InPlace => rig.agent.replace(),
    }
    let Rig {
        server,
        agent,
        machine,
        address,
        flags,
        files,
        ..
    } = rig;
    let ran = agent.finish();
    let said = "js-agent: handed over: every run is left as it is to the next process";
    assert!(ran.err.contains(said), "{way:?}: {}", ran.err);
    match way {
        Way::StopStart => assert_eq!(ran.status(), code(0), "{}", ran.err),
        // The harness's exec fails, so the old run ends there: the next program is started by
        // the test, as the exec would have run it.
        Way::InPlace => assert!(
            ran.err
                .contains("js-agent: handed over in place: running js-agent again"),
            "{}",
            ran.err
        ),
    }
    assert!(
        !machine
            .world
            .log()
            .iter()
            .any(|call| call.starts_with("stop ")),
        "{way:?}: {:?}",
        machine.world.log()
    );
    Handed {
        server,
        address,
        flags,
        machine,
        files,
    }
}

/// Starts the next process of the rig's host over the same flags, machine and state root, and
/// lets every wait it asks for before its hello is taken pass on the server's clock too, the
/// host read live at each; returns the agent, its worker and its polls, and how many hellos the
/// server refused.
fn next_process(
    server: &Server,
    address: &Arc<Mutex<String>>,
    flags: &[String],
    machine: &Arc<FakeMachine>,
) -> (Agent, Handle, Receiver<Poll>, usize) {
    let (events, polls) = channel();
    let dispatch = Announcing {
        inner: Redirect(address.clone()),
        events: Mutex::new(events),
    };
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let agent = Agent::spawn_polling(
        server.url(),
        server.join_file(),
        &flags,
        linux_host,
        (Redirect(address.clone()), Arc::new(dispatch)),
        machine.clone(),
    );
    let worker = agent.worker();
    let mut refused = 0;
    loop {
        // The wait after a taken hello is the interval; after a refused one, the backoff.
        let wait = agent.next_wait();
        if wait == INTERVAL {
            break;
        }
        refused += 1;
        assert!(refused <= HELLOS, "the hello is never taken");
        server.clock.advance(wait);
        assert_eq!(
            read(server).0,
            "live",
            "the host reads lost while its agent hands over, after {refused} hellos refused"
        );
        agent.elapse();
    }
    (agent, worker, polls, refused)
}

/// Asserts that an update under load, stop-and-start and in place, leaves the host live in
/// every read of the server and ends no job: the host reads live and draining once the old
/// agent has handed over, live after the seconds the exec takes, live at every wait of the next
/// process; the next process's first hello is taken (the server refuses none `host-live`) and
/// the host serves again; every job that ran runs on, in its first attempt, and its result is
/// the job's own; the job that waited is placed on the host by the next process once a running
/// one ends; nothing is started twice. Catches a next process that says hello as another agent
/// process than the one that handed over, which the server refuses until the old session's
/// grace has run out, so that the host reads lost and takes no job for that long on every roll.
#[test]
fn an_update_under_load_keeps_the_host_live_in_every_read_and_ends_no_job() {
    for (name, way) in [
        ("jobs_e2e_live_stop", Way::StopStart),
        ("jobs_e2e_live_place", Way::InPlace),
    ] {
        let rig = busy_rig(name);
        let (running, waiting) = load(&rig);
        let Handed {
            server,
            address,
            flags,
            machine,
            files,
        } = hand_over(rig, way);
        assert_eq!(read(&server), ("live".into(), "draining".into()), "{way:?}");

        server.clock.advance(EXEC_GAP);
        assert_eq!(read(&server).0, "live", "{way:?}");
        let (agent, worker, polls, refused) = next_process(&server, &address, &flags, &machine);
        assert_eq!(
            refused, 0,
            "{way:?}: hellos refused before the next one was taken"
        );
        let rig = Rig {
            polls,
            server,
            agent,
            worker,
            machine,
            address,
            flags,
            files,
        };
        assert_eq!(
            read(&rig.server),
            ("live".into(), "serving".into()),
            "{way:?}"
        );

        // The first heartbeats list the runs the next process adopted.
        rig.tick();
        rig.tick();
        assert_eq!(read(&rig.server).0, "live", "{way:?}");
        for job in &running {
            assert_eq!(rig.view(job), (RUNNING.to_owned(), None), "{way:?}: {job}");
            assert_eq!(rig.server.job(job)["attempt"], 1, "{way:?}: {job}");
        }
        assert_eq!(rig.view(&waiting).0, "JOB_STATE_QUEUED", "{way:?}");

        // A job ends on the next process; the one that waited is placed on the host.
        rig.machine.world.exit(&running[0], 0);
        assert!(rig.worker.settle());
        rig.tick();
        assert_eq!(
            rig.view(&running[0]),
            (
                "JOB_STATE_SUCCEEDED".into(),
                Some("RESULT_REASON_EXIT".into())
            ),
            "{way:?}"
        );
        rig.until_running(&waiting);
        assert_eq!(read(&rig.server).0, "live", "{way:?}");
        let mut all: Vec<&str> = running.iter().map(String::as_str).collect();
        all.push(&waiting);
        assert_eq!(rig.machine.world.created(), all, "{way:?}: nothing twice");

        for job in running.iter().skip(1).chain([&waiting]) {
            rig.machine.world.exit(job, 0);
        }
        assert!(rig.worker.settle());
        rig.tick();
        rig.tick();
        assert!(
            rig.worker.snapshot().results.is_empty(),
            "{way:?}: acknowledged"
        );
        let ran = rig.agent.stop();
        assert_eq!(ran.status(), code(0), "{way:?}: {}", ran.err);
    }
}

/// Asserts the bound the hand-over keeps: a host whose agent handed over and whose next process
/// never says hello reads live for the grace after the last heartbeat and lost the moment it
/// has passed, as any host that falls silent, and its job is not ended for that. Catches a
/// hand-over that holds a dead host live past the grace.
#[test]
fn a_host_whose_next_process_never_comes_reads_lost_once_the_grace_has_passed() {
    let rig = busy_rig("jobs_e2e_live_never");
    let job = rig.queued(1000);
    rig.until_running(&job);
    let Handed { server, .. } = hand_over(rig, Way::InPlace);
    server.clock.advance(GRACE);
    assert_eq!(read(&server).0, "live", "the grace has not passed");
    server.clock.advance(Duration::from_millis(1));
    assert_eq!(read(&server).0, "lost", "the grace has passed");
    assert_eq!(server.job_state(&job), RUNNING, "silence ends no job");
}
