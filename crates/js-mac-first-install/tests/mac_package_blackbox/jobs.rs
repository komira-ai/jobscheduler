//! The jobs the test runs through the agent, submitted to the job service as a client submits
//! them (`POST /v1/jobs`) and read back (`GET /v1/jobs/<id>`, `GET /v1/jobs/<id>/result`).
//!
//! Each names the smoke bundle the release publishes (`packaging/js-smoke-bundle`), by the
//! repository, tag and digest of its recipe, on macOS and arm64: the agent pulls it from the
//! public registry the farm's flags allow (`--image-allow`), holds it to the digest of
//! `bundle-allow` and runs it as one of its slot accounts. With no command the job runs the
//! bundle's entrypoint, which prints one line; with a command it runs that program, of a tool
//! root, with the bundle as its bundle.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::host::LeaseServer;

/// The recipe of the smoke bundle, as committed.
const RECIPE: &str = include_str!("../../../../packaging/js-smoke-bundle/bundle.toml");

/// How often a job is read while the test waits for it.
const POLL: Duration = Duration::from_millis(500);

/// The state of a job that is running, as the job service names it.
pub const RUNNING: &str = "JOB_STATE_RUNNING";

/// The state of a job that ended by itself with exit status 0.
pub const SUCCEEDED: &str = "JOB_STATE_SUCCEEDED";

/// The image of the smoke bundle: `<repository>:<tag>@<digest>` of the recipe.
pub fn smoke_image() -> String {
    let field = |key: &str| {
        RECIPE
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(" = ")?;
                (name.trim() == key).then(|| value.trim().trim_matches('"').to_owned())
            })
            .unwrap_or_else(|| panic!("bundle.toml has no {key}"))
    };
    format!(
        "{}:{}@{}",
        field("repository"),
        field("tag"),
        field("digest")
    )
}

/// Submits a job of the smoke bundle that runs the program `command` (the bundle's entrypoint
/// when it is `None`) with the arguments `args`; returns its id. The job service's `command` is
/// the one program, and the arguments follow it.
pub fn submit(server: &LeaseServer, command: Option<&str>, args: &[&str]) -> String {
    let mut spec = json!({
        "image": smoke_image(),
        "args": args,
        "needs": { "cpuMillis": "500", "memoryBytes": "268435456" },
        "wallLimitSeconds": "1800",
        "constraints": { "os": "macos", "arch": "arm64" },
    });
    if let Some(program) = command {
        spec["command"] = json!(program);
    }
    let reply = server
        .jobs()
        .post("/v1/jobs", &json!({ "spec": spec }))
        .expect("the job service answers the submit");
    assert_eq!(reply.status, 201, "the submit: {}", reply.body);
    reply.body["job"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the submit names no job id: {}", reply.body))
        .to_owned()
}

/// The job `id` as `GET /v1/jobs/<id>` answers it.
pub fn job(server: &LeaseServer, id: &str) -> Value {
    let reply = server
        .jobs()
        .get(&format!("/v1/jobs/{id}"))
        .expect("the job service answers");
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.body["job"].clone()
}

/// The state of the job `id`, `JOB_STATE_*`.
pub fn state(server: &LeaseServer, id: &str) -> String {
    job(server, id)["state"]
        .as_str()
        .unwrap_or_else(|| panic!("the job {id} has no state"))
        .to_owned()
}

/// The result of the job `id`, as `GET /v1/jobs/<id>/result` answers it.
pub fn result(server: &LeaseServer, id: &str) -> Value {
    let reply = server
        .jobs()
        .get(&format!("/v1/jobs/{id}/result"))
        .expect("the job service answers");
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.body["result"].clone()
}

/// Reads the job `id` until its state is `wanted` or one that comes after it can no longer be
/// it (an ended job), for at most `bound`: a bound on a hang, past which the test fails saying
/// where the job was. Returns the state it found.
pub fn wait_for(server: &LeaseServer, id: &str, wanted: &str, bound: Duration) -> String {
    let began = Instant::now();
    loop {
        let found = state(server, id);
        if found == wanted || ended(&found) {
            return found;
        }
        assert!(
            began.elapsed() < bound,
            "the job {id} is {found} after {bound:?}, not {wanted}: {}",
            job(server, id)
        );
        std::thread::sleep(POLL);
    }
}

/// Whether `state` is one a job ends in.
fn ended(state: &str) -> bool {
    [
        SUCCEEDED,
        "JOB_STATE_FAILED",
        "JOB_STATE_CANCELLED",
        "JOB_STATE_LOST",
        "JOB_STATE_FAILED_TO_START",
    ]
    .contains(&state)
}

/// Waits for the job `id` to end, for at most `bound`, and asserts that it ended by itself with
/// exit status 0 at its first attempt: the job was not lost, retried or stopped. Returns its
/// standard output.
pub fn assert_ends_zero(server: &LeaseServer, id: &str, bound: Duration) -> String {
    let found = wait_for(server, id, SUCCEEDED, bound);
    let ended = result(server, id);
    assert_eq!(found, SUCCEEDED, "the job {id}: {ended}");
    assert_eq!(ended["exitCode"], 0, "the job {id}: {ended}");
    assert_eq!(
        ended["reason"], "RESULT_REASON_EXIT",
        "the job {id}: {ended}"
    );
    assert_eq!(ended["attempt"], 1, "the job {id} ran once: {ended}");
    let tail = ended["stdoutTail"].as_str().unwrap_or_default();
    decode(tail)
}

/// The bytes of base64 `text` (the JSON form of a proto `bytes` field) as text.
fn decode(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = Vec::new();
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET
            .iter()
            .position(|known| *known == byte)
            .unwrap_or_else(|| panic!("not base64: {text}"));
        bits = (bits << 6) | u32::try_from(value).expect("a 6-bit value");
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from((bits >> held) & 0xff).expect("a byte"));
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
