//! The cases. Each behaviour of the v1 path runs against both stores, and each that a client
//! drives runs through both clients; the case's name says which (`sqlite::cli::...`).

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::SystemClock;
use js_state::sqlite::{Options, SqliteStore};
use js_usage::{Booked, EndedHistory, EndedReason};

use crate::agent::Agent;
use crate::rig::{Kind, Rig, Spec, Via, text};
use crate::runner::Case;
use crate::server::Server;

/// The heartbeats a case lets pass after a job ended, to see that nothing more happens to
/// it: each is a whole exchange in which the server could hand the job out again.
const QUIET_BEATS: usize = 3;

/// A behaviour run for each store and each client.
type ByClient = fn(Kind, Via);

/// A behaviour run for each store.
type ByStore = fn(Kind);

/// Every case, named `STORE::CLIENT::BEHAVIOUR` or `STORE::BEHAVIOUR`.
pub fn all() -> Vec<Case> {
    let by_client: [(&str, ByClient); 4] = [
        (
            "a_job_runs_once_and_succeeds_with_its_output",
            a_job_runs_once_and_succeeds_with_its_output,
        ),
        (
            "a_job_that_exits_non_zero_fails_with_its_exit_code",
            a_job_that_exits_non_zero_fails_with_its_exit_code,
        ),
        ("a_running_job_is_cancelled", a_running_job_is_cancelled),
        (
            "a_second_whole_host_job_runs_after_the_first_ends",
            a_second_whole_host_job_runs_after_the_first_ends,
        ),
    ];
    let by_store: [(&str, ByStore); 5] = [
        (
            "get_job_answers_after_the_server_restarts",
            get_job_answers_after_the_server_restarts,
        ),
        (
            "a_finished_job_has_a_usage_row",
            a_finished_job_has_a_usage_row,
        ),
        (
            "an_agent_with_no_runtime_flag_runs_a_job_through_crun",
            an_agent_with_no_runtime_flag_runs_a_job_through_crun,
        ),
        (
            "a_second_agent_on_the_same_host_id_or_scratch_root_is_refused",
            a_second_agent_on_the_same_host_id_or_scratch_root_is_refused,
        ),
        (
            "sigterm_lets_a_running_job_finish_and_reports_it",
            sigterm_lets_a_running_job_finish_and_reports_it,
        ),
    ];
    let mut cases: Vec<Case> = Vec::new();
    for kind in [Kind::Sqlite, Kind::Postgres] {
        for via in [Via::Cli, Via::Http] {
            for (name, case) in by_client {
                let full = format!("{}::{}::{name}", kind.name(), via.name());
                cases.push((full, Box::new(move || case(kind, via))));
            }
        }
        for (name, case) in by_store {
            let full = format!("{}::{name}", kind.name());
            cases.push((full, Box::new(move || case(kind))));
        }
    }
    cases
}

/// The name of a case's node and files: its behaviour and store, and its client.
fn name(behaviour: &str, kind: Kind, via: Option<Via>) -> String {
    let via = via.map_or("", Via::name);
    format!("{behaviour}-{}{via}", kind.name())
}

/// Asserts the path of one job that exits 0, through `via`: submitted, it is queued, placed on
/// the agent's host, started through `crun run` of the stand-in, and ends succeeded with exit
/// code 0, reason exit, and the line it printed in both tails of `GetResult` and in the tails
/// of `GetLogs`. The stand-in started it exactly once, and it is not started again in the
/// heartbeats after it ended. It catches a server that never hands a queued job to the agent,
/// an agent that never reaches `crun`, a result lost on the way back, output dropped, and a job
/// handed out twice.
fn a_job_runs_once_and_succeeds_with_its_output(kind: Kind, via: Via) {
    let rig = Rig::new(&name("runs", kind, Some(via)), kind, &[]);
    let jobs = rig.client(via);
    let spec = Spec::new("j1");
    let made = rig.submit(via, &spec);
    assert_eq!(made["id"], "j1");
    assert_eq!(made["state"], "JOB_STATE_QUEUED");
    rig.until_state("j1", "JOB_STATE_SUCCEEDED");
    let job = jobs.job("j1");
    assert_eq!(job["hostId"], "h1", "{job}");
    let run = rig
        .node
        .calls()
        .lines()
        .filter(|line| line.contains(" run --bundle ") && line.ends_with("-j1"))
        .count();
    assert_eq!(run, 1, "one `crun run` of j1:\n{}", rig.node.calls());
    let result = jobs.result("j1");
    assert_eq!(result["state"], "JOB_STATE_SUCCEEDED");
    assert_eq!(result["exitCode"], 0, "{result}");
    assert_eq!(result["reason"], "RESULT_REASON_EXIT");
    assert_eq!(text(&result["stdoutTail"]), format!("{}\n", spec.says()));
    assert_eq!(
        text(&result["stderrTail"]),
        format!("{} on stderr\n", spec.says())
    );
    let logs = jobs.logs("j1");
    assert_eq!(logs["state"], "JOB_STATE_SUCCEEDED");
    assert!(text(&logs["stdoutTail"]).contains(&spec.says()), "{logs}");
    let stderr = format!("{} on stderr", spec.says());
    assert!(text(&logs["stderrTail"]).contains(&stderr), "{logs}");
    for _ in 0..QUIET_BEATS {
        rig.agent.beat();
    }
    assert_eq!(rig.starts("j1"), 1, "{:?}", rig.node.events());
    assert_eq!(rig.state("j1"), "JOB_STATE_SUCCEEDED");
}

/// Asserts that a job that exits 3, through `via`, ends failed with exit code 3 and reason
/// exit, its output in the tails, started once. It catches an exit code read as 0 or as
/// another number on any hop (the stand-in, the supervisor, the agent's result, the server's
/// row, the read), and a failed job that is run again.
fn a_job_that_exits_non_zero_fails_with_its_exit_code(kind: Kind, via: Via) {
    let rig = Rig::new(&name("fails", kind, Some(via)), kind, &[]);
    let jobs = rig.client(via);
    let spec = Spec::new("j3").exiting(3);
    rig.submit(via, &spec);
    rig.until_state("j3", "JOB_STATE_FAILED");
    let result = jobs.result("j3");
    assert_eq!(result["state"], "JOB_STATE_FAILED");
    assert_eq!(result["exitCode"], 3, "{result}");
    assert_eq!(result["reason"], "RESULT_REASON_EXIT");
    assert_eq!(text(&result["stdoutTail"]), format!("{}\n", spec.says()));
    assert!(text(&jobs.logs("j3")["stdoutTail"]).contains(&spec.says()));
    for _ in 0..QUIET_BEATS {
        rig.agent.beat();
    }
    assert_eq!(rig.starts("j3"), 1, "{:?}", rig.node.events());
}

/// Asserts the cancel of a running job, through `via`: the job is running in the stand-in;
/// the cancel answers it still running with the cancel requested; the stop rides on a
/// heartbeat reply, the agent kills the container (`crun kill`, the stand-in's TERM), and the
/// job ends cancelled with reason cancelled. The stand-in never let it end by itself. It
/// catches a cancel that is recorded and never delivered, a stop the agent does not act on,
/// and a killed job reported as failed or succeeded.
fn a_running_job_is_cancelled(kind: Kind, via: Via) {
    let rig = Rig::new(&name("cancel", kind, Some(via)), kind, &[]);
    let jobs = rig.client(via);
    rig.submit(via, &Spec::new("c1").held());
    rig.until_up("c1");
    let asked = jobs.cancel("c1");
    assert_eq!(asked["state"], "JOB_STATE_RUNNING", "{asked}");
    assert_eq!(asked["cancelRequested"], true, "{asked}");
    rig.until_state("c1", "JOB_STATE_CANCELLED");
    assert!(
        rig.node.calls().lines().any(|line| line.contains(" kill ")),
        "no `crun kill`:\n{}",
        rig.node.calls()
    );
    let events = rig.node.events();
    assert_eq!(
        events,
        ["start c1", "stop c1"],
        "it was stopped, never ended by itself"
    );
    let result = jobs.result("c1");
    assert_eq!(result["state"], "JOB_STATE_CANCELLED");
    assert_eq!(result["reason"], "RESULT_REASON_CANCELLED", "{result}");
}

/// Asserts that two whole-host jobs on one host run one after the other, through `via`: the
/// first is placed, prepared and runs; while it runs the second stays queued for heartbeat
/// after heartbeat and the stand-in never starts it; once the first ends the second is placed
/// and runs, and the stand-in's record is the first's start and end, then the second's. It
/// catches a whole-host booking that lets a second job onto the busy host, and a second job
/// never placed after the first freed the host.
fn a_second_whole_host_job_runs_after_the_first_ends(kind: Kind, via: Via) {
    let rig = Rig::new(&name("whole", kind, Some(via)), kind, &[]);
    rig.submit(via, &Spec::new("w1").whole_host().held());
    rig.until_up("w1");
    rig.submit(via, &Spec::new("w2").whole_host());
    for _ in 0..QUIET_BEATS {
        rig.agent.beat();
        assert_eq!(rig.state("w2"), "JOB_STATE_QUEUED");
        assert_eq!(rig.state("w1"), "JOB_STATE_RUNNING");
    }
    assert_eq!(rig.starts("w2"), 0, "{:?}", rig.node.events());
    rig.node.release("w1");
    rig.until_state("w1", "JOB_STATE_SUCCEEDED");
    rig.until_state("w2", "JOB_STATE_SUCCEEDED");
    assert_eq!(
        rig.node.events(),
        ["start w1", "end w1", "start w2", "end w2"],
        "the second ran after the first ended, never beside it"
    );
}

/// Asserts that the server's job state outlives the process: a job ends succeeded, the agent
/// and then the server stop (SIGTERM, status 0), and a new server process over the same files
/// and the same store, started with `--boot resume`, answers `GetJob` and `GetResult` of the
/// job as the first did, through both clients. It catches a job store that is not reopened, or
/// not the same, after a restart.
fn get_job_answers_after_the_server_restarts(kind: Kind) {
    let rig = Rig::new(&name("restart", kind, None), kind, &[]);
    rig.submit(Via::Http, &Spec::new("r1"));
    rig.until_state("r1", "JOB_STATE_SUCCEEDED");
    let before = rig.http.job("r1");
    let result_before = crate::rig::Jobs::result(&rig.http, "r1");
    assert_eq!(text(&result_before["stdoutTail"]), "hello from r1\n");
    let Rig {
        node: _node,
        files,
        server,
        agent,
        ..
    } = rig;
    assert_eq!(agent.stop().status, "ExitCode(unix_exit_status(0))");
    assert!(server.stop().success());
    let server = Server::start(&files, "resume");
    let http = crate::http::Gateway::new(server.job);
    let js = crate::cli::Js::new(server.job, &files.token_file());
    for jobs in [&http as &dyn crate::rig::Jobs, &js] {
        let after = jobs.job("r1");
        assert_eq!(after, before, "GetJob after the restart");
        assert_eq!(
            jobs.result("r1"),
            result_before,
            "GetResult after the restart"
        );
    }
}

/// Asserts that a finished job leaves its usage row: once the agent no longer lists the job,
/// the server has freed its allocation and written the ended row in the same commit, which
/// the job store beside the lease file holds after the server stops: one row, the job's id,
/// the agent's host, the CPU and memory it booked and the reason `released`.
///
/// `GetUsage` reads the usage store's ended bookings of leases (docs/http-gateway.md), not the
/// job store, so the row of a job is not listed there yet; when it is, this case reads it
/// through `GetUsage` too.
fn a_finished_job_has_a_usage_row(kind: Kind) {
    let rig = Rig::new(&name("usage", kind, None), kind, &[]);
    rig.submit(Via::Http, &Spec::new("u1"));
    rig.until_state("u1", "JOB_STATE_SUCCEEDED");
    rig.until_freed();
    let Rig {
        node: _node,
        files,
        server,
        agent,
        ..
    } = rig;
    agent.stop();
    assert!(server.stop().success());
    let store = SqliteStore::open(
        &files.job_store(),
        Arc::new(SystemClock::default()),
        Options::default(),
    )
    .expect("the job store");
    let limit = NonZeroUsize::new(10).expect("a limit");
    let read = EndedHistory::new(&store)
        .range(0, u64::MAX, limit)
        .expect("the usage rows");
    assert_eq!(read.skipped, 0);
    assert_eq!(read.rows.len(), 1, "{:?}", read.rows);
    let row = &read.rows[0];
    assert_eq!(row.id.as_str(), "u1");
    assert_eq!(row.host.as_ref().map(|host| host.as_str()), Some("h1"));
    assert_eq!(row.ended_reason, EndedReason::Released);
    let booked = Booked {
        whole_host: false,
        cpu_milli: 1000,
        memory_bytes: 268_435_456,
    };
    assert_eq!(row.booked, Some(booked));
}

/// Asserts the default runtime (ported from the lease path of the agent's crun tests): an
/// agent given no `--runtime` registers, runs a job through the stand-in and reports it
/// succeeded. It catches a default that is not crun, or a crun agent that runs no job.
fn an_agent_with_no_runtime_flag_runs_a_job_through_crun(kind: Kind) {
    let rig = Rig::with_agent(&name("default", kind, None), kind, |node, server, token| {
        Agent::start_defaulted(node, server, token, "h1")
    });
    rig.submit(Via::Http, &Spec::new("d1"));
    rig.until_state("d1", "JOB_STATE_SUCCEEDED");
    assert_eq!(rig.starts("d1"), 1);
}

/// Asserts the agent's claims (ported from the lease path of the agent's crun tests): while an
/// agent runs a job, a second agent with the same host id, and one with another host id on the
/// same scratch root, each exit 2 naming what they share, and the first's job still ends
/// succeeded. It catches two agents that would sweep each other's jobs.
fn a_second_agent_on_the_same_host_id_or_scratch_root_is_refused(kind: Kind) {
    let rig = Rig::new(&name("claims", kind, None), kind, &[]);
    rig.submit(Via::Http, &Spec::new("k1").held());
    rig.until_up("k1");
    let (url, join) = (rig.server.url(), rig.files.join_file());
    let same_host = Agent::refused(&rig.node, &url, &join, "h1", &[]);
    assert_eq!(same_host.status, "ExitCode(unix_exit_status(2))");
    assert!(
        same_host.err.contains("--host-id h1: another crun agent"),
        "{}",
        same_host.err
    );
    let same_scratch = Agent::refused(&rig.node, &url, &join, "h2", &[]);
    assert_eq!(same_scratch.status, "ExitCode(unix_exit_status(2))");
    let scratch = rig.node.scratch.display().to_string();
    assert!(
        same_scratch.err.contains("--scratch-root ")
            && same_scratch.err.contains("another crun agent")
            && same_scratch.err.contains(&scratch),
        "{}",
        same_scratch.err
    );
    rig.node.release("k1");
    rig.until_state("k1", "JOB_STATE_SUCCEEDED");
}

/// Asserts the drain (ported from the agent's lease end-to-end tests): after SIGTERM while a
/// job runs, the agent goes on beating and the job runs on, heartbeat after heartbeat; once it
/// ends by itself the agent reports it, the server records it succeeded, and the agent exits
/// 0, never having stopped it. It catches a drain that kills the job, or an agent that leaves
/// before its last result reached the server.
fn sigterm_lets_a_running_job_finish_and_reports_it(kind: Kind) {
    let rig = Rig::new(&name("drain", kind, None), kind, &["--drain-wait", "3600"]);
    rig.submit(Via::Http, &Spec::new("s1").held());
    rig.until_up("s1");
    rig.agent.signal();
    for _ in 0..QUIET_BEATS {
        rig.agent.beat();
        assert_eq!(rig.state("s1"), "JOB_STATE_RUNNING");
    }
    rig.node.release("s1");
    rig.until_state("s1", "JOB_STATE_SUCCEEDED");
    let Rig { node, agent, .. } = rig;
    let ran = agent.finish();
    assert_eq!(ran.status, "ExitCode(unix_exit_status(0))", "{}", ran.err);
    assert_eq!(node.events(), ["start s1", "end s1"], "never stopped");
}
