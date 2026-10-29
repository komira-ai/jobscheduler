//! `js-mac-probe`: see the library's documentation.

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    js_mac_probe::run(&js_mac_probe::Cli::parse())
}
