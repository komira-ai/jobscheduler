//! Where the hosts a server leases come from.

use std::path::PathBuf;

use js_lease::Booking;
use js_pool_static::StaticPool;
use js_provider::Provider;
use js_types::{ProviderId, ResourceClass};

use crate::error::StartupError;

/// A provider built for a start, and the bookings of the lease file it could not honour.
pub struct Seated {
    /// The provider the lease table runs on.
    pub provider: Box<dyn Provider + Send>,
    /// The bookings the source was given and could not put back, with why. The table
    /// then finds the provider ignorant of each lease and drops it; the server logs them.
    pub unseated: Vec<Unseated>,
}

impl Seated {
    /// A provider that needed no bookings put back, or took them all.
    pub fn all(provider: Box<dyn Provider + Send>) -> Self {
        Self {
            provider,
            unseated: Vec::new(),
        }
    }
}

/// A booking of the lease file that a source could not put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unseated {
    /// The booking, as the lease file records it.
    pub booking: Booking,
    /// Why the source refused it.
    pub reason: String,
}

/// Builds the provider a server's lease table runs on.
///
/// The server asks its source once, at start, and hands it the bookings the lease file
/// records (see [`js_lease::read_bookings`]; none on a first boot). A source whose
/// provider remembers nothing across a process restart must put them back before it
/// returns, because the table is about to ask the provider about each held lease and
/// drops the ones it does not know. Today the only source is the operator's static pool
/// ([`StaticPoolSource`]): the hosts are listed in a file, the bookings are in memory
/// only, and the lease file is their record. Hosts that agents register themselves are
/// a future source of the same trait: its provider answers from the agents that report
/// in, so it ignores the bookings it is given. The server needs no change for it, and it
/// is not built here.
///
/// # Blocking
///
/// The server calls the provider from the request handlers and the tick loop on a
/// runtime worker thread, one call at a time, with the table lock held: every other
/// request and the tick wait for a call to return, and a few slow calls stall the
/// server. A provider must therefore return promptly, and one that talks to a network
/// must apply its own timeout and report the call as unanswered
/// (`Unknown`, as [`js_provider::StartAck`] and its siblings spell it) when it expires. The static
/// pool never leaves memory.
pub trait CapacitySource {
    /// The provider with `bookings` put back as far as it can, or the flag that made it
    /// impossible to build.
    fn provider(&self, bookings: &[Booking]) -> Result<Seated, StartupError>;
}

/// The hosts an operator lists in a TOML file (`--pool-config`), as
/// [`js_pool_static::StaticPool`] reads them.
///
/// The lease file is the source of truth for this pool's bookings. At start every
/// booking of the file is seated in the fresh pool, in lease id order: a booking whose
/// host the file no longer lists, or that an earlier lease of the file already holds,
/// is not seated and comes back as [`Unseated`].
#[derive(Debug, Clone)]
pub struct StaticPoolSource {
    path: PathBuf,
    classes: Option<Vec<ResourceClass>>,
}

impl StaticPoolSource {
    /// A source over the pool configuration file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            classes: None,
        }
    }

    /// The same source serving `classes` instead of the class the pool serves by default.
    #[must_use]
    pub fn with_classes(mut self, classes: Vec<ResourceClass>) -> Self {
        self.classes = Some(classes);
        self
    }
}

impl CapacitySource for StaticPoolSource {
    fn provider(&self, bookings: &[Booking]) -> Result<Seated, StartupError> {
        let id = ProviderId::new("static-pool").map_err(StartupError::because("--pool-config"))?;
        let pool = StaticPool::from_config_file(id, &self.path)
            .map_err(StartupError::because("--pool-config"))?;
        let mut pool = match &self.classes {
            Some(classes) => pool.with_classes(classes.clone()),
            None => pool,
        };
        let unseated = bookings
            .iter()
            .filter_map(|booking| {
                let refused = pool.seat(&booking.host, booking.lease).err()?;
                Some(Unseated {
                    booking: booking.clone(),
                    reason: refused.to_string(),
                })
            })
            .collect();
        Ok(Seated {
            provider: Box::new(pool),
            unseated,
        })
    }
}
