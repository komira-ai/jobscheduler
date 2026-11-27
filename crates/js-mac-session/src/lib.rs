//! `js-mac-session`: the one way `js-agent` and `js-mac-provision` start a program, and the
//! judgement of whether a process holds Full Disk Access. The root helper program this crate
//! once held was retired; the crate is a library alone.
//!
//! - [`disclaim`]: starting a program. On macOS its privacy (TCC) responsibility is
//!   disclaimed by default, so it does not hold the starter's grants; only a call of
//!   `Program::keep_responsibility` keeps it. Off macOS it is the standard library's
//!   `Command` with the steps and the single use the module adds.
//! - [`full_disk_access`]: whether a process holds Full Disk Access, which `js-agent`
//!   judges of itself at start.
//!
//! Both modules build and are tested on every platform; the calls that disclaim the
//! responsibility are macOS's alone.

pub mod disclaim;
pub mod full_disk_access;

#[cfg(test)]
mod testing;
