//! The environment of a native run: the base the agent builds (the body's own `env`, or the
//! bundle's rules applied to it, then `TMPDIR` and, for a run with files, `JS_FILES_DIR`),
//! with a job start's environment (`RunSpec::env`) laid over it.
//!
//! The whole environment goes to the helper as the `env` of its `run` request, which the
//! helper sets in the child's environment block before it executes `sandbox-exec`. No value
//! is ever put on the command line: a process's arguments are readable by every user of the
//! Mac, its environment only by its own user and root.

use js_agent_wire::NativeSpec;
use js_types::env::env_name_reserved;
use std::path::Path;

use crate::lifecycle::RuntimeError;

/// The environment of a run that pins no bundle: the job's own, then `TMPDIR`.
pub(super) fn plain(native: &NativeSpec, tmp: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = native
        .env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    env.push(("TMPDIR".to_owned(), tmp.display().to_string()));
    env
}

/// `base` with the job start's environment `job` laid over it: a name `job` sets is dropped
/// from `base`, and `job` follows in its own order, so the job's value is the one the run
/// sees. The names `job` may hold were checked before a user was made
/// ([`crate::lifecycle::check_run_env`]): none is reserved, so `TMPDIR`, `JS_FILES_DIR` and
/// the names the helper sets itself (`HOME`, `USER`, `LOGNAME`) are never replaced.
pub(super) fn with_job(
    base: Vec<(String, String)>,
    job: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = base
        .into_iter()
        .filter(|(name, _)| !job.iter().any(|(set, _)| set == name))
        .collect();
    env.extend(job.iter().cloned());
    env
}

/// Refuses a body whose environment names a variable a job may not set
/// ([`js_types::env::env_name_reserved`]: the names that steer a shell, the agent's own
/// names, and the prefixes of the loader, the sandbox and imported shell functions). No
/// shell of the agent's reads the environment any more, but the program a job names may be
/// one. The wire checks these too; this is the last check before a user is made, for a
/// body that did not pass through it.
pub(super) fn check_body(native: &NativeSpec) -> Result<(), RuntimeError> {
    match native.env.keys().find(|name| env_name_reserved(name)) {
        Some(name) => Err(RuntimeError::new(format!(
            "native.env: {name} is a reserved name"
        ))),
        None => Ok(()),
    }
}
