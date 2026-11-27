//! The x2 runner: replays a directory of exported per-minute series through the compared
//! rankers and prints the report.
//!
//! ```text
//! cargo run --release -p js-sched --example sim_x2 -- --export <dir> \
//!     [--hosts N] [--threads N] [--mem-mib N] \
//!     [--balance N] [--slack-tol-ppm N] [--key-share-ppm N] [--seed N]
//! ```
//!
//! The directory holds `key_profile.txt` and, optionally, `node_minute.txt`; the format
//! is described in `tests/sim_x_replay/engine/export.rs`, and `js-usage`'s `x2_export`
//! example writes it from a usage store. The pool is invented (6 hosts of 88 threads and
//! 251 GiB unless the flags say otherwise); the score's flags default to the scorer's. The
//! report (`tests/sim_x_replay/engine/x2.rs`) is printed, not asserted: it says what each
//! ranker did and the gain of the score with curves over the same score on flat beliefs. A
//! malformed line stops it with the file and line number.

use std::process::ExitCode;

// The same engine as the `sim_x_replay` test, included by path.
#[path = "../tests/sim_x_replay/engine/mod.rs"]
mod engine;

const USAGE: &str = "usage: sim_x2 --export <dir> [--hosts N] [--threads N] [--mem-mib N] \
                     [--balance N] [--slack-tol-ppm N] [--key-share-ppm N] [--seed N]";

fn main() -> ExitCode {
    let args = match engine::x2::parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("sim_x2: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let export = match engine::export::read_dir(&args.export) {
        Ok(export) => export,
        Err(error) => {
            eprintln!("sim_x2: {error}");
            return ExitCode::FAILURE;
        }
    };
    print!("{}", engine::x2::run(&export, &args.config).render());
    ExitCode::SUCCESS
}
