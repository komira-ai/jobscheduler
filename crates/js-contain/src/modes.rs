//! The helper modes of the daemon's binary, told apart by the first argument.
//!
//! A helper container ([`crate::oci::helper`]) runs the daemon's own binary as
//! `<binary> <mode> <arguments>...`. The mode words are the [`HelperModes`] the runner
//! was configured with, so the words the runner puts in a helper's argv and the words
//! the binary answers to come from one value: [`dispatch`] with the same
//! [`HelperModes`] the [`OciConfig`](crate::OciConfig) holds (the default for both, in a
//! node that sets none).
//!
//! The supervise mode ([`crate::oci::supervise`]) is run the same way, as
//! `<binary> <supervise> <arguments>...`, but on the host and not in a helper container: by
//! [`Oci::start_supervised`](crate::Oci::start_supervised), to run one job's container to its
//! end in a process that outlives the agent.
//!
//! The binary's `main` calls [`dispatch`] first, before it parses its flags or installs
//! its signal handlers: a helper has no flags, and the arguments are not the program's.

use std::ffi::OsString;

use crate::oci::helper::HelperModes;
use crate::oci::supervise;
use crate::{give, unpack_layers};

/// The exit status of the helper mode that `std::env::args_os` selects, `None` if its
/// first argument names none.
#[must_use]
pub fn dispatch(modes: &HelperModes) -> Option<u8> {
    let args: Vec<OsString> = std::env::args_os().collect();
    dispatch_args(modes, &args)
}

/// [`dispatch`] for the argument list `args`, the program's name first.
#[must_use]
pub fn dispatch_args(modes: &HelperModes, args: &[OsString]) -> Option<u8> {
    let (mode, rest) = args.get(1..)?.split_first()?;
    if mode == modes.give.as_str() {
        Some(give::run(modes, rest))
    } else if mode == modes.unpack.as_str() {
        Some(unpack_layers::run(modes, rest))
    } else if mode == modes.supervise.as_str() {
        Some(supervise::run(rest))
    } else {
        None
    }
}
