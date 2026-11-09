//! Skew between this release and the previous one, both ways (`docs/design/rollout.md`,
//! version compatibility, rules 1 to 3): this release's agent against the previous
//! release's server ([`previous_server`]), the previous release's agent against this
//! release's server ([`previous_agent`]), and the refusal of a hello whose protocol major
//! the server does not speak.
//!
//! The previous release cannot be built here: a git dependency on it would bring a second
//! `js-types`, which `layers.toml` refuses (`single_source`), and `deny.toml` allows no git
//! source of this repository. So it stands as a transcript: `v0.1.213.json` is the
//! conversation the agent and the server of that release had, recorded by [`record`] at
//! that tag (hello and welcome, the heartbeats around a job that is started, reported
//! running, cancelled, stopped, reported killed and acknowledged, the heartbeat of the
//! stopping agent, and a hello of majors 2 to 3 with its refusal). It is a file, not values
//! built from the current types, so a change to those types cannot move it. How it is
//! replaced by the next release's is in `docs/layering.md`, "The previous release of the
//! agent wire".
//!
//! A frame of one side serves the other's previous release when it says everything the
//! previous release's frame said ([`compare`]): a field removed, renamed or given another
//! value fails the test that meets it, and a field added does not. Nothing here asserts a
//! time: the server's clock is a manual one no step moves, and the agent's waits are
//! opened one at a time by the test, which compares the durations the agent asks for.

pub mod compare;
pub mod previous_agent;
pub mod previous_server;
pub mod record;

use serde_json::Value;

/// The transcript of the previous release, `v0.1.213`.
pub fn previous() -> Value {
    serde_json::from_str(include_str!("v0.1.213.json")).unwrap()
}

/// Asserts that this release's agent and server, met as [`record`] meets them, say
/// everything their previous release said, with the fresh values paired as the replay
/// pairs them; and writes this release's transcript to `skew/transcript.json` under the
/// directory cargo gives integration tests for scratch files, which is where the next
/// release's transcript is taken from. It catches a transcript that no longer matches the
/// conversation [`record`] has (the other tests would then compare against a conversation
/// that is not the one they run).
#[test]
fn this_version_still_says_what_the_previous_release_said() {
    let transcript = record::record("skew-this-version");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("skew");
    std::fs::create_dir_all(&dir).unwrap();
    let text = serde_json::to_string_pretty(&transcript).unwrap();
    std::fs::write(dir.join("transcript.json"), text + "\n").unwrap();

    let previous = previous();
    let mut minted = compare::Minted::default();
    let mut problems = Vec::new();
    let steps = |of: &Value| {
        let mut all = of["flow"].as_array().unwrap().clone();
        all.push(of["refusal"].clone());
        all
    };
    let (then, now) = (steps(&previous), steps(&transcript));
    assert_eq!(then.len(), now.len(), "{transcript}");
    for (index, (old, new)) in then.iter().zip(&now).enumerate() {
        let old = minted.apply(old);
        let found = compare::uncovered(&old, new, Some(&mut minted));
        problems.extend(found.into_iter().map(|what| format!("{index} {what}")));
    }
    assert_eq!(problems, Vec::<String>::new());
}
