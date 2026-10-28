//! The booked rule as a setting, and the way a suite runs under both rules.
//!
//! The default placement is by load. A test whose subject is not placement should hold
//! under either rule: it takes its scheduler's settings from [`current`], and a suite
//! declares it with [`both_rules!`], which runs the body once under the booked rule and
//! once under the default load settings. Its assertions name the behaviour, never which
//! worker placement picked (a test follows the worker a start names).
//!
//! A test that asserts the booked layout itself (the least booked worker, then the fewest
//! leases, then the first by name: the spread, a lost run's budget on a named node, the
//! reference-model simulations) stays on the booked rule through [`booked`], the rollback
//! switch. Placement by load is tested by the `admission_*`, `load_*` and score suites.

#![allow(
    dead_code,
    unused_macros,
    reason = "each test binary uses some of this"
)]

use std::cell::Cell;

use js_sched::{LoadConfig, PlacementLoad};

/// The settings that place by the booked rule.
pub fn booked() -> LoadConfig {
    LoadConfig {
        mode: PlacementLoad::Off,
        ..LoadConfig::default()
    }
}

thread_local! {
    static CURRENT: Cell<LoadConfig> = Cell::new(LoadConfig::default());
}

/// The settings of the rule the running test is run under: the default load settings
/// unless [`under`] says otherwise. Each test runs on its own thread, so one test's rule
/// never reaches another.
pub fn current() -> LoadConfig {
    CURRENT.with(Cell::get)
}

/// Whether the running test is under the booked rule: a test that keeps an exact
/// layout assertion under the booked rule branches on it.
pub fn is_booked() -> bool {
    current().mode == PlacementLoad::Off
}

/// Runs `body` with [`current`] giving `config`.
pub fn under(config: LoadConfig, body: fn()) {
    CURRENT.with(|c| c.set(config));
    body();
}

/// Declares tests that hold under both rules: for each `fn name() { .. }` a module `name`
/// with the test `booked_rule` (the booked rule) and the test `load_rule` (the default).
macro_rules! both_rules {
    ($($(#[$meta:meta])* fn $name:ident() $body:block)*) => {$(
        $(#[$meta])*
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            fn run() $body

            // The canary: each variant checks the rule it runs under before the body.
            fn as_booked() {
                assert_eq!(booked::current().mode, js_sched::PlacementLoad::Off);
                run();
            }

            fn as_load() {
                assert_eq!(booked::current().mode, js_sched::PlacementLoad::On);
                run();
            }

            #[test]
            fn booked_rule() {
                booked::under(booked::booked(), as_booked);
            }

            #[test]
            fn load_rule() {
                booked::under(js_sched::LoadConfig::default(), as_load);
            }
        }
    )*};
}
