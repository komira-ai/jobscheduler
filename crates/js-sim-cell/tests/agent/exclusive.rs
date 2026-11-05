//! The agent node's side of the whole-host handshake against the scripted stand-in: the
//! phase and the ready report each heartbeat carries, the starts it launches or leaves,
//! and what it writes into the cell's ledger. Uses the helpers of the parent file; the
//! times follow its schedule (heartbeat `n` leaves at `10 + n * 1000` and its answer is
//! back 10 ms later).

use js_agent_wire::Phase;
use js_sim_cell::{Prepare, StopCommand};

use super::*;

/// A heartbeat answer that names lease `1-1` in `exclusive`, with `commands`.
fn exclusive(commands: Value) -> BeatAnswer {
    BeatAnswer::Reply(json!({ "exclusive": "1-1", "commands": commands }))
}

/// Each heartbeat's phase and `exclusive_ready`, in order.
fn phases(sim: &Sim<Cell>) -> Vec<(Phase, Option<String>)> {
    let beats = stand_in(sim).beats();
    let each =
        |(_, beat): (u64, &js_agent_wire::Heartbeat)| (beat.phase, beat.exclusive_ready.clone());
    beats.into_iter().map(each).collect()
}

/// An agent whose prepare takes 1.5 s is told of lease `1-1` by the answer to heartbeat
/// 1 (at 1 020). Heartbeat 2 (2 010) says `preparing`; heartbeat 3 (3 010), past the
/// delay and holding nothing, says `serving` with `exclusive_ready: "1-1"`, and so do
/// heartbeats 4 and 5 while the answers name the lease. The lease's start in answer 3 is
/// launched. Answer 5 names no lease and stops the run (and cancels a run the agent never
/// ran): heartbeat 6 says `restoring`, and after answer 6, heartbeat 7 says `serving`.
/// The ledger holds the three ready reports, the start with the heartbeat it names and
/// its wall limit, and the one stop that reached a running run. Catches a node that
/// never tells the server its phase, reports ready early or once, launches nothing on a
/// held host, restores without a `restoring` heartbeat, or records a stop of a run it
/// was not running.
#[test]
fn the_agent_node_prepares_reports_ready_and_restores_through_its_heartbeats() {
    let ending = json!([
        { "type": "stop", "run_id": "r1" },
        { "type": "cancel", "run_id": "r9" },
    ]);
    let script = Script::plain()
        .beat(1, exclusive(json!([])))
        .beat(2, exclusive(json!([])))
        .beat(3, exclusive(json!([start("r1", LONG_RUN_MS, 3, 0)])))
        .beat(4, exclusive(json!([])))
        .commands(5, ending);
    let (mut sim, ledger) = cell(spec(), script, INTERVAL);
    let delay = Duration::from_millis(1_500);
    agent_mut(&mut sim).set_prepare(Prepare::Ready { delay });
    assert_eq!(agent(&sim).prepare(), Prepare::Ready { delay });
    sim.run_until(ms(7_500));
    let ready = Some("1-1".to_owned());
    let wanted = [
        (Phase::Serving, None),
        (Phase::Preparing, None),
        (Phase::Serving, ready.clone()),
        (Phase::Serving, ready.clone()),
        (Phase::Serving, ready),
        (Phase::Restoring, None),
        (Phase::Serving, None),
    ];
    assert_eq!(phases(&sim), wanted);
    let reports: Vec<(String, String, u64)> = ledger
        .ready()
        .into_iter()
        .map(|r| (r.host.as_str().to_owned(), r.lease, r.at.as_millis()))
        .collect();
    let one = |at| ("h000".to_owned(), "1-1".to_owned(), at);
    assert_eq!(reports, [one(3_010), one(4_010), one(5_010)]);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 3_020)]);
    let starts = ledger.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].run, RunId::new("r1"));
    assert_eq!(starts[0].lease, LeaseId::new(1, 1));
    assert_eq!((starts[0].at, starts[0].asked_at), (ms(3_020), ms(3_010)));
    assert_eq!(starts[0].wall, Duration::from_millis(600_000));
    let stops = ledger.stops();
    assert_eq!(stops.len(), 1, "only the stop of the running run");
    assert_eq!(stops[0].run, RunId::new("r1"));
    assert_eq!(stops[0].command, StopCommand::Stop);
    assert_eq!(stops[0].at, ms(5_020));
    assert_eq!(stops[0].wall_ends_at, ms(3_020 + 600_000));
}

/// A silent agent is told of lease `1-1` by answer 1, which also carries a start of that
/// lease: the start is written into the ledger and not launched (a preparing agent takes
/// no start). Answer 2's `exclusive` fails the wire's check: it is counted and changes
/// nothing, so heartbeat 3 still says `preparing`. Answer 3 names no lease (`restoring`
/// at heartbeat 4), and answer 4 ends the session: the agent says hello again at once,
/// announcing `restoring`. Catches a preparing agent that launches a start, an agent that
/// acts on an `exclusive` that fails its check (or does not count it), and a hello that
/// announces `serving` whatever the agent is doing.
#[test]
fn a_preparing_agent_launches_no_start_and_a_bad_exclusive_is_counted_and_ignored() {
    let bad = BeatAnswer::Reply(json!({ "exclusive": "not an id!" }));
    let script = Script::plain()
        .beat(1, exclusive(json!([start("r1", LONG_RUN_MS, 1, 0)])))
        .beat(2, bad)
        .beat(
            4,
            BeatAnswer::Refuse {
                status: 409,
                code: "stale-epoch",
            },
        );
    let (mut sim, ledger) = cell(spec(), script, INTERVAL);
    agent_mut(&mut sim).set_prepare(Prepare::Silent);
    sim.run_until(ms(4_500));
    let wanted = [
        (Phase::Serving, None),
        (Phase::Preparing, None),
        (Phase::Preparing, None),
        (Phase::Restoring, None),
    ];
    assert_eq!(phases(&sim), wanted);
    assert!(launches(&ledger).is_empty());
    let started: Vec<RunId> = ledger.starts().into_iter().map(|s| s.run).collect();
    assert_eq!(started, [RunId::new("r1")]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 1);
    let hellos = stand_in(&sim).hellos();
    assert_eq!(hellos.len(), 2);
    assert_eq!(hellos[1].phase, Phase::Restoring);
    assert!(ledger.ready().is_empty());
}
