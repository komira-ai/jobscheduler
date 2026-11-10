//! The targets `release desired` hands to hostkeeper's `set-desired`: for each platform one
//! entry of the installer `apt`, naming the host's repository entry of the stable suite, the
//! package version, the SHA-256 of the `.deb` and the hook manifest of js-agent.
//!
//! hostkeeper's native entries carry the manifest in the call, not in the package. The entry is
//! the proto3 canonical JSON of hostkeeper's `Target` (lowerCamel names, 64-bit integers as JSON
//! strings), the form its coordinator and its own `cargo xtask release targets` write.

use serde_json::{Map, Value, json};

use super::super::deb::Platform;

/// The name of the host's repository entry of the stable suite: the file stem of
/// `/etc/hostkeeper/repositories/komira-stable.sources`.
pub const REPOSITORY: &str = "komira-stable";

/// The oldest hostkeeper that reads the entry: the first release whose daemon runs a native
/// entry's `hook.command` from the sealed tree of the package.
pub const MIN_HOSTKEEPER: &str = "0.1.225";

/// The program of the hook, an absolute path of the package, and its arguments.
pub const HOOK_COMMAND: [&str; 2] = ["/usr/bin/js-agent", "update-hook"];

/// The unit hostkeeper starts, stops and observes.
pub const RUN_UNIT: &str = "js-agent-crun.service";

/// The bound of `prepare`, in seconds: staging and checking the new release, with the wait for
/// the agent's running jobs to be cordoned and drained (up to 3600 s) and a margin.
pub const PREPARE_SECS: u64 = 3900;

/// The bound of `cutover`: the stop of the agent (a drain of up to 3600 s and 120 s to stop
/// jobs), the wait for the unit's cgroup to empty, and the start.
pub const CUTOVER_SECS: u64 = 4320;

/// The bound of `verify`: the new agent runs and is registered.
pub const VERIFY_SECS: u64 = 615;

/// The bound of `rollback`: the stop and start back to the previous release.
pub const ROLLBACK_SECS: u64 = 8220;

/// The bound of `cleanup`: removing what the update left behind.
pub const CLEANUP_SECS: u64 = 120;

/// The time from the end of `cutover` to the end of the verdict: at least `verify` plus
/// `rollback`, so a failed verify can still be rolled back.
pub const VERIFY_DEADLINE_SECS: u64 = 9000;

/// How many times hostkeeper retries a failed `cleanup`.
pub const CLEANUP_RETRIES: u64 = 3;

/// The bounds of the installer's steps, in seconds: staging the package, activating it and
/// removing it.
pub const INSTALLER_BOUNDS: [(&str, u64); 3] = [
    ("stageSecs", 600),
    ("activateSecs", 300),
    ("removeSecs", 300),
];

/// One value of the hook's `params`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    /// A text, sent as `{"text": ...}`.
    Text(&'static str),
    /// A whole number, sent as `{"integer": "<n>"}`.
    Integer(u64),
}

/// The hook's `params`: exactly the keys of `js_agent::update_hook::params::KEYS`, which the hook
/// requires (a test holds the two equal and has the hook's reader accept the table). The bounds
/// mirror the ones of `hook.timeoutSecs`.
pub const PARAMS: [(&str, Param); 17] = [
    ("unit", Param::Text(RUN_UNIT)),
    ("state_dir", Param::Text("/var/lib/js-agent-update")),
    (
        "release_link",
        Param::Text("/var/lib/hostkeeper/packages/js-agent/current"),
    ),
    ("flags_file", Param::Text("/etc/js-agent/flags")),
    ("token_file", Param::Text("/etc/js-agent/token")),
    ("state_root", Param::Text("/var/lib/js-agent/state")),
    ("runtime", Param::Text("crun")),
    ("cordon_lease_secs", Param::Integer(300)),
    ("cordon_max_secs", Param::Integer(17355)),
    ("drain_wait_secs", Param::Integer(3600)),
    ("defer_retry_secs", Param::Integer(300)),
    ("prepare_timeout_secs", Param::Integer(PREPARE_SECS)),
    ("cutover_timeout_secs", Param::Integer(CUTOVER_SECS)),
    ("verify_timeout_secs", Param::Integer(VERIFY_SECS)),
    ("rollback_timeout_secs", Param::Integer(ROLLBACK_SECS)),
    ("cleanup_timeout_secs", Param::Integer(CLEANUP_SECS)),
    ("verify_deadline_secs", Param::Integer(VERIFY_DEADLINE_SECS)),
];

/// A whole number as proto3 JSON writes a 64-bit one: a string.
fn int64(n: u64) -> Value {
    Value::String(n.to_string())
}

/// The `params` table of the manifest.
#[must_use]
pub fn params() -> Value {
    let table: Map<String, Value> = PARAMS
        .iter()
        .map(|(key, param)| {
            let value = match param {
                Param::Text(text) => json!({ "text": text }),
                Param::Integer(n) => json!({ "integer": int64(*n) }),
            };
            ((*key).to_owned(), value)
        })
        .collect();
    Value::Object(table)
}

/// The hook manifest of js-agent.
#[must_use]
pub fn manifest() -> Value {
    json!({
        "package": "js-agent",
        "hook": {
            "command": HOOK_COMMAND,
            "timeoutSecs": {
                "prepare": int64(PREPARE_SECS),
                "cutover": int64(CUTOVER_SECS),
                "verify": int64(VERIFY_SECS),
                "rollback": int64(ROLLBACK_SECS),
                "cleanup": int64(CLEANUP_SECS),
            },
            "verifyDeadlineSecs": int64(VERIFY_DEADLINE_SECS),
            "cleanupRetries": CLEANUP_RETRIES,
        },
        "run": [RUN_UNIT],
        "params": params(),
        "minHostkeeper": MIN_HOSTKEEPER,
        "installerBounds": INSTALLER_BOUNDS
            .iter()
            .map(|(name, secs)| ((*name).to_owned(), int64(*secs)))
            .collect::<Map<String, Value>>(),
    })
}

/// The entry of the installer `apt` for the `.deb` of `version` whose SHA-256 is `digest`.
#[must_use]
pub fn entry(version: &str, digest: &str) -> Value {
    json!({
        "installer": "apt",
        "repository": REPOSITORY,
        "packageVersion": version,
        "digest": digest,
        "manifest": manifest(),
    })
}

/// The targets: an object keyed by platform, each with its one entry, for the `.deb` digests
/// `digests` (`(platform, sha256)`).
#[must_use]
pub fn targets(version: &str, digests: &[(Platform, String)]) -> Value {
    let by_platform: Map<String, Value> = digests
        .iter()
        .map(|(platform, digest)| {
            (
                platform.name().to_owned(),
                json!({ "entries": [entry(version, digest)] }),
            )
        })
        .collect();
    Value::Object(by_platform)
}
