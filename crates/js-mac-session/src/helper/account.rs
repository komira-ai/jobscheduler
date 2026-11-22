//! `user-create`: the lease's own account, and the choice of its uid.
//!
//! Every lease gets an account that did not exist before it, `kbf-lease-<term>-<seq>`
//! ([`crate::lease::user_name`]), which `user-delete` deletes when the lease ends
//! (`super::teardown`):
//!
//! - hidden, shell [`SHELL`](super::SHELL) (`/usr/bin/false`), no password, so no login and
//!   no secure token; never an administrator;
//! - the primary group `--lease-gid` and no other;
//! - the home `<homes>/<name>`, made by the helper, owned by the account, mode 0700, and
//!   refused when the path exists: a new account takes nothing of what another left.
//!
//! The order is the crash safety. The lease id is refused if the ledger has seen it, and a
//! name longer than 31 bytes is refused before anything is recorded. The uid is chosen
//! ([`Helper::free_uid`]), the lease is recorded with it (`created`, durably), and only then
//! are the home and the account made. A failure after `created` leaves the lease live, so
//! its uid is held and the boot reset (or the caller's `kill-uid` and `user-delete`)
//! deletes whatever was made.
//!
//! A uid is free when all of these hold: it is in the helper's range; no live lease holds
//! it; no user record has it; no process runs as it; no directory under the per-user
//! folders is owned by it; and nothing in the places the sweep covers is owned by it. The
//! helper takes the first free uid after the one the ledger's last `created` record names,
//! wrapping at the end of the range, rather than the lowest free one: a uid is then used
//! again as late as the range allows, so anything a proof could not see has the longest
//! time to show in a leak scan before the same uid runs again. An error in looking at a
//! uid is not "free": the uid is passed over, and the refusal, when no uid is free, names
//! why each is not.

use js_types::LeaseId;

use super::teardown::TeardownError;
use super::{Helper, NewUser};
use crate::lease::{Dotted, account_name, user_name};
use crate::ledger::Ledger;
use crate::sweep::{owned_entries, owned_left};

/// How many uids a refusal for want of a free one names; it counts the rest.
const REASONS: usize = 8;

/// The user of a live lease: its name and its uid.
#[derive(Debug)]
pub(super) struct LeaseUser {
    pub(super) name: String,
    pub(super) uid: u32,
}

impl Helper {
    /// The user of the live `lease`: its own account, `kbf-lease-<term>-<seq>`.
    pub(super) fn lease_user(&self, ledger: &Ledger, lease: LeaseId) -> Result<LeaseUser, String> {
        Ok(LeaseUser {
            name: user_name(lease),
            uid: self.live_uid(ledger, lease)?,
        })
    }

    /// `user-create <lease>`: the lease gets an account of its own, never an
    /// administrator, with a new home only it can enter. Returns its uid. See the module
    /// documentation.
    ///
    /// # Errors
    /// Why it was refused or failed: the lease id was used before, its account name is too
    /// long, no uid is free (the refusal names why each is not), the ledger could not
    /// record it, or the home or the account could not be made (the lease then stays live).
    pub fn user_create(&self, lease: &str) -> Result<u32, String> {
        let lease = crate::lease::parse_lease(lease)?;
        let mut ledger = self.lock();
        if ledger.get(lease).is_some() {
            return Err(format!(
                "lease {} was used before; a lease id is used once",
                Dotted(lease)
            ));
        }
        let name = account_name(lease)?;
        let uid = self.free_uid(&ledger)?;
        self.first_lease_user()?;
        ledger
            .record_created(lease, uid)
            .map_err(|why| format!("recording lease {}: {why}", Dotted(lease)))?;
        self.create_account(&name, uid)?;
        tracing::info!(lease = %Dotted(lease), uid, "created lease user {name}");
        Ok(uid)
    }

    /// The first free uid of the range after the one the ledger's last `created` record
    /// names.
    ///
    /// # Errors
    /// No uid is free: the text names up to [`REASONS`] uids and why each is not.
    fn free_uid(&self, ledger: &Ledger) -> Result<u32, String> {
        let range = self.settings.range;
        let held = ledger.held_uids();
        let mut busy = Vec::new();
        for uid in range.after(ledger.last_uid()) {
            let why = match held.contains(&uid) {
                true => Some("held by a live lease".to_owned()),
                false => self.why_busy(uid),
            };
            match why {
                None => return Ok(uid),
                Some(why) => busy.push(format!("{uid}: {why}")),
            }
        }
        let more = busy.len().saturating_sub(REASONS);
        busy.truncate(REASONS);
        if more > 0 {
            busy.push(format!("and {more} more"));
        }
        Err(format!("no free uid in {range}: {}", busy.join("; ")))
    }

    /// Why `uid`, which no live lease holds, is not free; `None` when it is.
    fn why_busy(&self, uid: u32) -> Option<String> {
        match self.host.uid_taken(uid) {
            Err(why) => return Some(format!("looking up its record: {why}")),
            Ok(true) => return Some("a user record has it".to_owned()),
            Ok(false) => {}
        }
        match self.host.live_processes(uid) {
            Err(why) => return Some(format!("listing its processes: {why}")),
            Ok(left) if !left.is_empty() => {
                return Some(format!("{} processes run as it", left.len()));
            }
            Ok(_) => {}
        }
        let folders = &self.settings.user_folders;
        match owned_entries(folders, uid) {
            Err(why) => return Some(format!("searching {}: {why}", folders.display())),
            Ok(found) if !found.is_empty() => {
                return Some(format!("{} is owned by it", found[0].display()));
            }
            Ok(_) => {}
        }
        match owned_left(&self.swept_places(), None, uid) {
            Err(why) => Some(format!("looking in the swept places: {why}")),
            Ok(found) if !found.is_empty() => {
                Some(format!("{} is owned by it", found[0].display()))
            }
            Ok(_) => None,
        }
    }

    /// Makes the home and the account `name`, with `uid`.
    pub(super) fn create_account(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let home = self.settings.homes.join(name);
        self.host
            .make_home(&self.settings.homes, name, uid, self.settings.gid)
            .map_err(|source| TeardownError::MakeHome {
                path: home.clone(),
                source,
            })?;
        let user = NewUser {
            name: name.to_owned(),
            uid,
            gid: self.settings.gid,
            home,
        };
        self.host
            .create_user(&user)
            .map_err(|source| TeardownError::Create {
                name: name.to_owned(),
                source,
            })
    }
}
