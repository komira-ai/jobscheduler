//! Sims x and x2: a replay of a day of per-action records against the placement scorer.
//!
//! One `#[test]` per world and assertion family, each in a module of its own so a pull
//! request that adds a family adds a `mod` line here and a file beside this one.
//!
//! * `engine` - the replay engine (`engine/mod.rs` lists its modules).
//! * `fixture_tests` - the committed trace and the loader that reads it.

mod engine;
mod fixture_tests;
mod predictor_tests;
