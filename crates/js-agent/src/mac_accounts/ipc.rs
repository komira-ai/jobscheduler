//! The System V IPC objects of a uid (shared memory, semaphores, message queues): they
//! outlive every process of the uid, and the account too, so the deletion of an account
//! removes them and judges by the outcome: `ipcs` lists none of the uid's afterwards (and a
//! fresh listing after the account's record is gone, which names an object's owner by the
//! uid's number, shows the same).
//!
//! `ipcs -a` prints one object per line, `<kind> <id> <key> <mode> <owner> <group>
//! <creator> <creator's group> ...`, with the kind `q`, `m` or `s`; owner and creator are
//! an account's name, or the uid when no record has it. An object is the uid's when it owns
//! it or created it: a creator keeps its access after giving the object away (`IPC_SET`),
//! so an object given to another owner is removed too.
//!
//! POSIX shared memory objects and named semaphores are listed by no interface of macOS
//! (they have names but no directory), so a deletion cannot find one to remove, and one
//! lasts until it is unlinked or the Mac restarts. The job profile's floor denies creating
//! shared memory; a named semaphore a job left stays, and holds a count, not data.

use super::host::AccountHost;

/// The program that lists the IPC objects.
const IPCS: &str = "/usr/bin/ipcs";

/// The program that removes one.
const IPCRM: &str = "/usr/bin/ipcrm";

/// One object as `ipcs -a` lists it: its `ipcrm` kind flag and its id.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Object {
    kind: &'static str,
    id: String,
}

/// The objects in `listing` (what `ipcs -a` printed) that `name` or `uid` owns or created.
fn owned(listing: &str, name: &str, uid: u32) -> Vec<Object> {
    let number = uid.to_string();
    listing
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let kind = match fields.first() {
                Some(&"q") => "-q",
                Some(&"m") => "-m",
                Some(&"s") => "-s",
                _ => return None,
            };
            let owner = *fields.get(4)?;
            let id = fields.get(1)?.parse::<u64>().ok()?;
            let mine = |who: &str| who == name || who == number;
            (mine(owner) || fields.get(6).is_some_and(|creator| mine(creator))).then(|| Object {
                kind,
                id: id.to_string(),
            })
        })
        .collect()
}

/// The IPC objects `name` (with `uid`) owns, in a fresh listing.
fn objects(host: &dyn AccountHost, name: &str, uid: u32) -> Result<Vec<Object>, String> {
    let output = host
        .command(IPCS, &["-a"])
        .map_err(|why| format!("running ipcs: {why}"))?;
    if !output.success {
        return Err(format!("ipcs failed: {}", output.stderr.trim()));
    }
    Ok(owned(&output.stdout, name, uid))
}

/// The IPC objects `name` (with `uid`) owns in a fresh listing, each as its `ipcrm` kind
/// flag and id (`-m 65538`).
///
/// # Errors
/// The listing failed.
pub(super) fn left(host: &dyn AccountHost, name: &str, uid: u32) -> Result<Vec<String>, String> {
    Ok(objects(host, name, uid)?
        .iter()
        .map(|object| format!("{} {}", object.kind, object.id))
        .collect())
}

/// Removes every IPC object of `name` (with `uid`) and looks again: an `ipcrm` that fails
/// is only logged, and an object still listed afterwards fails the step. With none listed
/// at first there is nothing to remove or look at again.
///
/// # Errors
/// The listing failed, or an object is still listed after its removal.
pub(super) fn remove(host: &dyn AccountHost, name: &str, uid: u32) -> Result<(), String> {
    let found = objects(host, name, uid)?;
    if found.is_empty() {
        return Ok(());
    }
    for object in found {
        match host.command(IPCRM, &[object.kind, &object.id]) {
            Ok(output) if output.success => {}
            Ok(output) => tracing::info!(
                "ipcrm {} {} (the outcome decides): {}",
                object.kind,
                object.id,
                output.stderr.trim()
            ),
            Err(why) => {
                tracing::info!(
                    "ipcrm {} {} (the outcome decides): {why}",
                    object.kind,
                    object.id
                );
            }
        }
    }
    let remaining = left(host, name, uid)?;
    match remaining.is_empty() {
        true => Ok(()),
        false => Err(format!(
            "IPC objects of uid {uid} remain after their removal: {}",
            remaining.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests;
