//! The upload of one job's outputs, as a machine of steps the thread drives
//! (`docs/design/job-outputs.md`, section 3): each gathered file put whole at its key under the
//! job's prefix, in order, then the manifest. It reads time from its clock and asks for no
//! sleep itself: [`Upload::step`] says when it would go on, so a test drives it with a manual
//! clock and never waits.

use std::collections::VecDeque;
use std::io::Read as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use js_logstore::outputs::{Manifest, OutputFailure, OutputFile};
use js_logstore::{
    Clock, Credential, Endpoint, HttpRequest, HttpResponse, JobPrefix, Metadata, Store, StoreError,
    Transport, TransportError,
};
use sha2::{Digest as _, Sha256};

use super::gather::{Gathered, Gotten, READ, TOO_LARGE};
use crate::logs::HostGrant;

/// A file the store refused for good.
pub(crate) const STORE: &str = "store";

/// How long an upload may take from its start (15 minutes).
pub const OUTPUTS_BUDGET: Duration = Duration::from_secs(15 * 60);

/// What one upload is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The longest the upload may take ([`OUTPUTS_BUDGET`]).
    pub budget: Duration,
    /// The first wait after a failed request; doubled for each failure in a row.
    pub backoff_first: Duration,
    /// The longest wait after failed requests.
    pub backoff_max: Duration,
    /// The most bytes of one file the upload reads (one put's).
    pub max_put: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            budget: OUTPUTS_BUDGET,
            backoff_first: crate::logs::BACKOFF_FIRST,
            backoff_max: crate::logs::BACKOFF_MAX,
            max_put: u64::try_from(js_logstore::MAX_PUT_BYTES).unwrap_or(u64::MAX),
        }
    }
}

/// What the thread does after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// Calls [`Upload::step`] again at this time (or earlier, when woken).
    Sleep(SystemTime),
    /// The upload is over: the manifest was put, or it gave up.
    Done,
}

/// The transport of the store requests.
struct Port(Arc<dyn Transport>);

impl Transport for Port {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.0.send(request)
    }
}

/// One thing to put.
enum Item {
    /// An output file, and its bytes with their digest once read.
    File(Gotten, Option<(Vec<u8>, String)>),
    /// The manifest, last.
    Manifest,
}

/// The upload of one job's outputs.
pub(crate) struct Upload {
    prefix: JobPrefix,
    endpoint: Endpoint,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    grant: Arc<HostGrant>,
    limits: Limits,
    store: Option<Store<Port, Arc<dyn Clock>>>,
    items: VecDeque<Item>,
    manifest: Manifest,
    deadline: SystemTime,
    failures: u32,
    retry_at: Option<SystemTime>,
    /// The generation of the credential the store answered as expired: a newer one ends the
    /// wait at once.
    expired: Option<u64>,
}

impl Upload {
    /// The upload of `gathered` under `prefix` at `endpoint`, starting now on `clock`.
    pub(crate) fn new(
        (prefix, endpoint): (JobPrefix, Endpoint),
        gathered: Gathered,
        (transport, clock, grant): (Arc<dyn Transport>, Arc<dyn Clock>, Arc<HostGrant>),
        limits: Limits,
    ) -> Self {
        let now = clock.now();
        let deadline = now.checked_add(limits.budget).unwrap_or(now);
        let items = gathered
            .files
            .into_iter()
            .map(|file| Item::File(file, None))
            .chain([Item::Manifest])
            .collect();
        let manifest = Manifest {
            version: js_logstore::outputs::MANIFEST_VERSION,
            files: Vec::new(),
            missing: gathered.missing,
            failed: gathered.failed,
        };
        Self {
            prefix,
            endpoint,
            transport,
            clock,
            grant,
            limits,
            store: None,
            items,
            manifest,
            deadline,
            failures: 0,
            retry_at: None,
            expired: None,
        }
    }

    /// Puts what is due until it is all put, the upload has to wait (a store that is down, a
    /// credential that expired or none held) or its budget is spent.
    pub(crate) fn step(&mut self) -> Next {
        loop {
            let now = self.clock.now();
            if now >= self.deadline {
                self.log_given_up();
                return Next::Done;
            }
            if let Some(at) = self.retry_at {
                let newer = self
                    .expired
                    .is_some_and(|generation| self.grant.generation() != generation);
                if now < at && !newer {
                    return Next::Sleep(at.min(self.deadline));
                }
            }
            let Some((credential, generation)) = self.grant.current() else {
                let at = now.checked_add(self.limits.backoff_first).unwrap_or(now);
                return Next::Sleep(at.min(self.deadline));
            };
            let Some((key, body)) = self.next_put() else {
                return Next::Done;
            };
            match self.put(&key, body, credential) {
                Ok(()) => {
                    self.failures = 0;
                    self.retry_at = None;
                    self.expired = None;
                    if self.done_one() {
                        return Next::Done;
                    }
                }
                Err(StoreError::Unavailable(why)) => {
                    tracing::debug!(key, "job outputs: the store is unavailable: {why}");
                    self.back_off(now, None);
                }
                Err(StoreError::Expired { .. }) => {
                    self.grant.refused(generation);
                    self.back_off(now, Some(generation));
                }
                Err(why) => {
                    if self.refused(&why) {
                        return Next::Done;
                    }
                }
            }
        }
    }

    /// The key and the bytes of the next put, reading the next file's bytes once; a file that
    /// cannot be read is failed and the next taken. `None` when nothing is left.
    fn next_put(&mut self) -> Option<(String, Vec<u8>)> {
        loop {
            let max = self.limits.max_put;
            match self.items.front_mut()? {
                Item::Manifest => {
                    let key = self.prefix.outputs_manifest_key();
                    return Some((key, self.manifest.encode()));
                }
                Item::File(gotten, Some((body, _))) => {
                    return Some((self.prefix.output_key(&gotten.path), body.clone()));
                }
                Item::File(gotten, read @ None) => match bytes(gotten, max) {
                    Ok(body) => {
                        let sha256 = hex(&Sha256::digest(&body));
                        *read = Some((body, sha256));
                    }
                    Err(why) => {
                        let path = gotten.path.to_string();
                        self.manifest.failed.push(failure(&path, why));
                        self.items.pop_front();
                    }
                },
            }
        }
    }

    /// The thing at the front was put: a file joins the manifest's list. Whether that was the
    /// manifest, so the upload is over.
    fn done_one(&mut self) -> bool {
        match self.items.pop_front() {
            Some(Item::File(gotten, Some((body, sha256)))) => {
                let bytes = u64::try_from(body.len()).unwrap_or(u64::MAX);
                tracing::debug!(path = %gotten.path, bytes, "job outputs: put");
                self.manifest.files.push(OutputFile {
                    path: gotten.path.to_string(),
                    bytes,
                    sha256,
                });
                false
            }
            _ => {
                tracing::info!(
                    prefix = self.prefix.as_str(),
                    files = self.manifest.files.len(),
                    missing = self.manifest.missing.len(),
                    failed = self.manifest.failed.len(),
                    "job outputs: uploaded, and the manifest put"
                );
                true
            }
        }
    }

    /// The store refused the thing at the front for good (`why`): a file is failed and the next
    /// taken; the manifest ends the upload with no manifest. Whether the upload is over.
    fn refused(&mut self, why: &StoreError) -> bool {
        self.failures = 0;
        self.retry_at = None;
        match self.items.pop_front() {
            Some(Item::File(gotten, _)) => {
                tracing::warn!(path = %gotten.path, "job outputs: the store refused a file: {why}");
                let path = gotten.path.to_string();
                self.manifest.failed.push(failure(&path, STORE));
                false
            }
            _ => {
                tracing::warn!(
                    prefix = self.prefix.as_str(),
                    "job outputs: the store refused the manifest: {why}; the outputs have none"
                );
                true
            }
        }
    }

    /// Holds the next request until a backoff after `now` has passed (or, after an expiry, a
    /// newer credential than `expired` has come).
    fn back_off(&mut self, now: SystemTime, expired: Option<u64>) {
        let doubled = self
            .limits
            .backoff_first
            .saturating_mul(1u32 << self.failures.min(16));
        let wait = doubled.min(self.limits.backoff_max);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now.checked_add(wait).unwrap_or(now));
        self.expired = expired;
    }

    /// Puts `body` at `key`, signed with `credential`.
    fn put(&mut self, key: &str, body: Vec<u8>, credential: Credential) -> Result<(), StoreError> {
        let (transport, clock, endpoint) = (&self.transport, &self.clock, &self.endpoint);
        let store = self.store.get_or_insert_with(|| {
            Store::new(
                Port(Arc::clone(transport)),
                Arc::clone(clock),
                endpoint.clone(),
                credential.clone(),
            )
        });
        store.set_credential(credential);
        store.put(key, body, &Metadata::new())
    }

    /// Logs that the budget was spent before the manifest was put.
    fn log_given_up(&self) {
        tracing::warn!(
            prefix = self.prefix.as_str(),
            left = self.items.len(),
            "job outputs: the upload's time is spent; the outputs have no manifest"
        );
    }
}

/// The bytes of `gotten`, at most `max`: a file read short or over `max` is refused.
fn bytes(gotten: &mut Gotten, max: u64) -> Result<Vec<u8>, &'static str> {
    let mut body = Vec::new();
    let read = (&gotten.file)
        .take(max.saturating_add(1))
        .read_to_end(&mut body);
    match read {
        Err(_) => Err(READ),
        Ok(_) if u64::try_from(body.len()).unwrap_or(u64::MAX) > max => Err(TOO_LARGE),
        Ok(_) => Ok(body),
    }
}

/// `digest` in lower-case hex.
fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A failure of `path` for `reason`.
fn failure(path: &str, reason: &str) -> OutputFailure {
    OutputFailure {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}
