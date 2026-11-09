//! The previous release's agent against this release's server: the previous agent's posts
//! are sent, in order, to a server of this release started in the test process, with the
//! job API calls between them made at the same points, and every answer must say what the
//! previous server's answer said (a field the previous agent reads must still be there).
//! The previous agent's posts after the first answer only carry back what it was told, so
//! the values this server draws afresh (session, job and allocation ids) are put in their
//! place ([`Minted`]).

use js_agent::{Connector, Dialer};
use serde_json::Value;

use super::compare::{Minted, uncovered};
use super::record::Cell;

/// Every place where this server's answers said less than the previous server's, as
/// `step path: what`.
pub fn replay(name: &str, transcript: &Value) -> Vec<String> {
    let cell = Cell::start(name);
    let mut transport = Dialer
        .connect(&cell.url(), &cell.join_file(), None)
        .unwrap();
    let mut minted = Minted::default();
    let mut problems = Vec::new();
    let flow = transcript["flow"].as_array().unwrap();
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
        let missing = uncovered(&expected, &answer.body, Some(&mut minted));
        problems.extend(missing.into_iter().map(|what| format!("{at} {what}")));
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
/// under another name, as if this server had renamed the field it reads): the server
/// refuses that heartbeat with the session mismatch, where the previous server accepted
/// it. It catches a replay that would pass a server that no longer reads a field the
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
