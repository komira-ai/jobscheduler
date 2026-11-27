//! A replay of a day of recorded per-action records against the placement scorer.
//!
//! One `#[test]` per world and assertion family, each in a module of its own so a pull
//! request that adds a family adds a `mod` line here and a file beside this one.
//!
//! * `engine` - the replay engine (`engine/mod.rs` lists its modules).
//! * `fixture_tests` - the committed trace and the loader that reads it.
//! * `adapter_tests` - the scorer adapter and ranker 5.
//! * `export_tests` - the x2 export reader.
//! * `rankers_tests` - rankers 3, 4, 5 and FW5, decided-key shares, the guard counters and
//!   candidate-order invariance.
//! * `x2_tests` - the x2 run: the real series as the truth, the learner's beliefs in the
//!   replay, the score rankers in the loop, the report and the runner's flags.
//! * `x2_store_tests` - the text the usage store's export writes reads back as the x2 input.

mod adapter_tests;
mod engine;
mod engine_tests;
mod export_tests;
mod fixture_tests;
#[path = "../load_rig/mod.rs"]
mod load_rig;
mod permute_tests;
mod predictor_tests;
mod rankers_tests;
mod truth_tests;
mod x2_store_tests;
mod x2_tests;
