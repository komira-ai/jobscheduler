//! A replay of a day of recorded per-action records against the placement scorer.
//!
//! One `#[test]` per world and assertion family, each in a module of its own so a pull
//! request that adds a family adds a `mod` line here and a file beside this one.
//!
//! * `engine` - the replay engine (`engine/mod.rs` lists its modules).
//! * `fixture_tests` - the committed trace and the loader that reads it.
//! * `adapter_tests` - the scorer adapter and ranker 5.
//! * `rankers_tests` - rankers 3, 4, 5 and FW5, decided-key shares, the guard counters and
//!   candidate-order invariance.

mod adapter_tests;
mod engine;
mod engine_tests;
mod fixture_tests;
#[path = "../load_rig/mod.rs"]
mod load_rig;
mod predictor_tests;
mod rankers_tests;
mod truth_tests;
