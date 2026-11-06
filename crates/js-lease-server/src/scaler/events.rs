//! The provider's notices ([`CapacityProvider::events`](js_elastic::CapacityProvider::events)):
//! a reclaim is acted on when it is sent, not at the next list.
//!
//! The first tick opens the provider's stream ([`Effects::events`]). A provider that sends
//! none (`None`) is never asked again and no reader is started. For one that does, a thread
//! of its own reads the stream, because `next` blocks. It holds only a weak handle on the
//! scaler, so a scaler that is dropped ends the reader at its next notice. For each
//! [`ProviderEvent::Reclaimed`] it:
//!
//! - maps the provider's id to a host by the records' `cloud_id`, the mapping the step uses
//!   for a list. An id that maps to no record is logged and counted
//!   ([`Scaler::reclaims_ignored`](super::Scaler::reclaims_ignored)). The records are those
//!   of the last tick that finished, so a machine whose provider id no tick has recorded yet
//!   (one launched in the tick still running) is among these; the list still shows its
//!   reclaim. An id whose record is no longer held (a release already under way), or that is
//!   already pending, is a repeat and changes nothing; one that comes while a tick that took
//!   the id has not finished yet renews the cordon and runs a tick that finds nothing to do;
//! - cordons the host ([`Call::Cordon`]) so that it takes no new work from that moment, if
//!   the host has said hello (a booting machine has no host to cordon);
//! - adds the id to the step's [`Input::reclaimed`](js_scaler::Input::reclaimed) and runs a
//!   tick at once. The step then takes the route it takes for a machine a list shows
//!   preempted: the release, the brake's strike, the removal of the host and with it the
//!   end of the work on it. The id is kept until a tick that carried it has written its
//!   records.
//!
//! A stream that ends, or a reader whose tick panicked, is logged and the stream opened
//! again at the next tick: the tick is the retry's pace, and until then the list is what
//! shows a reclaim, as it does for a provider that sends no notices.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, Weak};

use js_elastic::{EventStream, HostRef, ProviderEvent};
use js_scaler::Phase;
use tracing::{debug, warn};

use super::{Call, Core};

/// Where the scaler is with the provider's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Listen {
    /// Not open: the next tick asks for it.
    Closed,
    /// A reader holds it open.
    Open,
    /// The provider sends no notices; it is not asked again.
    Unsupported,
}

/// What a reclaim notice names, as the records tell.
enum Named {
    /// A machine held now, by host name, and whether that host has said hello.
    Held { host: String, joined: bool },
    /// A machine of the records that is not held (its release is under way or done).
    Ending,
    /// No machine of the records.
    Unknown,
}

impl Core {
    /// Opens the provider's stream if it is not open and the provider has one, and starts
    /// its reader. Called before every tick the server's ticker runs.
    pub(super) fn listen(self: &Arc<Self>) {
        let mut listen = self.listen.lock().unwrap_or_else(PoisonError::into_inner);
        if *listen != Listen::Closed {
            return;
        }
        let Some(stream) = self.effects.events() else {
            debug!("the provider sends no notices; its list shows what it reclaims");
            *listen = Listen::Unsupported;
            return;
        };
        let core = Arc::downgrade(self);
        let started = std::thread::Builder::new()
            .name("scaler-notices".to_owned())
            .spawn(move || read(&core, stream));
        // A thread that cannot be started (the system is out of threads) leaves the stream
        // closed, and the next tick asks again; until then the list shows what it reclaims.
        *listen = [Listen::Closed, Listen::Open][usize::from(started.is_ok())];
    }

    /// Whether a reader holds the provider's stream open.
    pub(super) fn notices_open(&self) -> bool {
        *self.listen.lock().unwrap_or_else(PoisonError::into_inner) == Listen::Open
    }

    /// Acts on one notice, as the module says.
    fn notice(&self, notice: ProviderEvent) {
        let ProviderEvent::Reclaimed(id) = notice;
        let (host, joined) = match self.named(&id) {
            Named::Held { host, joined } => (host, joined),
            Named::Ending => {
                debug!(id = %id.as_str(), "a reclaim notice for a machine already being released");
                return;
            }
            Named::Unknown => {
                warn!(id = %id.as_str(), "a reclaim notice names no machine the records know; it is ignored, and the list shows the reclaim if the machine is ours");
                self.reclaims_ignored.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let fresh = self
            .reclaimed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.0.clone());
        if !fresh {
            debug!(%host, "a repeated reclaim notice");
            return;
        }
        warn!(%host, id = %id.as_str(), "the provider reclaimed the machine; its host is cordoned and it is released now");
        if joined {
            self.effects.call(Call::Cordon, &host);
        }
        self.tick();
    }

    /// The record of this pool whose provider id is `id`, as of the last tick.
    fn named(&self, id: &HostRef) -> Named {
        let memory = self.memory();
        let found = memory.state.records.instances.iter().find(|record| {
            record.pool == self.settings.pool && record.cloud_id.as_deref() == Some(id.as_str())
        });
        match found.map(|record| (record.phase, record.name.clone())) {
            Some((Phase::Registered | Phase::Draining, host)) => Named::Held { host, joined: true },
            Some((Phase::Booting, host)) => Named::Held {
                host,
                joined: false,
            },
            Some(_) => Named::Ending,
            None => Named::Unknown,
        }
    }
}

/// Closes the scaler's stream when its reader stops, however it stops: the stream ended,
/// or a call in the reader's tick panicked. The next tick then opens it again.
struct Closing<'a>(&'a Weak<Core>);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        let Some(core) = self.0.upgrade() else {
            return;
        };
        let reason = ["ended", "stopped on a panic"][usize::from(std::thread::panicking())];
        warn!(
            reason,
            "the reader of the provider's notices stopped; the stream is opened again at the next tick, and until then the list shows what it reclaims"
        );
        *core.listen.lock().unwrap_or_else(PoisonError::into_inner) = Listen::Closed;
    }
}

/// Reads `stream` until it ends or the scaler is gone. The stream (a parameter) is dropped
/// after the guard (a local), so that whoever sees the drop sees the scaler closed.
fn read(core: &Weak<Core>, mut stream: EventStream) {
    let _closing = Closing(core);
    for notice in stream.by_ref() {
        let Some(core) = core.upgrade() else {
            return;
        };
        core.notice(notice);
    }
}
