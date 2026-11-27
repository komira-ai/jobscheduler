//! Writes the x2 export of a usage store: `key_profile.txt` and `node_minute.txt` in `<dir>`,
//! from the allocations that ended in `[from-ms, before-ms)` (milliseconds since the Unix
//! epoch) and the node minutes of their hosts. `js-sched`'s `sim_x2` example replays the
//! directory.
//!
//! ```text
//! cargo run --release -p js-usage --example x2_export -- \
//!     --sqlite <copy of the store file> --from-ms <ms> --before-ms <ms> --out <dir>
//! ```
//!
//! It reads a copy of a SQLite store file (the server holds the lock of the one it runs
//! on), and writes nothing to it. The flags, the export and the files are
//! `js_usage::parse_x2_export_args`, `js_usage::x2_export` and `js_usage::write_x2_export`;
//! this program only opens the store, forwards and prints the counts.

use std::process::ExitCode;
use std::sync::Arc;

use js_state::SystemClock;
use js_state::sqlite::{Options, SqliteStore};

const USAGE: &str = "usage: x2_export --sqlite <file> --from-ms <ms> --before-ms <ms> --out <dir>";

fn main() -> ExitCode {
    let args = match js_usage::parse_x2_export_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("x2_export: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let clock = Arc::new(SystemClock::default());
    let written = SqliteStore::open(&args.sqlite, clock, Options::default())
        .map_err(|e| format!("{}: {e}", args.sqlite.display()))
        .and_then(|store| {
            js_usage::x2_export(&store, args.from_ms, args.before_ms).map_err(|e| e.to_string())
        })
        .and_then(|export| {
            js_usage::write_x2_export(&args.out, &export).map_err(|e| e.to_string())?;
            Ok(export.counts)
        });
    match written {
        Ok(counts) => {
            println!("{counts:?}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("x2_export: {message}");
            ExitCode::FAILURE
        }
    }
}
