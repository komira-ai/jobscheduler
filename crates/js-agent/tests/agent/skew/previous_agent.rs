//! The previous release's agent against this release's server: the previous agent's posts
//! are sent, in order, to a server of this release started in the test process, with the
//! job API calls between them made at the same points, and every answer must say what the
//! previous server's answer said (a field the previous agent reads must still be there).
//! The previous agent's posts after the first answer only carry back what it was told, so
//! the values this server draws afresh (session, job and allocation ids) are put in their
//! place ([`Minted`]).
//!
//! One answer differs by design: this server places a job in the submit call, so the first
//! heartbeat after it already carries the job's start, where the previous server's carried
//! none and its next one the start. The previous agent starts a job from any reply, so the
//! replay accepts that start ([`take_early_start`]) when it is the only command, the previous
//! server sent none, and the transcript has the start at a later step; it is compared there
//! field by field with the start the transcript has, and this server sends it again at that
//! step, because the previous agent's heartbeat there does not list the job yet, where it
//! is compared as any answer. Any other extra command is a problem.

use js_agent::{Connector, Dialer, Login};
use serde_json::Value;

use super::compare::{Minted, start_after, take_early_start, uncovered};
use super::record::Cell;

/// Every place where this server's answers said less than the previous server's, as
/// `step path: what`.
pub fn replay(name: &str, transcript: &Value) -> Vec<String> {
    let cell = Cell::start(name);
    let mut transport = Dialer
        .connect(&cell.url(), &Login::Token(cell.join_file()), None)
        .unwrap();
    let mut minted = Minted::default();
    let mut problems = Vec::new();
    // The cell runs on the default pool, which asks 5000.
    let mut flow = transcript["flow"].as_array().unwrap().clone();
    super::welcome_interval_as(&mut flow, 5000);
    for (index, step) in flow.iter().chain([&transcript["refusal"]]).enumerate() {
        if let Some(job) = step.get("submitted") {
            minted.learn(job.clone(), Value::String(cell.submit()));
            continue;
        }
        if let Some(job) = step.get("cancelled") {
            cell.cancel(minted.now_of(job).as_str().unwrap());
            continue;
        }
        let path = step["agent"]["path"].as_str().unwrap();
        let body = minted.apply(&step["agent"]["body"]);
        let at = format!("{index} {path}");
        let answer = match transport.post(path, &body) {
            Ok(answer) => answer,
            Err(why) => {
                problems.push(format!("{at}: unreachable: {why}"));
                continue;
            }
        };
        let wanted = &step["server"];
        if wanted["status"] != answer.status {
            let was = &wanted["status"];
            problems.push(format!("{at}: status was {was}, now {}", answer.status));
        }
        let expected = minted.apply(&wanted["body"]);
        let mut body = answer.body.clone();
        if let Some(early) = take_early_start(&expected, &mut body) {
            if let Some((later, start)) = start_after(&flow, index) {
                let start = minted.apply(&start);
                let named = flow[later]["agent"]["path"].as_str().unwrap();
                for what in uncovered(&start, &early, Some(&mut minted)) {
                    let rest = what.strip_prefix('$').unwrap_or(&what);
                    let problem = format!("{later} {named} $.commands[0]{rest}");
                    if !problems.contains(&problem) {
                        problems.push(problem);
                    }
                }
            } else {
                problems.push(format!("{at}: a job start the previous server never sent"));
                body = answer.body.clone();
            }
        }
        let missing = uncovered(&expected, &body, Some(&mut minted));
        for what in missing {
            let problem = format!("{at} {what}");
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
    }
    problems
}

/// Asserts that this server answers every post of the previous agent as the previous
/// server did, with the fresh values in place: the hello with a welcome of the same
/// interval and grace, the heartbeats with the job start (same image, command, booking and
/// wall limit), the stop after the cancel and the acknowledgement of the killed result,
/// and the hello of majors 2 to 3 with status 400, `unsupported-protocol` and the majors it
/// speaks. It catches a field this server no longer sends, or sends under another name or
/// with another meaning, and a field of the previous agent's posts this server no longer
/// reads (its answer would then differ).
#[test]
fn the_previous_agent_against_this_server() {
    let transcript = super::previous();
    assert_eq!(replay("skew-server-new", &transcript), Vec::<String>::new());
}

/// Asserts that the replay fails, naming the field, when the previous server's job start
/// held a field this server does not send (the transcript's start carries `wall_ms` under
/// another name, as if this server had renamed it). It catches a replay that would pass a
/// server whose job start lost a field the previous agent reads.
#[test]
fn a_field_this_server_no_longer_sends_is_named() {
    let mut transcript = super::previous();
    let flow = transcript["flow"].as_array_mut().unwrap();
    let (index, start) = flow
        .iter_mut()
        .enumerate()
        .find(|(_, step)| step["server"]["body"]["commands"][0]["type"] == "job_start")
        .unwrap();
    let fields = start["server"]["body"]["commands"][0]
        .as_object_mut()
        .unwrap();
    let wall = fields.remove("wall_ms").unwrap();
    fields.insert("wall_limit_ms".to_owned(), wall);
    let problems = replay("skew-server-renamed", &transcript);
    let wanted = format!("{index} /v1/agent/heartbeat $.commands[0].wall_limit_ms: missing");
    assert_eq!(problems, [wanted]);
}

/// Asserts that the replay fails when the previous agent's heartbeat carries a field this
/// server needs under a name it does not read (the session secret of the first heartbeat
/// under another name, as if this server had renamed the field it reads): the first problem
/// is that heartbeat's status, 403 where the previous server answered 200 (the server
/// refuses a heartbeat without its session). It catches a replay that would pass a server that no longer reads a field the
/// previous agent sends.
#[test]
fn a_field_this_server_no_longer_reads_is_caught() {
    let mut transcript = super::previous();
    let flow = transcript["flow"].as_array_mut().unwrap();
    let (index, beat) = flow
        .iter_mut()
        .enumerate()
        .find(|(_, step)| step["agent"]["path"] == js_agent::HEARTBEAT_PATH)
        .unwrap();
    let fields = beat["agent"]["body"].as_object_mut().unwrap();
    let session = fields.remove("session").unwrap();
    fields.insert("secret".to_owned(), session);
    let problems = replay("skew-server-unread", &transcript);
    let wanted = format!("{index} /v1/agent/heartbeat: status was 200, now 403");
    assert_eq!(problems.first(), Some(&wanted), "{problems:?}");
}

/// Asserts that the replay fails, naming the field, when the previous server's refusal of
/// an unknown major held a field this server does not send (the transcript's refusal
/// carries `supported` under another name, as if this server had renamed it). It catches a
/// replay that would pass a server whose refusal lost the majors the previous agent reads.
#[test]
fn a_refusal_field_this_server_no_longer_sends_is_named() {
    let mut transcript = super::previous();
    let index = transcript["flow"].as_array().unwrap().len();
    let fields = transcript["refusal"]["server"]["body"]
        .as_object_mut()
        .unwrap();
    let supported = fields.remove("supported").unwrap();
    fields.insert("majors".to_owned(), supported);
    let problems = replay("skew-server-refusal", &transcript);
    let wanted = format!("{index} /v1/agent/hello $.majors: missing");
    assert_eq!(problems, [wanted]);
}

/// Asserts that the replay does not accept a job start in an earlier reply when the
/// transcript has no start at all (the transcript's start removed, as if the previous server
/// had never sent this job to the host): the start is named in the heartbeat after the
/// submit and again where this server sends it again, each as a command the previous server
/// did not send, and the commands themselves as extra. It catches an acceptance of an early start that does not look for the start it
/// is compared with, which would pass a server that starts jobs the previous one did not.
#[test]
fn a_start_the_previous_server_never_sent_is_named() {
    let mut transcript = super::previous();
    let flow = transcript["flow"].as_array_mut().unwrap();
    let index = flow
        .iter()
        .position(|step| step["server"]["body"]["commands"][0]["type"] == "job_start")
        .unwrap();
    flow[index]["server"]["body"]["commands"] = serde_json::json!([]);
    let problems = replay("skew-server-unsent", &transcript);
    let path = "/v1/agent/heartbeat";
    let (early, shown) = (index - 1, "$.commands: 0 items, now 1");
    let never = "a job start the previous server never sent";
    let wanted = [
        format!("{early} {path}: {never}"),
        format!("{early} {path} {shown}"),
        format!("{index} {path}: {never}"),
        format!("{index} {path} {shown}"),
    ];
    // The replay goes on after the first differences (the job is then started and ends on
    // the server's side as the transcript did not say), so only the first four are asserted.
    assert_eq!(problems[..4], wanted, "{problems:?}");
}
