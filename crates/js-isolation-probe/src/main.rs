//! `js-isolation-probe`: see the library's documentation.

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    js_isolation_probe::run(&js_isolation_probe::Cli::parse())
}
