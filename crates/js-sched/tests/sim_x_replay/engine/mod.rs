//! The replay engine behind the `sim_x_replay` test: it replays recorded per-action
//! records against the placement scorer. Other test or example targets may include this
//! file with `#[path]`.
//!
//! Modules, in dependency order. Each module has a file of its own and a `mod` line
//! here; a module imports only the ones above it in this list.
//!
//! | module | holds | state |
//! |---|---|---|
//! | `fixture` | the trace row, the key and outcome types every other module shares, the loader and the encoder | present |
//! | `tables` | integer quantile tables | present |
//! | `truth` | what really happens to a run, from a row and keyed draws | present |
//! | `predictor` | what the scheduler believes about a key | planned |
//! | `permute` | the control's permutation of belief minutes | planned |
//! | `model`, `predicate`, `metrics` | the pool, the admission stand-in and the integer metrics | planned |
//! | `rankers`, `report`, `control` | the compared placement rules, the report and the control | planned |
//!
//! A module takes its input as `fixture::Row` values (or a `fixture::Fixture`), never as
//! bytes, so a hand-made trace in a test and the committed one go through the same code.

// Two compile units (the test and the example) each use a subset of what the engine holds,
// and modules arrive in separate pull requests ahead of their callers.
#![allow(dead_code, reason = "each compile unit uses a subset of the engine")]

pub mod fixture;
pub mod tables;
pub mod truth;
