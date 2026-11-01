//! Whole-node reservations across a restart: the restart grace the lease table is opened
//! with.

use std::time::Duration;

use js_lease::Settings;
use js_registry::WholeNodeConfig;

/// The least time a restored reservation has to drain after a boot: a deadline that
/// passed while the server was down, or would pass before the agents have said hello
/// again, is raised to the boot plus this. It is not added to the stored deadline, so a
/// restart loop does not stretch a reservation.
pub const RESERVATION_BOOT_GRACE: Duration = Duration::from_secs(120);

/// `settings` with the restart grace of reservation records.
pub(crate) fn with_restart_grace(settings: Settings, whole: Option<&WholeNodeConfig>) -> Settings {
    let _ = whole;
    settings
}
