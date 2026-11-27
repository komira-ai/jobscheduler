//! The runtime's side of the account pool ([`crate::mac_accounts`]): which pool the flags
//! name, the job name a run's account is lent under, opening the pool, and retiring an
//! account after its run.
//!
//! The pool is opened only when the agent holds Full Disk Access: without it the pool can
//! delete no account ([`AccountError::NoFullDiskAccess`]), and an agent that stopped there
//! could not report `full-disk-access: not-granted`. Such an agent keeps no pool and refuses
//! every native job `no-full-disk-access` (`native/grant.rs`).
//!
//! The pool is opened with the jobs the agent takes back after a restart
//! (`native/adopt.rs`): the account lent to one of them is kept, and every other account a
//! run of an earlier process held is retired by the open.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_mac_session::full_disk_access::FullDiskAccess;

use super::fail;
use crate::lifecycle::RuntimeError;
use crate::mac_accounts::{Account, AccountError, Pool, Retired, Slots};

/// The account pool the flags name (`--native-accounts`, `--native-account-range`,
/// `--native-account-ready`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accounts {
    /// The pool's state directory: its ledger.
    pub state: PathBuf,
    /// The uids of its slots.
    pub slots: Slots,
    /// How many accounts it keeps ready.
    pub ready: usize,
}

/// The job name the account of the lease `lease` (`<term>.<sequence>`) is lent under: the
/// pool takes `a-z`, `0-9` and `-` only.
pub(super) fn job_of(lease: &str) -> String {
    lease.replace('.', "-")
}

/// Opens the pool of `accounts` on this Mac when `held` is [`FullDiskAccess::Granted`],
/// keeping the accounts of the jobs `adopted` names; `None` otherwise (nothing is made or
/// deleted). A state directory that is
/// not there is made first, mode 0700 (the agent's, root's); its parent must exist.
///
/// # Errors
///
/// The state directory cannot be made, or the pool refused to open, naming why.
pub(super) fn open(
    accounts: &Accounts,
    held: FullDiskAccess,
    adopted: &BTreeSet<String>,
) -> Result<Option<Arc<Pool>>, RuntimeError> {
    if held != FullDiskAccess::Granted {
        return Ok(None);
    }
    make_state(&accounts.state).map_err(|why| fail(&accounts.state.display(), &why))?;
    let pool = Pool::open(&accounts.state, accounts.slots, accounts.ready, adopted);
    pool.map(|pool| Some(Arc::new(pool)))
        .map_err(|why: AccountError| fail(&"the account pool", &why))
}

/// Makes the directory `state`, mode 0700, unless it is there (the pool then checks it).
fn make_state(state: &Path) -> io::Result<()> {
    match fs::create_dir(state) {
        Ok(()) => fs::set_permissions(state, fs::Permissions::from_mode(0o700)),
        Err(why) if why.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(why) => Err(why),
    }
}

/// Retires `account` from `pool` after its run: what it did, or a [`RuntimeError`] that
/// names the step the account is held at (its uid is chosen by no one until a later retire
/// shows it gone).
pub(super) fn retire(pool: &Pool, account: Account) -> Result<Retired, RuntimeError> {
    let name = account.name.clone();
    pool.retire(account)
        .inspect(|retired| {
            tracing::info!(
                "native: account {name} retired: killed {:?}, gone {:?}, in {} ms",
                retired.killed,
                retired.gone,
                retired.delete_ms
            );
        })
        .map_err(|held| RuntimeError::new(held.to_string()))
}

/// `error`, the error of a start, and what could not be undone after it: an account that
/// is held stays out of the pool until a later retire shows it gone, and its lease stays
/// in the journal.
pub(super) fn with_held(error: RuntimeError, held: Option<RuntimeError>) -> RuntimeError {
    match held {
        None => error,
        Some(held) => RuntimeError::new(format!(
            "{}; and the run's account could not be retired ({})",
            error.0, held.0
        )),
    }
}
