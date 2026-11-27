//! [`Pending`]: a batch held at the point of commit. Test support: it is how a test holds one
//! commit back while another goes through.

use super::tx::Tx;
use super::{Fail, PostgresStore, applied};
use crate::{Commit, Conflict, StoreError, Txn};

/// A batch run in a database transaction that is not committed yet. Test support: it is
/// how a test holds one commit back while another goes through.
#[doc(hidden)]
pub struct Pending<'s> {
    store: &'s PostgresStore,
    tx: Tx<'s>,
    results: Vec<Result<Commit, Conflict>>,
}

impl Pending<'_> {
    /// Commits, and returns the result of each transaction.
    ///
    /// # Errors
    /// [`StoreError`] when the commit fails.
    pub fn commit(self) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let Pending { store, tx, results } = self;
        store
            .commit(tx, applied(&results))
            .map_err(Fail::into_store)?;
        Ok(results)
    }
}

impl PostgresStore {
    /// Runs `txn` to the point of commit and holds it there: when it writes, it holds the
    /// commit head. Test support.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails.
    #[doc(hidden)]
    pub fn begin(&self, txn: Txn) -> Result<Pending<'_>, StoreError> {
        let batch = [txn];
        let (tx, results) = self.retrying(|| self.run_batch(&batch))?;
        Ok(Pending {
            store: self,
            tx,
            results,
        })
    }
}
