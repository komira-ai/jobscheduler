//! Host support for the tree store on macOS: the sealer that makes a tree immutable with
//! `uchg`, and the checks the store makes before it starts (the volume it lives on and the
//! kernel's securelevel).
//!
//! The logic is plain Rust over values that are read from the system through a function the
//! caller passes, so it is tested on every platform; the system calls themselves
//! (`fchflags`, `statfs`, `sysctl`) are compiled on macOS only, each behind a safe function.

mod seal;
mod securelevel;
#[cfg(target_os = "macos")]
mod sys;
mod volume;

pub use crate::sealer::Owner;
#[cfg(target_os = "macos")]
pub use seal::{Clonefile, Fchflags, MacSealer};
pub use seal::{Cloner, FlagSealer, IMMUTABLE, UserFlags};
pub use securelevel::{SecurelevelError, check_securelevel};
#[cfg(target_os = "macos")]
pub use sys::{StartError, start_checks};
pub use volume::{VolumeError, VolumeFacts, check_volumes};
