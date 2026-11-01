//! Whole-node reservations across a restart. The registry owns the marks, the wait list
//! and the give-up counts; the lease file keeps them beside the leases, as the table's
//! reservation ledger (`js-lease`), so a restart over the same file with `--boot resume`
//! gets them back (design `whole-node.md`, D55).
//!
//! - Each tick hands the table the registry's ledger when it changed since the last
//!   hand-over ([`Handed`]), and the table writes it in the same atomic replace as the
//!   leases. The file is therefore at most one tick behind the registry; the final tick of
//!   a shutdown brings it level.
//! - The table is opened with a restart grace ([`with_restart_grace`]): a restored
//!   deadline is at least the boot plus [`RESERVATION_BOOT_GRACE`].
//! - Once the table is open, the registry is given the ledger it read ([`restore`]), before
//!   any host says hello. The registry drops and counts what cannot stand.
//! - Nothing about estimates is kept: the registry's drain choice reads them again.
//!
//! The file format is additive: a build from before the ledger, or from before the wait
//! place, reads past the rows and keys it does not know (see the `js-lease` crate
//! documentation).

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use js_lease::{ReservationRecord, Settings};
use js_registry::{Registry, WholeNodeConfig};
use tracing::info;

use crate::api::Table;

/// The least time a restored reservation has to drain after a boot: a deadline that
/// passed while the server was down, or would pass before the agents have said hello
/// again, is raised to the boot plus this. It is not added to the stored deadline, so a
/// restart loop does not stretch a reservation.
pub const RESERVATION_BOOT_GRACE: Duration = Duration::from_secs(120);

/// `settings` with the restart grace of reservation records: the floor of a restored
/// deadline is [`RESERVATION_BOOT_GRACE`], and a record may be claimed for the pool's
/// `regain_grace` (the default of a dedicated pool for a pool without whole-node
/// settings).
pub(crate) fn with_restart_grace(settings: Settings, whole: Option<&WholeNodeConfig>) -> Settings {
    let regain = whole.map_or(WholeNodeConfig::dedicated(1).regain_grace_ms, |whole| {
        whole.regain_grace_ms
    });
    settings.with_restart_grace(RESERVATION_BOOT_GRACE, Duration::from_millis(regain))
}

/// Gives the registry the ledger `table` opened with, and logs what came of it.
pub(crate) fn restore(registry: &Registry, table: &Table) {
    let records = table.reservations();
    let restored = registry.restore_reservations(&records);
    if !records.is_empty() {
        info!(
            resumed = restored.resumed,
            dropped = restored.dropped,
            "reservations restored from the lease file"
        );
    }
}

/// The ledger the last tick handed the table, so a tick that changes nothing writes
/// nothing for it.
pub(crate) struct Handed(Mutex<Option<Vec<ReservationRecord>>>);

impl Handed {
    /// For a server that has just opened `table`. A table with no records has nothing to
    /// write until the registry has some; one with records is written at its first tick,
    /// which holds the raised deadlines and whatever the registry dropped.
    pub(crate) fn opened_with(table: &Table) -> Self {
        let ledger = table.reservations().is_empty().then(Vec::new);
        Self(Mutex::new(ledger))
    }

    /// Hands `table` the registry's ledger if it is not the one handed last (the first
    /// call always hands it), for the tick that follows to write. The registry's lock is
    /// taken under the table's, as the lock order says.
    pub(crate) fn hand_over(&self, registry: &Registry, table: &mut Table) {
        let ledger = registry.reservation_ledger();
        let mut last = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_ref() != Some(&ledger) {
            table.set_reservations(ledger.clone());
            *last = Some(ledger);
        }
    }
}

#[cfg(test)]
mod tests {
    use js_registry::DEFAULT_REGAIN_GRACE_MS;

    use super::*;

    /// Asserts how the pool's settings reach the table's restart grace: the boot grace is
    /// the constant whatever the pool says, the regain grace is the pool's when it has
    /// whole-node settings and the default when it has none, and the ttls are untouched.
    /// It catches a grace that never reaches the table (a restored deadline would then be
    /// the stored one) and a pool key that is dropped on the way.
    #[test]
    fn the_restart_grace_is_the_constant_and_the_pools_regain_grace() {
        let secs = Duration::from_secs;
        let flags = Settings::new(secs(60), secs(3600)).unwrap();
        let mut whole = WholeNodeConfig::dedicated(2);
        whole.regain_grace_ms = 45_000;
        let set = with_restart_grace(flags, Some(&whole));
        assert_eq!(set.boot_grace(), RESERVATION_BOOT_GRACE);
        assert_eq!(set.regain_grace(), secs(45));
        assert_eq!((set.default_ttl(), set.max_ttl()), (secs(60), secs(3600)));
        let plain = with_restart_grace(flags, None);
        assert_eq!(plain.boot_grace(), RESERVATION_BOOT_GRACE);
        let default = Duration::from_millis(DEFAULT_REGAIN_GRACE_MS);
        assert_eq!(plain.regain_grace(), default);
    }
}
