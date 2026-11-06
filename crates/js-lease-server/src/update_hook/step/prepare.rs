//! `prepare`: look at the release link and the store, and write the record. It stops nothing,
//! cordons no host and waits for no job: the update restarts the server in place.
//!
//! In this order, each a read:
//! 1. the lock and the record; a record of this update is a rerun, one whose stop is begun is
//!    refused (`state_invalid`: run `rollback`);
//! 2. the release link: a symbolic link to a release directory beside the staged one, the one
//!    the params table names as the previous release, or no link at all on a first install.
//!    Anything else halts (76, `state_invalid`): the hook cannot name what runs;
//! 3. the store's schema version, against the range this release runs on (the hook is the
//!    staged release's binary): no schema, or one older than this release's oldest, fails
//!    `schema_unsupported`;
//! 4. the store's writer lock, probed without taking it: its holder is the running server,
//!    recorded so that `verify` can ask for another; nobody holding it is a stopped server,
//!    recorded as no holder.
//!
//! A store that does not answer either read defers (75, class environment, `store_unreachable`),
//! having written nothing. Otherwise the record is written: the holder, the schema, where the
//! link pointed. A rerun with a record observes again and writes nothing.

use std::path::Path;

use crate::update_hook::ports::{Schema, StoreUnreachable, Writer};
use crate::update_hook::{Done, Outcome, Params, Reason, Record, Report};

use super::{Env, failure, halt, open_state, release_link, save, success, success_with};

/// Runs `prepare`.
pub fn prepare(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    if loaded.as_ref().is_some_and(|record| record.stop_begun) {
        return Err(failure(
            Reason::StateInvalid,
            "the record says cutover has begun: run rollback",
        ));
    }
    // A record of a unit other than the declared one would be refused by every later step,
    // `rollback` and `cleanup` included, and never deleted.
    if params.unit != env.declaration.unit {
        return Err(failure(
            Reason::BadParams,
            format!(
                "--unit {} is not the declared unit {}",
                params.unit, env.declaration.unit
            ),
        ));
    }
    let link = release_link(env, params)?;
    let previous_target = link_target(env, params, &link)?;
    let mut record = Record::new(params, previous_target);
    record.store_schema = match env
        .ports
        .store
        .schema()
        .map_err(|why| deferred(params, why))?
    {
        Schema::Runs(version) => Some(version),
        Schema::Unchecked => None,
        Schema::Missing => {
            return Err(failure(
                Reason::SchemaUnsupported,
                "the store holds no schema",
            ));
        }
        Schema::TooOld { found, min } => {
            return Err(failure(
                Reason::SchemaUnsupported,
                format!("the store's schema is version {found}, older than this release's {min}"),
            ));
        }
    };
    record.holder = match env
        .ports
        .store
        .writer()
        .map_err(|why| deferred(params, why))?
    {
        Writer::Free => None,
        Writer::Held { holder } => Some(holder),
    };
    if loaded.is_some() {
        return Ok(success_with(
            "the record of this update is there already and is kept",
        ));
    }
    save(env, &held, &record)?;
    Ok(success())
}

/// The store did not answer: nothing was done, try later.
fn deferred(params: &Params, why: StoreUnreachable) -> Done {
    Done {
        outcome: Outcome::Deferred,
        report: Report {
            retry_after_secs: Some(params.defer_retry_secs),
            ..super::store_down(why).report
        },
    }
}

/// Where the release link points: a release directory beside the staged one, the previous
/// release the params table names, or nothing on a first install. The staged release's own
/// directory must be a declared one.
fn link_target(
    env: &Env<'_>,
    params: &Params,
    link: &Path,
) -> Result<Option<std::path::PathBuf>, Done> {
    let releases = params.staged_release.parent().filter(|dir| {
        Some(*dir) != link.parent()
            && *dir != env.declaration.state_dir
            && env.declaration.write_paths.iter().any(|path| path == dir)
    });
    let Some(releases) = releases else {
        return Err(failure(
            Reason::BadParams,
            format!(
                "the staged release {} is not directly inside a declared release directory",
                params.staged_release.display()
            ),
        ));
    };
    let target = env.ports.link.target(link).map_err(|why| {
        halt(
            Reason::StateInvalid,
            format!(
                "{} cannot be read as a symbolic link: {why}",
                link.display()
            ),
        )
    })?;
    if target
        .as_deref()
        .is_some_and(|t| t.parent() != Some(releases))
    {
        return Err(halt(
            Reason::StateInvalid,
            format!(
                "{} points outside the release directory {}",
                link.display(),
                releases.display()
            ),
        ));
    }
    if target != params.previous_release {
        return Err(halt(
            Reason::StateInvalid,
            format!(
                "{} points at {}, not at the previous release the params name ({})",
                link.display(),
                shown(target.as_deref()),
                shown(params.previous_release.as_deref()),
            ),
        ));
    }
    Ok(target)
}

/// A path for a message, or `nothing`.
fn shown(path: Option<&Path>) -> String {
    path.map_or_else(|| "nothing".to_owned(), |path| path.display().to_string())
}
