//! The booked rule as a setting, for the tests whose subject is not placement by load.
//!
//! Those tests (a lost run's budget, a drain's order, the platform match, the spread of
//! booked cores, the reference-model simulations) lay their leases out by the booked
//! rule's spread: the least booked worker, then the fewest leases, then the first by
//! name. They name the rule they were written for through this setting, the rollback
//! switch, instead of inheriting whatever the default placement is. Placement by load is
//! tested by the `admission_*`, `load_*` and score suites.

use js_sched::{LoadConfig, PlacementLoad};

/// The settings that place by the booked rule.
pub fn booked() -> LoadConfig {
    LoadConfig {
        mode: PlacementLoad::Off,
        ..LoadConfig::default()
    }
}
