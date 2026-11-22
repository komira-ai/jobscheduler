//! The runtime's side of the account pool ([`crate::mac_accounts`]): which pool the flags
//! name, the job name a run's account is lent under, opening the pool, and retiring an
//! account after its run.
//!
//! The pool is opened only when the agent holds Full Disk Access: without it the pool can
//! delete no account ([`AccountError::NoFullDiskAccess`]), and an agent that stopped there
//! could not report `full-disk-access: not-granted`. Such an agent keeps no pool and refuses
//! every native job `no-full-disk-access` (`native/grant.rs`).
//!
//! The pool is always opened with no adopted job: a restart of the agent ends every native
//! run, so every account a run of an earlier process held is retired by the open.

use std::collections::BTreeSet;
use std::path::PathBuf;
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

/// Opens the pool of `accounts` on this Mac when `held` is [`FullDiskAccess::Granted`], with
/// no adopted job; `None` otherwise (nothing is made or deleted).
///
/// # Errors
///
/// The pool refused to open, naming why.
pub(super) fn open(
    accounts: &Accounts,
    held: FullDiskAccess,
) -> Result<Option<Arc<Pool>>, RuntimeError> {
    if held != FullDiskAccess::Granted {
        return Ok(None);
    }
    let pool = Pool::open(
        &accounts.state,
        accounts.slots,
        accounts.ready,
        &BTreeSet::new(),
    );
    pool.map(|pool| Some(Arc::new(pool)))
        .map_err(|why: AccountError| fail(&"the account pool", &why))
}

/// Retires `account` from `pool` after its run: what it did, or a [`RuntimeError`] that
/// names the step the account is held at (its uid is chosen by no one until a later retire
/// shows it gone).
pub(super) fn retire(pool: &Pool, account: Account) -> Result<Retired, RuntimeError> {
    let name = account.name.clone();
    pool.retire(account)
        .inspect(|retired| {
            tracing::info!(
                "native: account {name} retired, {} processes killed, gone in {} ms",
                retired.killed.len(),
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
