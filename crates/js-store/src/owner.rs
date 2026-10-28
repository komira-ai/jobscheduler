//! Which frontend owns a daemon, when several frontends share one state store.
//!
//! A daemon is connected to one frontend at a time, and only that frontend may write the
//! leases of the daemon's node: grant one, give one up, take a result for one. Two
//! frontends that both believed they were its owner could each grant the node an
//! operation, or one could give up a lease the daemon still runs under the other.
//!
//! The `owner/<worker>` row says who owns a node: the id of a frontend and a counter, eight
//! bytes each, big-endian. A daemon's `Hello` makes the frontend it says it to the owner,
//! by a compare-and-set that adds one to the counter ([`Rows::arrive`]). The frontend
//! remembers the counter it wrote, and every write of a lease of that node (a grant, a
//! lease given up, a result) reads the row and is conditional on it ([`Rows::guard`]): it
//! names this frontend at that counter, in the very transaction. A frontend that finds
//! another name or counter, because the daemon said `Hello` elsewhere meanwhile, writes
//! nothing and fails with [`StateError::StaleOwner`]. That error is about one node: it
//! drops what the frontend holds for that node and nothing else, and does not mark the
//! frontend as superseded as a refusal of the store as `Fenced` does
//! ([`js_state::StoreError::Fenced`]).
//!
//! A node this frontend never claimed (a lease taken up by a request that joined its
//! operation, before its daemon said `Hello` here) has no counter to check: a write for it
//! is conditional on its lease rows alone. A node whose claim failed after a `Hello` (the
//! store could not be read or written, or another frontend won every try) is lost like one
//! the daemon left: nothing is written for it until a claim succeeds.
//!
//! The frontend also acknowledges a node's heartbeats only while it owns the node
//! ([`Rows::owns`]), looking at the row again when the last look is [`OWNER_CACHE_TTL`]
//! old.

use std::sync::PoisonError;
use std::time::Duration;

use js_sched::DaemonInstance;
use js_state::{Cond, Txn, Versioned};
use js_types::sched::{FarmTime, WorkerId};

use crate::blocks::contended;
use crate::codec::Value;
use crate::load::StoredOperation;
use crate::{Rows, StateError, keys};

/// How long a frontend trusts its last look at an `owner/` row before it reads the row
/// again to acknowledge a heartbeat. The frontend a daemon moved to waits this long beyond
/// the handover grace before it takes a lease the daemon does not list as lost, because the
/// frontend the daemon left may go on acknowledging the old daemon process this long after
/// the move.
pub const OWNER_CACHE_TTL: Duration = Duration::from_secs(2);

/// The two numbers of a 16-byte row, big-endian; `None` for a row of another length.
pub(crate) fn pair_of(row: &[u8]) -> Option<(u64, u64)> {
    let (high, rest) = row.split_first_chunk::<8>()?;
    let low = <[u8; 8]>::try_from(rest).ok()?;
    Some((u64::from_be_bytes(*high), u64::from_be_bytes(low)))
}

/// The 16-byte row of two numbers, big-endian.
pub(crate) fn pair_row(high: u64, low: u64) -> Vec<u8> {
    [high.to_be_bytes(), low.to_be_bytes()].concat()
}

/// What an `owner/<worker>` row says: the frontend that owns the node, and how many times
/// a daemon has said `Hello` to a frontend as the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    /// The frontend's id, as it wrote it in the row.
    pub frontend: u64,
    /// How many times the node has been claimed.
    pub counter: u64,
}

impl Owner {
    fn from_row(key: &str, row: &Versioned) -> Result<Self, StateError> {
        let Some((frontend, counter)) = pair_of(&row.value) else {
            return Err(StateError::corrupt(key, "not a 16-byte owner"));
        };
        Ok(Self { frontend, counter })
    }
}

/// What a frontend holds of the ownership of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// The frontend wrote the row at `counter`, and last read it at `checked`.
    Held { counter: u64, checked: FarmTime },
    /// The row names another frontend or another counter, or the frontend could not make
    /// it name itself: until the node says `Hello` here again, nothing is written for it.
    Lost,
}

impl Ownership {
    const fn held(counter: u64, checked: FarmTime) -> Self {
        Self::Held { counter, checked }
    }
}

/// What a `Hello` finds in the store, as the frontend that made itself the node's owner
/// reads it.
#[derive(Debug)]
pub struct Arrival {
    /// This frontend's id, as it wrote it in the row.
    pub frontend: u64,
    /// The owner the row named before, if it named one.
    pub previous: Option<Owner>,
    /// The unfinished operations whose lease rows name the node.
    pub leases: Vec<StoredOperation>,
    /// The daemon process the node's `session/` row names: the one that said `Hello` to the
    /// frontend that owned the node, as far as the row has not expired.
    pub session: Option<DaemonInstance>,
}

impl Rows {
    /// A daemon said `Hello` as `worker` to this frontend: it becomes the node's owner at
    /// the next counter, and reads what it takes over ([`Arrival`]). The write is retried
    /// when another frontend writes the row first, and if every try loses, the node is
    /// given up to the other (nothing is written for it until it says `Hello` here again).
    ///
    /// A failure to read what the claim takes over leaves the node lost to this frontend
    /// too, with the owner the claim replaced kept for the retry, whose claim finds this
    /// frontend's own row.
    ///
    /// # Errors
    /// The store cannot be read or written, the row is not an owner, or another frontend
    /// won every try ([`StateError::Conflict`]).
    pub fn arrive(&self, worker: &WorkerId, now: FarmTime) -> Result<Arrival, StateError> {
        let previous = self.claim(worker, now)?;
        match self.read_arrival(worker, previous) {
            Ok(arrival) => {
                self.forget_handover(worker);
                Ok(arrival)
            }
            Err(error) => {
                self.remember_handover(worker, previous);
                self.remember_owner(worker, Ownership::Lost);
                Err(error)
            }
        }
    }

    /// What the claim of `worker`, which replaced `previous`, takes over.
    fn read_arrival(
        &self,
        worker: &WorkerId,
        previous: Option<Owner>,
    ) -> Result<Arrival, StateError> {
        let leases = self.held_by(worker)?;
        let session = self.session_instance(worker)?;
        Ok(Arrival {
            frontend: self.frontend,
            previous,
            leases,
            session,
        })
    }

    fn remember_handover(&self, worker: &WorkerId, previous: Option<Owner>) {
        let mut handovers = self
            .handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handovers.insert(worker.clone(), previous);
    }

    fn forget_handover(&self, worker: &WorkerId) {
        let mut handovers = self
            .handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handovers.remove(worker);
    }

    /// The owner a claim of `worker` replaced, given the one it read in the row: the owner
    /// an earlier claim whose arrival did not complete replaced, if the row is that claim's
    /// own (this frontend's), else the row's.
    fn replaced(&self, worker: &WorkerId, read: Option<Owner>) -> Option<Owner> {
        let own = read.is_some_and(|owner| owner.frontend == self.frontend);
        let handovers = self
            .handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match handovers.get(worker) {
            Some(before) if own => *before,
            _ => read,
        }
    }

    /// Makes this frontend the owner of `worker`: the row, with its counter raised by one,
    /// put where the row is as it was read. Returns the owner it named before.
    fn claim(&self, worker: &WorkerId, now: FarmTime) -> Result<Option<Owner>, StateError> {
        let key = keys::owner(worker);
        let claimed = contended(|| {
            let row = self.store.get(&key)?;
            let previous = match &row {
                Some(row) => Some(Owner::from_row(&key, row)?),
                None => None,
            };
            let cond = row.map_or(Cond::Absent, |row| Cond::Version(row.version));
            let counter = previous.map_or(1, |owner| owner.counter.saturating_add(1));
            let value = pair_row(self.frontend, counter);
            let txn = Txn::new().put(key.clone(), value, cond);
            self.commit(txn)?;
            Ok((previous, counter))
        });
        match claimed {
            Ok((previous, counter)) => {
                self.remember_owner(worker, Ownership::held(counter, now));
                Ok(self.replaced(worker, previous))
            }
            Err(error) => {
                self.remember_owner(worker, Ownership::Lost);
                Err(error)
            }
        }
    }

    /// The daemon process the `session/` row of `worker` names, if the row is there and is
    /// a registration.
    fn session_instance(&self, worker: &WorkerId) -> Result<Option<DaemonInstance>, StateError> {
        let key = keys::session(worker);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        Ok(match self.codec.read(&key, &row.value) {
            Ok(Value::Session(up)) => Some(up.instance),
            _ => None,
        })
    }

    fn remember_owner(&self, worker: &WorkerId, state: Ownership) {
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        owners.insert(worker.clone(), state);
    }

    fn ownership_of(&self, worker: &WorkerId) -> Option<Ownership> {
        let owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        owners.get(worker).copied()
    }

    /// The error of a write for `worker` that finds it owned by another frontend; the
    /// node's ownership is lost from now on.
    fn disowned(&self, worker: &WorkerId) -> StateError {
        self.remember_owner(worker, Ownership::Lost);
        StateError::StaleOwner(worker.clone())
    }

    /// `txn`, which writes a lease of `worker`, with the check that the `owner/` row is
    /// the one this frontend wrote, at the version read now: the transaction fails if the
    /// row changes before it commits. A node this frontend never claimed adds nothing.
    /// Commit the result with [`Rows::commit_owned`].
    ///
    /// # Errors
    /// [`StateError::StaleOwner`] if the row names another frontend or counter, or this
    /// frontend gave the node up before; the store cannot be read.
    pub fn guard(&self, txn: Txn, worker: &WorkerId) -> Result<Txn, StateError> {
        let counter = match self.ownership_of(worker) {
            None => return Ok(txn),
            Some(Ownership::Lost) => return Err(StateError::StaleOwner(worker.clone())),
            Some(Ownership::Held { counter, .. }) => counter,
        };
        let key = keys::owner(worker);
        let Some(row) = self.store.get(&key)? else {
            return Err(self.disowned(worker));
        };
        let mine = Owner {
            frontend: self.frontend,
            counter,
        };
        if Owner::from_row(&key, &row)? != mine {
            return Err(self.disowned(worker));
        }
        Ok(txn.check(key, Cond::Version(row.version)))
    }

    /// Commits `txn`, which [`Rows::guard`] checked the owner of `worker` in. A conflict
    /// on the owner row is the node changing hands between the read and the commit.
    ///
    /// # Errors
    /// [`StateError::StaleOwner`] if the node changed hands; the store cannot be written,
    /// or a row `txn` is conditional on changed ([`StateError::Conflict`]).
    pub fn commit_owned(&self, worker: &WorkerId, txn: Txn) -> Result<(), StateError> {
        match self.commit(txn) {
            Err(StateError::Conflict(conflict)) if conflict.key == keys::owner(worker) => {
                Err(self.disowned(worker))
            }
            done => done,
        }
    }

    /// Whether this frontend owns `worker` at `now`. The row is read when the frontend's
    /// last look is [`OWNER_CACHE_TTL`] old; a node it never claimed, or gave up, is not its
    /// own.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not an owner.
    pub fn owns(&self, worker: &WorkerId, now: FarmTime) -> Result<bool, StateError> {
        let Some(Ownership::Held { counter, checked }) = self.ownership_of(worker) else {
            return Ok(false);
        };
        if now < checked.saturating_add(OWNER_CACHE_TTL) {
            return Ok(true);
        }
        let key = keys::owner(worker);
        let mine = Owner {
            frontend: self.frontend,
            counter,
        };
        let holds = match self.store.get(&key)? {
            Some(row) => Owner::from_row(&key, &row)? == mine,
            None => false,
        };
        if holds {
            self.remember_owner(worker, Ownership::held(counter, now));
        } else {
            self.remember_owner(worker, Ownership::Lost);
        }
        Ok(holds)
    }
}
