//! `js-mac-session`: see the library's documentation.

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    js_mac_session::start::main(&js_mac_session::start::Args::parse())
}
