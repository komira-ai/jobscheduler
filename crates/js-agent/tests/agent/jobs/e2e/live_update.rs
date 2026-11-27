//! An update of the agent on a busy host against the real server, as hosts are rolled: jobs
//! run and one waits for room, the update hook's request (the handover file) and its signal
//! hand the runs over, and the next process of the agent starts over the same flags, machine
//! and state root. In place, the program runs again over the same process (the exec, which the
//! harness cannot do in a test process); here the old run ends and the next one is started by
//! the test, which is the same start of the program. The server's clock moves only by hand: by
//! the seconds the exec and the next start take, and by every wait the next process asks for
//! before its hello is taken, as the farm's would. The host is read from the server at each of
//! those points.
//!
//! The next process takes the id only when the process file was written no longer ago, by the
//! host's real clock, than the grace of the session that left it. So that no test races that
//! real time, the tests whose next process must take the id run on a pool whose grace is four
//! minutes, below the most the agent's default silence bound (300 s, with the 10 s stop grace)
//! accepts ([`POOL_GRACE_4M_TOML`]); the bound of the grace itself is asserted on the default 15 s
//! ([`POOL_GRACE_15S_TOML`]), where no next process starts.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::Handle;
use js_agent::lifecycle::handover;
use serde_json::Value;

use super::{Announcing, HELD, INTERVAL, Poll, RUNNING, Rig, rig_over};
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

/// [`POOL_GRACE_15S_TOML`] with a grace of four minutes: the real time a test takes between an
/// agent's end and the next one's start never passes it, and it is below what the agent's
/// default `--silence-stop` (300 s) and stop grace (10 s) accept.
const POOL_GRACE_4M_TOML: &str = r#"
heartbeat_interval = "250ms"
grace = "4m"
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

/// A server over the pool file `pool` and one agent that runs containers on the pool's 4 cores
/// (the flag lowers what is detected), with `extra` flags, said hello.
fn busy_rig(name: &str, pool: &'static str, extra: &[&str]) -> Rig {
    let server = Server::start_pool(name, &[], pool);
    let address = Arc::new(Mutex::new(server.url()));
    let mut flags = vec!["--cpu-millis", "4000"];
    flags.extend_from_slice(extra);
    rig_over(server, address, name, &flags, Redirect)
}

/// Ends every job of `jobs` on the host, has the results acknowledged and stops the agent,
/// which drains nothing.
fn finish(rig: Rig, jobs: &[&String]) {
    for job in jobs {
        rig.machine.world.exit(job, 0);
    }
    assert!(rig.worker.settle());
    rig.tick();
    rig.tick();
    assert!(rig.worker.snapshot().results.is_empty(), "acknowledged");
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
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
        let rig = busy_rig(name, POOL_GRACE_4M_TOML, &[]);
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

        let rest: Vec<&String> = running.iter().skip(1).chain([&waiting]).collect();
        finish(rig, &rest);
    }
}

/// Asserts the bound the hand-over keeps: a host whose agent handed over and whose next process
/// never says hello reads live for the grace after the last heartbeat and lost the moment it
/// has passed, as any host that falls silent, and its job is not ended for that. Catches a
/// hand-over that holds a dead host live past the grace.
#[test]
fn a_host_whose_next_process_never_comes_reads_lost_once_the_grace_has_passed() {
    let rig = busy_rig("jobs_e2e_live_never", POOL_GRACE_15S_TOML, &[]);
    let job = rig.queued(1000);
    rig.until_running(&job);
    let Handed { server, .. } = hand_over(rig, Way::InPlace);
    server.clock.advance(GRACE);
    assert_eq!(read(&server).0, "live", "the grace has not passed");
    server.clock.advance(Duration::from_millis(1));
    assert_eq!(read(&server).0, "lost", "the grace has passed");
    assert_eq!(server.job_state(&job), RUNNING, "silence ends no job");
}

/// The seconds a restart of the unit for a new flags file leaves the host unheard on the farm:
/// from the drained agent's end to the next process's first hello, at most 8.5 s on five hosts
/// (the config hook polls the stopped unit every 10 s before it starts it again).
const RESTART_GAP: Duration = Duration::from_secs(9);

/// The seconds the farm's agents could not reach the server while it was redeployed (their
/// heartbeats failed for about 8 s): the test lets the server's clock pass them while the
/// server restarts, the agent waiting meanwhile.
const SERVER_OUTAGE: Duration = Duration::from_secs(8);

/// Asserts that a restart of the agent's unit for a new flags file (a stop with no handover
/// file, so the agent drains, and a start of the unit after the gap the config hook leaves)
/// leaves the host live in every read: live and draining while the drain lets its job end and
/// reports it, live through the gap, and the next process's first hello is taken (none refused
/// `host-live`), after which the host serves and the job that waited during the drain is placed
/// on it. Catches a drained agent whose next process says hello as another agent process, which
/// the server refuses until the drained session's grace has run out.
#[test]
fn a_restart_for_new_flags_keeps_the_host_live_and_its_first_hello_is_taken() {
    let rig = busy_rig(
        "jobs_e2e_live_flags",
        POOL_GRACE_4M_TOML,
        &["--drain-wait", "3600"],
    );
    let job = rig.queued(2000);
    rig.until_running(&job);

    rig.agent.signal();
    assert_eq!(
        rig.agent.next_wait(),
        INTERVAL,
        "the first draining heartbeat"
    );
    assert_eq!(read(&rig.server), ("live".into(), "draining".into()));
    let waiting = rig.queued(1000);
    assert_eq!(rig.view(&waiting).0, "JOB_STATE_QUEUED", "a draining host");
    rig.machine.world.exit(&job, 0);
    assert!(rig.worker.settle());
    rig.agent.elapse();
    assert_eq!(rig.agent.next_wait(), INTERVAL, "the result goes up");
    assert!(rig.worker.settle());
    rig.agent.elapse();
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
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran.err.contains("handing over"), "a drain: {}", ran.err);

    server.clock.advance(RESTART_GAP);
    assert_eq!(read(&server).0, "live");
    let (agent, worker, polls, refused) = next_process(&server, &address, &flags, &machine);
    assert_eq!(refused, 0, "hellos refused before the next one was taken");
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
    assert_eq!(read(&rig.server), ("live".into(), "serving".into()));
    rig.until_running(&waiting);
    assert_eq!(read(&rig.server).0, "live");
    finish(rig, &[&waiting]);
}

/// Asserts that a restart of the server under load, over its files, with the host unheard for as
/// long as the farm's redeploy of the server took, on the farm's 15 s grace, leaves the host live
/// in every read and sends no hello (the farm's agents, too, kept their sessions through it):
/// the host reads live once the server is back, the agent's next two heartbeats are served in
/// the session it had (each a held reply, not a refused beat and a hello), every job runs on in
/// its first attempt, and the job that waited is placed on the host once a running one ends.
/// Catches a restarted server that holds the host lost or refuses the session its rows hold,
/// which costs a hello, a `host-live` wait and a re-adoption of every job.
#[test]
fn a_server_restart_under_load_keeps_the_host_live_and_no_hello_is_sent() {
    let rig = busy_rig("jobs_e2e_live_server", POOL_GRACE_15S_TOML, &[]);
    let (running, waiting) = load(&rig);
    let Rig {
        polls,
        server,
        agent,
        worker,
        machine,
        address,
        flags,
        files,
    } = rig;
    let server = server.restart();
    *address.lock().unwrap() = server.url();
    server.clock.advance(SERVER_OUTAGE);
    assert_eq!(read(&server).0, "live", "the restarted server");
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
    rig.beat(HELD);
    assert_eq!(read(&rig.server), ("live".into(), "serving".into()));
    rig.beat(HELD);
    assert_eq!(read(&rig.server).0, "live", "no hello followed");
    for job in &running {
        assert_eq!(rig.view(job), (RUNNING.to_owned(), None), "{job}");
        assert_eq!(rig.server.job(job)["attempt"], 1, "{job}");
    }
    rig.machine.world.exit(&running[0], 0);
    assert!(rig.worker.settle());
    rig.tick();
    rig.until_running(&waiting);
    assert_eq!(read(&rig.server).0, "live");
    let rest: Vec<&String> = running.iter().skip(1).chain([&waiting]).collect();
    finish(rig, &rest);
}
