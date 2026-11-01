//! The bundle flags, what the agent reports about its store, and the checks of a start that
//! names a bundle. The machine is a fake host in memory (`fake`), so every rule is driven
//! without a Mac, a registry or a store on disk; `system` runs the machine's own host over
//! real directories.

mod admit;
pub mod fake;
pub mod fixture;
mod hello;
mod lifecycle;
mod place;
mod registry;
mod settings;
mod start;
mod system;
