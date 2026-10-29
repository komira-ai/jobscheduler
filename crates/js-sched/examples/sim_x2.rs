//! The x2 runner: reads a directory of exported per-minute series and prints which of its
//! inputs are real and which are invented.
//!
//! ```text
//! cargo run --release -p js-sched --example sim_x2 -- --export <dir>
//! ```
//!
//! The directory holds `key_profile.txt` and, optionally, `node_minute.txt`; the format
//! is described in `tests/sim_x_replay/engine/export.rs`. This step only reads and
//! prints: it replays nothing and asserts nothing about results. A malformed line stops
//! it with the file and line number.

use std::process::ExitCode;

// The same engine as the `sim_x_replay` test, included by path.
#[path = "../tests/sim_x_replay/engine/mod.rs"]
mod engine;

const USAGE: &str = "usage: sim_x2 --export <dir>";

fn main() -> ExitCode {
    let dir = match engine::export::parse_args(std::env::args().skip(1)) {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("sim_x2: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let export = match engine::export::read_dir(&dir) {
        Ok(export) => export,
        Err(error) => {
            eprintln!("sim_x2: {error}");
            return ExitCode::FAILURE;
        }
    };
    let beliefs = export.beliefs();
    print!("{}", export.header(&beliefs));
    ExitCode::SUCCESS
}
