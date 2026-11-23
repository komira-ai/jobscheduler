//! The environment of a native run: the base the agent builds (the body's own `env`, or the
//! bundle's rules applied to it, then `TMPDIR`, `DEVELOPER_DIR` for a run that names an Xcode
//! and, for a run with files, `JS_FILES_DIR`), with a job start's environment
//! (`RunSpec::env`) laid over it, and then the names of the run's account ([`account`]).
//!
//! The whole environment is the supervisor's, which the agent starts with exactly this list
//! (nothing inherited) and which passes it to the job unchanged, adding nothing. No value is
//! ever put on the command line: a process's arguments are readable by every user of the Mac,
//! its environment only by its own user and root.

use js_agent_wire::NativeSpec;
use js_types::env::env_name_reserved;
use std::path::Path;

use crate::lifecycle::RuntimeError;

/// The variable `xcrun` keeps its cache file at: with it the cache is made at the path it
/// names, inside the run directory, rather than in the account's per-user temporary folder
/// (which the profile grants too).
pub(super) const XCRUN_DB: &str = "xcrun_db";

/// The names the native runtime sets from the run's account, last, beside the names every
/// runtime owns ([`js_types::env::OWNED_ENV_NAMES`]): a job or a body that sets one is
/// refused before an account is lent ([`check_names`]).
pub(super) const ACCOUNT_ENV: [&str; 5] =
    ["HOME", "CFFIXED_USER_HOME", "USER", "LOGNAME", XCRUN_DB];

/// The environment of a run that pins no bundle: the job's own, then `TMPDIR`, then
/// `DEVELOPER_DIR` for a run that names an Xcode (`developer_dir`, its `Contents/Developer`).
/// The name is the agent's own, so the job's own environment never holds it.
pub(super) fn plain(
    native: &NativeSpec,
    tmp: &Path,
    developer_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = native
        .env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    env.push(("TMPDIR".to_owned(), tmp.display().to_string()));
    env.extend(developer_dir.map(|dir| ("DEVELOPER_DIR".to_owned(), dir.display().to_string())));
    env
}

/// The names of the run's account, which the runtime sets because the supervisor starts the
/// program itself and sets nothing: `HOME` and `CFFIXED_USER_HOME` (which Foundation reads
/// for the home) are the job's home in the run directory (`job_home`), `USER` and `LOGNAME`
/// the account's name, and [`XCRUN_DB`] a file in the run's `TMPDIR` (`tmp`).
pub(super) fn account(job_home: &Path, tmp: &Path, name: &str) -> Vec<(String, String)> {
    let home = job_home.display().to_string();
    vec![
        ("HOME".to_owned(), home.clone()),
        ("CFFIXED_USER_HOME".to_owned(), home),
        ("USER".to_owned(), name.to_owned()),
        ("LOGNAME".to_owned(), name.to_owned()),
        (
            XCRUN_DB.to_owned(),
            tmp.join(XCRUN_DB).display().to_string(),
        ),
    ]
}

/// `base` with the job start's environment `job` laid over it: a name `job` sets is dropped
/// from `base`, and `job` follows in its own order, so the job's value is the one the run
/// sees. The names `job` may hold were checked before an account was lent
/// ([`crate::lifecycle::check_run_env`] and [`check_names`]): none is reserved, so `TMPDIR`,
/// `JS_FILES_DIR` and the account's names are never replaced.
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
/// one. The wire checks these too; this is the last check before an account is lent, for a
/// body that did not pass through it.
pub(super) fn check_body(native: &NativeSpec) -> Result<(), RuntimeError> {
    match native.env.keys().find(|name| env_name_reserved(name)) {
        Some(name) => Err(RuntimeError::new(format!(
            "native.env: {name} is a reserved name"
        ))),
        None => check_names("native.env", native.env.keys().map(String::as_str)),
    }
}

/// Refuses an environment (`what`, its names `names`) that sets a name of the run's account
/// ([`ACCOUNT_ENV`]): the runtime sets those last, and a job that set `HOME` would run with a
/// home it chose.
pub(super) fn check_names<'a>(
    what: &str,
    mut names: impl Iterator<Item = &'a str>,
) -> Result<(), RuntimeError> {
    match names.find(|name| ACCOUNT_ENV.contains(name)) {
        Some(name) => Err(RuntimeError::new(format!(
            "{what}: {name} is set by the runtime for the run's account"
        ))),
        None => Ok(()),
    }
}
