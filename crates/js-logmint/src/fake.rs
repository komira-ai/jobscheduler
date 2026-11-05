//! A minter for tests (feature `fake`): it issues credentials from js-logstore's
//! [`FakeStore`] with exactly the session policy [`policy`] builds, parsed back by the fake
//! ([`Policy::from_session_policy`]), so a test of a credential's scope goes through the same
//! text the real call sends.
//!
//! It counts the mints per host and records each request, and a test scripts it:
//! [`FakeMinter::fail_next`] answers the next mints with an error, [`FakeMinter::block`] holds
//! every later mint until [`FakeMinter::release`] (never for a time; [`FakeMinter::wait_for_held`]
//! says when a mint is held), and [`FakeMinter::expire_all`] makes every credential it minted
//! expire in the store.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use js_logstore::Credential;
use js_logstore::fake::{FakeStore, Policy};

use crate::policy::policy;
use crate::{MintError, MintRequest, Minter, Refusal};

/// The fake minter.
#[derive(Debug)]
pub struct FakeMinter {
    store: FakeStore,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    calls: BTreeMap<String, u64>,
    requests: Vec<MintRequest>,
    failures: VecDeque<MintError>,
    blocked: bool,
    held: usize,
    minted: Vec<String>,
}

impl FakeMinter {
    /// A minter issuing from `store`.
    pub fn new(store: FakeStore) -> Self {
        Self {
            store,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }

    /// The store the credentials are issued from.
    pub fn store(&self) -> &FakeStore {
        &self.store
    }

    /// Answers the next `n` mints with `error`; they issue nothing.
    pub fn fail_next(&self, n: usize, error: MintError) {
        self.lock().failures.extend(std::iter::repeat_n(error, n));
    }

    /// Holds every later mint, after it is counted and recorded, until [`FakeMinter::release`].
    pub fn block(&self) {
        self.lock().blocked = true;
    }

    /// Lets the held mints, and every later one, go on.
    pub fn release(&self) {
        self.lock().blocked = false;
        self.changed.notify_all();
    }

    /// Waits until at least `n` mints are held by [`FakeMinter::block`].
    pub fn wait_for_held(&self, n: usize) {
        let state = self.lock();
        let _held = self
            .changed
            .wait_while(state, |state| state.held < n)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Makes every credential this minter issued expire now, as the store sees it.
    pub fn expire_all(&self) {
        let minted = self.lock().minted.clone();
        for access_key_id in &minted {
            self.store.expire(access_key_id);
        }
    }

    /// How many mints were asked for `host`, failed and held ones included.
    pub fn calls(&self, host: &str) -> u64 {
        self.lock().calls.get(host).copied().unwrap_or_default()
    }

    /// Every request asked, in order.
    pub fn requests(&self) -> Vec<MintRequest> {
        self.lock().requests.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Minter for FakeMinter {
    fn mint(&self, request: &MintRequest) -> Result<Credential, MintError> {
        let mut state = self.lock();
        state.requests.push(request.clone());
        *state.calls.entry(request.host.clone()).or_default() += 1;
        state.held += 1;
        self.changed.notify_all();
        let mut state = self
            .changed
            .wait_while(state, |state| state.blocked)
            .unwrap_or_else(PoisonError::into_inner);
        state.held -= 1;
        if let Some(error) = state.failures.pop_front() {
            return Err(error);
        }
        let text = policy(&request.bucket, &request.prefix);
        let unreadable = MintError::Refused(Refusal::Answer("the fake cannot read the policy"));
        let scope =
            Policy::from_session_policy(&text, request.bucket.as_str()).ok_or(unreadable)?;
        let credential = self.store.issue_for(scope, request.ttl);
        state.minted.push(credential.access_key_id.clone());
        Ok(credential)
    }
}
