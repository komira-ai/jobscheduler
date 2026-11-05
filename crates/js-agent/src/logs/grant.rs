//! The host's upload credential: one for every uploader of the host, held in memory only.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::{LogGrant, LogGrantState};
use js_logstore::{Clock, Credential};

/// How long before its expiry a held credential is reported stale, so that the server renews
/// it before an uploader is refused (2 minutes).
pub const GRANT_STALE_MARGIN: Duration = Duration::from_secs(120);

/// The cell that holds the host's credential ([`LogGrant`], from the heartbeat replies) for
/// every uploader of the host. A reply's grant replaces the one held; each uploader reads the
/// cell before each request, so after a renewal every uploader signs its next request with the
/// new credential (a request in flight ends with the one it started with).
///
/// Every credential the cell takes is given a generation, one above the last: an uploader
/// refused for an expired credential tells the cell which generation it used, and after that
/// waits for a newer one.
///
/// The credential lives in memory only: it is never written to a file, and the secret access
/// key and the session token appear in no `Debug` output, no error and no log line.
pub struct HostGrant {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    held: Option<Held>,
    generation: u64,
    refused: bool,
}

struct Held {
    grant: LogGrant,
    credential: Credential,
}

impl fmt::Debug for HostGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("HostGrant")
            .field("generation", &inner.generation)
            .field(
                "until_ms",
                &inner.held.as_ref().map(|h| h.grant.expires_at_ms),
            )
            .field("refused", &inner.refused)
            .finish_non_exhaustive()
    }
}

impl HostGrant {
    /// A cell that holds no credential, reading the time from `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes `grant` as the host's credential. The very grant the cell holds (the server
    /// resends it until a heartbeat reports it held) changes nothing, so a refusal stays
    /// recorded; any other replaces it, under a new generation, and clears the refusal.
    pub fn replace(&self, grant: &LogGrant) {
        let mut inner = self.lock();
        if inner.held.as_ref().is_some_and(|held| held.grant == *grant) {
            return;
        }
        let expires_at = UNIX_EPOCH
            .checked_add(Duration::from_millis(grant.expires_at_ms))
            .unwrap_or(UNIX_EPOCH);
        let credential = Credential {
            access_key_id: grant.access_key_id.clone(),
            secret_access_key: grant.secret_access_key.clone(),
            session_token: grant.session_token.clone(),
            expires_at,
        };
        inner.held = Some(Held {
            grant: grant.clone(),
            credential,
        });
        inner.generation += 1;
        inner.refused = false;
    }

    /// The credential held, with its generation; `None` while the host holds none.
    #[must_use]
    pub fn current(&self) -> Option<(Credential, u64)> {
        let inner = self.lock();
        let generation = inner.generation;
        inner
            .held
            .as_ref()
            .map(|held| (held.credential.clone(), generation))
    }

    /// The generation of the credential held (0 before the first).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Records that the store answered the credential of `generation` as expired. A refusal of
    /// an older generation than the one held changes nothing.
    pub fn refused(&self, generation: u64) {
        let mut inner = self.lock();
        if inner.generation == generation {
            inner.refused = true;
        }
    }

    /// What the agent reports in its heartbeat: `until_ms` is the held credential's
    /// `expires_at_ms`; `stale` is true while it holds none, after the store answered the held
    /// one as expired, and from [`GRANT_STALE_MARGIN`] before its expiry on, until a newer one
    /// arrives.
    #[must_use]
    pub fn state(&self) -> LogGrantState {
        let now = self.clock.now();
        let inner = self.lock();
        let until_ms = inner.held.as_ref().map(|held| held.grant.expires_at_ms);
        let near = inner.held.as_ref().is_none_or(|held| {
            now.checked_add(GRANT_STALE_MARGIN)
                .is_none_or(|soon| soon >= held.credential.expires_at)
        });
        LogGrantState {
            stale: inner.refused || near,
            until_ms,
        }
    }
}

/// Milliseconds since the Unix epoch at `at` (0 before it).
pub(super) fn epoch_ms(at: SystemTime) -> u64 {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}
