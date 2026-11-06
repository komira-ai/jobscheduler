//! The System V IPC objects of a uid (shared memory, semaphores, message queues): they
//! outlive every process of the uid, and the user too, so the reset and the deletion of
//! a pool user remove them and judge by the outcome: `ipcs` lists none of the uid's
//! afterwards.
//!
//! `ipcs -a` prints one object per line, `<kind> <id> <key> <mode> <owner> <group> ...`,
//! with the kind `q`, `m` or `s`; the owner is the user's name, or the uid when no user
//! record has it. POSIX shared memory and semaphores are not listed by any interface of
//! macOS (they have names but no directory), so they cannot be found to be removed.

use super::Helper;
use super::teardown::TeardownError;

/// The program that lists the IPC objects.
pub const IPCS: &str = "/usr/bin/ipcs";

/// The program that removes one.
pub const IPCRM: &str = "/usr/bin/ipcrm";

/// One object as `ipcs -a` lists it: its kind's letter and its id.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Object {
    kind: &'static str,
    id: String,
}

/// The objects in `listing` (what `ipcs -a` printed) that `name` or `uid` owns.
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
            (owner == name || owner == number).then(|| Object {
                kind,
                id: id.to_string(),
            })
        })
        .collect()
}

impl Helper {
    /// The IPC objects `name` (with `uid`) owns.
    fn ipc_objects(&self, name: &str, uid: u32) -> Result<Vec<Object>, TeardownError> {
        let failed = |why: String| TeardownError::Ipc { uid, why };
        let output = self
            .host
            .command(IPCS, &["-a"])
            .map_err(|why| failed(why.to_string()))?;
        if !output.success {
            return Err(failed(format!("ipcs failed: {}", output.stderr.trim())));
        }
        Ok(owned(&output.stdout, name, uid))
    }

    /// Removes every IPC object of `name` (with `uid`) and looks again: an `ipcrm` that
    /// fails is only logged, and an object still listed afterwards fails the step. With
    /// none listed at first there is nothing to remove or look at again.
    pub(super) fn remove_ipc(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let objects = self.ipc_objects(name, uid)?;
        if objects.is_empty() {
            return Ok(());
        }
        for object in objects {
            let removed = self.host.command(IPCRM, &[object.kind, &object.id]);
            match removed {
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
        let left = self.ipc_objects(name, uid)?;
        match left.is_empty() {
            true => Ok(()),
            false => Err(TeardownError::IpcLeft {
                uid,
                left: left
                    .iter()
                    .map(|o| format!("{} {}", o.kind, o.id))
                    .collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: an object of another owner taken for the uid's, a header or a section
    /// title read as an object, or the uid's spelling as a number (after its record
    /// went) missed.
    #[test]
    fn only_the_uid_s_objects_are_listed() {
        let listing = "IPC status from <running system> as of Thu Nov  5 10:00:00 2026\n\
             T     ID     KEY        MODE       OWNER    GROUP\n\
             Message Queues:\n\
             q  65536 0x00000001 --rw-------  kbf-lease-pool-0    staff\n\
             Shared Memory:\n\
             m  65537 0x00000002 --rw-------     root    wheel\n\
             m  65538 0x00000003 --rw-------      601    staff\n\
             m  bad   0x00000004 --rw-------      601    staff\n\
             m  65540\n\
             Semaphores:\n\
             s  65539 0x00000005 --ra-------  kbf-lease-pool-0    staff\n";
        assert_eq!(
            owned(listing, "kbf-lease-pool-0", 601),
            [
                Object {
                    kind: "-q",
                    id: "65536".to_owned()
                },
                Object {
                    kind: "-m",
                    id: "65538".to_owned()
                },
                Object {
                    kind: "-s",
                    id: "65539".to_owned()
                },
            ]
        );
        assert!(owned(listing, "kbf-lease-pool-1", 602).is_empty());
    }
}
