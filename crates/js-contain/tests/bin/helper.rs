//! The daemon's own binary as a helper container runs it, for the tests: the first
//! argument selects a helper mode, as the default [`HelperModes`] name them, and nothing
//! else is a command. A real daemon calls the same `dispatch` first in its `main`.

use std::process::ExitCode;

use js_contain::modes::dispatch;
use js_contain::oci::helper::HelperModes;

fn main() -> ExitCode {
    match dispatch(&HelperModes::default()) {
        Some(code) => ExitCode::from(code),
        None => {
            eprintln!("js-helper: the first argument is not a helper mode");
            ExitCode::from(2)
        }
    }
}
