//! A reply is checked command by command: every rule has a test, and each says the
//! defect it catches.

mod common;

use common::{expect, job};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{Ack, Command, HeartbeatReply, ReplyCheck, ResultAck, Rule, ValidationError};

fn stop(run: &str) -> Command {
    Command::Stop { run_id: run.into() }
}

fn cancel(run: &str) -> Command {
    Command::Cancel { run_id: run.into() }
}

fn start(run: &str, gpus: &[u32]) -> Command {
    let mut spec = job();
    spec.run_id = run.into();
    spec.gpu_indices = gpus.to_vec();
    Command::Start(Box::new(spec))
}

fn reply(commands: Vec<Command>) -> HeartbeatReply {
    HeartbeatReply {
        commands,
        result_acks: Vec::new(),
        taint_ack: false,
        exclusive: None,
    }
}

fn ack(run: &str) -> ResultAck {
    ResultAck {
        run_id: run.into(),
        ack: Ack::Stale,
    }
}

/// The refusal an index got, read out of a checked reply.
fn error_at(check: &ReplyCheck<'_>, index: usize) -> ValidationError {
    let found = check.refused.iter().find(|r| r.index == index);
    found.expect("the command was refused").error
}

/// A stop beside a start whose image is over the cap keeps the stop: the reply is checked
/// command by command, the start is refused by index and code, and the stop is among the
/// commands the agent acts on. It catches a reply-wide error that throws away a stop
/// (retained intent the server resends only until the run leaves the held list) because
/// of an unrelated bad start.
#[test]
fn a_refused_start_does_not_cost_the_stop_beside_it() {
    let mut over = job();
    over.image = "a".repeat(MAX_IMAGE_LEN + 1);
    let body = reply(vec![stop("r1"), Command::Start(Box::new(over))]);
    let check = body.validate();
    assert_eq!(check.commands, vec![&stop("r1")]);
    assert_eq!(check.refused.len(), 1);
    let first = check.refused[0];
    assert_eq!(first.index, 1);
    expect(first.error, "image", Rule::TooLong, refusal::INVALID_IMAGE);
    assert_eq!(check.acks, Ok(()));
}

/// The same reply read by an older agent when a newer server has raised the image cap:
/// the start the older agent cannot accept is refused and the stop and the cancel stay.
/// The input is the raw JSON a newer server sends, so it catches both the decode and the
/// check dropping the stop.
#[test]
fn a_newer_servers_raised_cap_does_not_cost_the_stop() {
    let image = format!("{}/x", "a".repeat(MAX_IMAGE_LEN + 1));
    let mut start = serde_json::to_value(job()).unwrap();
    start["image"] = image.into();
    start["type"] = "start".into();
    let body = serde_json::json!({
        "commands": [{"type": "stop", "run_id": "r1"}, start, {"type": "cancel", "run_id": "r2"}],
    });
    let body: HeartbeatReply = serde_json::from_value(body).unwrap();
    let check = body.validate();
    assert_eq!(check.commands, vec![&stop("r1"), &cancel("r2")]);
    assert_eq!(check.refused.len(), 1);
    assert_eq!(check.refused[0].index, 1);
    assert_eq!(check.refused[0].error.code(), refusal::INVALID_IMAGE);
}

/// Every refused command is named by its own index and code, valid ones keep their order,
/// and an unknown command passes and is among the valid ones for the agent to skip. It
/// catches indices that drift (counting only valid commands) and a verdict that mixes up
/// two commands.
#[test]
fn each_command_is_judged_alone() {
    let body = reply(vec![
        stop("../x"),
        Command::Unknown,
        cancel("r2"),
        cancel(""),
        start("s1", &[0]),
    ]);
    let check = body.validate();
    assert_eq!(
        check.commands,
        vec![&Command::Unknown, &cancel("r2"), &start("s1", &[0])]
    );
    assert_eq!(
        check.refused.iter().map(|r| r.index).collect::<Vec<_>>(),
        [0, 3]
    );
    expect(
        error_at(&check, 0),
        "run_id",
        Rule::BadChars,
        refusal::INVALID_REPLY,
    );
    expect(
        error_at(&check, 3),
        "run_id",
        Rule::Empty,
        refusal::INVALID_REPLY,
    );
}

/// Commands past the cap are refused by index; the first `MAX_COMMANDS` are still read,
/// so a stop or cancel early in a reply from a server with a higher cap is acted on, and
/// a command at the cap passes. It catches an unbounded command list being read, the cap
/// moving by one, and a long reply costing the stop at its head.
#[test]
fn commands_past_the_cap_are_refused_and_the_rest_are_read() {
    let mut commands = vec![stop("a"), cancel("b")];
    commands.resize(MAX_COMMANDS + 2, Command::Unknown);
    let body = reply(commands);
    let check = body.validate();
    assert_eq!(check.commands.len(), MAX_COMMANDS);
    assert_eq!(check.commands[..2], [&stop("a"), &cancel("b")]);
    assert_eq!(
        check.refused.iter().map(|r| r.index).collect::<Vec<_>>(),
        [MAX_COMMANDS, MAX_COMMANDS + 1]
    );
    expect(
        error_at(&check, MAX_COMMANDS),
        "commands",
        Rule::TooMany,
        refusal::INVALID_REPLY,
    );
    let at_cap = reply(vec![Command::Unknown; MAX_COMMANDS]);
    assert_eq!(at_cap.validate().commands.len(), MAX_COMMANDS);
    assert_eq!(HeartbeatReply::default().validate().refused, []);
}

/// A second start that names a run id or a GPU index an earlier accepted start already
/// names is refused (the later one, by index); the first stays, stops are untouched, and
/// a start refused for another reason names nothing, so it blocks nobody. It catches a
/// reply that starts a run twice or hands one device to two jobs, and a refused start
/// that still reserves its device.
#[test]
fn starts_in_one_reply_do_not_overlap() {
    let same_run = reply(vec![start("a", &[0]), stop("a"), start("a", &[1])]);
    let check = same_run.validate();
    assert_eq!(check.commands, vec![&start("a", &[0]), &stop("a")]);
    expect(
        error_at(&check, 2),
        "commands",
        Rule::Duplicate,
        refusal::INVALID_REPLY,
    );
    let same_gpu = reply(vec![start("a", &[0, 1]), start("b", &[1])]);
    let check = same_gpu.validate();
    assert_eq!(check.commands, vec![&start("a", &[0, 1])]);
    assert_eq!(check.refused.len(), 1);
    assert_eq!(check.refused[0].index, 1);
    assert_eq!(
        check.refused[0].error.rule(),
        Rule::Duplicate,
        "the second start is the one refused"
    );
    let mut bad = job();
    bad.run_id = "a".into();
    bad.gpu_indices = vec![0];
    bad.image = "--x".into();
    let freed = reply(vec![Command::Start(Box::new(bad)), start("b", &[0])]);
    let check = freed.validate();
    assert_eq!(check.commands, vec![&start("b", &[0])]);
    assert_eq!(check.refused.len(), 1);
    assert_eq!(check.refused[0].index, 0);
    assert_eq!(check.refused[0].error.code(), refusal::INVALID_IMAGE);
    let apart = reply(vec![start("a", &[0]), start("b", &[1])]);
    assert_eq!(apart.validate().commands.len(), 2);
}

/// Acknowledgements over the cap, with a bad run id or repeating a run, make `acks` an
/// error while the commands still come through; the cap passes. It catches a bad
/// acknowledgement discarding a stop, and an unbounded acknowledgement list.
#[test]
fn bad_acknowledgements_do_not_cost_the_commands() {
    let with = |acks: Vec<ResultAck>| HeartbeatReply {
        result_acks: acks,
        ..reply(vec![stop("r1")])
    };
    let cases = [
        (
            vec![ack("r"); MAX_RESULTS + 1],
            "result_acks",
            Rule::TooMany,
        ),
        (vec![ack("a b")], "result_acks.run_id", Rule::BadChars),
        (vec![ack("a"), ack("a")], "result_acks", Rule::Duplicate),
    ];
    for (acks, field, rule) in cases {
        let body = with(acks);
        let check = body.validate();
        assert_eq!(check.commands, vec![&stop("r1")]);
        let err = check.acks.unwrap_err();
        expect(err, field, rule, refusal::INVALID_REPLY);
    }
    let at_cap = (0..MAX_RESULTS).map(|i| ack(&format!("r{i}"))).collect();
    assert_eq!(with(at_cap).validate().acks, Ok(()));
}

/// An acknowledgement checked on its own: a good run id passes and an empty or path-like
/// one is refused with the reply code. It catches the public check being dropped from an
/// acknowledgement that arrives outside a reply.
#[test]
fn a_single_acknowledgement_is_checked() {
    assert_eq!(ack("r").validate(), Ok(()));
    for bad in ["", "../x"] {
        let err = ack(bad).validate().unwrap_err();
        assert_eq!(err.field(), "result_acks.run_id");
        assert_eq!(err.code(), refusal::INVALID_REPLY);
    }
}

/// The lease id of `exclusive` is held to the id alphabet and length: a good one, one at
/// the cap and none pass; an empty, path-like or over-long one fails `exclusive` and
/// leaves the commands and acknowledgements alone. It catches an unchecked peer string
/// reaching the agent's lease bookkeeping.
#[test]
fn reply_exclusive_lease_id_is_checked() {
    let with = |id: Option<String>| HeartbeatReply {
        exclusive: id,
        ..reply(vec![stop("r1")])
    };
    for good in [None, Some("1-1".to_string()), Some("a".repeat(MAX_ID_LEN))] {
        assert_eq!(with(good).validate().exclusive, Ok(()));
    }
    let cases = [
        ("", Rule::Empty),
        ("../x", Rule::BadChars),
        (&"a".repeat(MAX_ID_LEN + 1), Rule::TooLong),
    ];
    for (id, rule) in cases {
        let body = with(Some(id.to_string()));
        let check = body.validate();
        assert_eq!(check.commands, vec![&stop("r1")]);
        assert_eq!(check.acks, Ok(()));
        expect(
            check.exclusive.unwrap_err(),
            "exclusive",
            rule,
            refusal::INVALID_REPLY,
        );
    }
}
