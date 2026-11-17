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
#[cfg(target_os = "linux")]
pub mod previous_server;
pub mod record;

use serde_json::Value;

/// Sets the heartbeat interval of the welcome in `steps` (a transcript's flow, the hello
/// first) to `ms`, the interval of the pool the transcript is compared on. A transcript holds
/// either the default 5000 (one recorded on the default pool, as `v0.1.213` was) or
/// [`record::QUICK_INTERVAL_MS`] (one recorded by [`record::record`], which runs on the
/// quick pool); any other value fails, so a welcome that names another interval is still
/// caught.
pub fn welcome_interval_as(steps: &mut [Value], ms: u64) {
    let welcome = &mut steps[0]["server"]["body"]["heartbeat_interval_ms"];
    assert!(
        [5000, record::QUICK_INTERVAL_MS]
            .map(Value::from)
            .contains(welcome),
        "the transcript's welcome: {welcome}"
    );
    *welcome = Value::from(ms);
}

/// The transcript of the previous release, `v0.1.213`.
pub fn previous() -> Value {
    serde_json::from_str(include_str!("v0.1.213.json")).unwrap()
}

/// Asserts that this release's agent and server, met as [`record`] meets them, say
/// everything their previous release said, with the fresh values paired as the replay
/// pairs them, in two respects that are not a field added: the recording's pool asks a
/// heartbeat interval of [`record::QUICK_INTERVAL_MS`] where the previous release's asked
/// 5000 (the one value of the transcript set to it), and the job start comes in the reply
/// to the heartbeat after the submit, one before the previous release's (the server
/// places the job in the submit call), and is then compared field by field at the step
/// the previous release has it, where this release sends none ([`compare::take_early_start`]);
/// any other command, or a start the previous release never sent, is a difference. It also
/// writes this release's transcript to `skew/transcript.json` under the
/// directory cargo gives integration tests for scratch files (`CARGO_TARGET_TMPDIR`, under
/// the build's own target directory; nothing else writes there), which is where the next
/// release's transcript is taken from: a path that outlives the test, unlike a [`Scratch`]
/// directory. It catches a transcript that no longer matches the conversation [`record`] has
/// (the other tests would then compare against a conversation that is not the one they run).
///
/// [`Scratch`]: crate::support::Scratch
#[test]
#[cfg(target_os = "linux")]
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
    let (mut then, now) = (steps(&previous), steps(&transcript));
    assert_eq!(then.len(), now.len(), "{transcript}");
    welcome_interval_as(&mut then, record::QUICK_INTERVAL_MS);
    let mut ahead = std::collections::BTreeMap::new();
    for (index, (old, new)) in then.iter().zip(&now).enumerate() {
        let old = minted.apply(old);
        let mut new = new.clone();
        let body = new
            .get_mut("server")
            .and_then(|server| server.get_mut("body"));
        if let Some(start) =
            body.and_then(|body| compare::take_early_start(&old["server"]["body"], body))
        {
            match compare::start_after(&then, index) {
                Some((later, _)) => {
                    ahead.insert(later, start);
                }
                None => problems.push(format!(
                    "{index} a job start the previous release never sent"
                )),
            }
        }
        if let Some(start) = ahead.remove(&index) {
            let commands = &mut new["server"]["body"]["commands"];
            if commands.as_array().is_some_and(Vec::is_empty) {
                *commands = serde_json::json!([start]);
            }
        }
        let found = compare::uncovered(&old, &new, Some(&mut minted));
        problems.extend(found.into_iter().map(|what| format!("{index} {what}")));
    }
    assert_eq!(problems, Vec::<String>::new());
}
