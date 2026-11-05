//! Where the hosts a server leases come from.

use js_lease::Booking;
use js_provider::Pool;

use crate::error::StartupError;

/// A provider built for a start, and the bookings of the lease file it could not honour.
pub struct Seated {
    /// The provider the lease table runs on.
    pub provider: Box<dyn Pool + Send>,
    /// The bookings the source was given and could not put back, with why. The table
    /// then finds the provider ignorant of each lease and drops it; the server logs them.
    pub unseated: Vec<Unseated>,
}

impl Seated {
    /// A provider that needed no bookings put back, or took them all.
    pub fn all(provider: Box<dyn Pool + Send>) -> Self {
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
/// drops the ones it does not know. The server builds one source itself: the hosts that
/// run `js-agent` and register, layered behind one provider with the source a library
/// caller handed in, if any. A library caller hands one in only through the simulation
/// seam ([`Config::simulated`](crate::Config::simulated),
/// [`Config::with_capacity_source`](crate::Config::with_capacity_source), or a kind of
/// its own that builds [`Built::Simulated`](crate::Built::Simulated)): tests and
/// simulations of the scheduler without real hosts. No flag and no kind the server ships
/// makes one.
///
/// # Blocking
///
/// The server calls the provider from the request handlers and the tick loop on a
/// runtime worker thread, one call at a time, with the table lock held: every other
/// request and the tick wait for a call to return, and a few slow calls stall the
/// server. A provider must therefore return promptly, and one that talks to a network
/// must apply its own timeout and report the call as unanswered
/// (`Unknown`, as [`js_provider::StartAck`] and its siblings spell it) when it expires.
pub trait CapacitySource {
    /// The provider with `bookings` put back as far as it can, or the flag that made it
    /// impossible to build.
    fn provider(&self, bookings: &[Booking]) -> Result<Seated, StartupError>;
}
