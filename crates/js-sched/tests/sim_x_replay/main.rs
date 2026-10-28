//! Sims x and x2: a replay of a day of per-action records against the placement scorer.
//!
//! One `#[test]` per world and assertion family, each in a module of its own so a pull
//! request that adds a family adds a `mod` line here and a file beside this one.
//!
//! * `engine` - the replay engine (`engine/mod.rs` lists its modules).
//! * `fixture_tests` - the committed trace and the loader that reads it.
//! * `adapter_tests` - the scorer adapter and ranker 5.

mod adapter_tests;
mod engine;
mod fixture_tests;
#[path = "../load_rig/mod.rs"]
mod load_rig;
