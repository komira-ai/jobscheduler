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
//! on), and writes nothing to it. What the files hold is `js_usage::x2_export`'s doc; the
//! counts of what was written and of the history rows left out, by cause, are printed.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use js_state::SystemClock;
use js_state::sqlite::{Options, SqliteStore};

const USAGE: &str = "usage: x2_export --sqlite <file> --from-ms <ms> --before-ms <ms> --out <dir>";

struct Args {
    sqlite: PathBuf,
    from_ms: u64,
    before_ms: u64,
    out: PathBuf,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut sqlite, mut from, mut before, mut out) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("{flag} is not a whole number"))
        };
        let slot_was = match flag.as_str() {
            "--sqlite" => sqlite.replace(PathBuf::from(&value)).is_some(),
            "--from-ms" => from.replace(number()?).is_some(),
            "--before-ms" => before.replace(number()?).is_some(),
            "--out" => out.replace(PathBuf::from(&value)).is_some(),
            _ => return Err(format!("unknown argument {flag:?}")),
        };
        if slot_was {
            return Err(format!("{flag} given twice"));
        }
    }
    Ok(Args {
        sqlite: sqlite.ok_or("--sqlite is required")?,
        from_ms: from.ok_or("--from-ms is required")?,
        before_ms: before.ok_or("--before-ms is required")?,
        out: out.ok_or("--out is required")?,
    })
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("x2_export: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let clock = Arc::new(SystemClock::default());
    let store = match SqliteStore::open(&args.sqlite, clock, Options::default()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("x2_export: {}: {error}", args.sqlite.display());
            return ExitCode::FAILURE;
        }
    };
    let export = match js_usage::x2_export(&store, args.from_ms, args.before_ms) {
        Ok(export) => export,
        Err(error) => {
            eprintln!("x2_export: {error}");
            return ExitCode::FAILURE;
        }
    };
    let files = [
        ("key_profile.txt", &export.key_profile),
        ("node_minute.txt", &export.node_minute),
    ];
    for (name, text) in files {
        let path = args.out.join(name);
        if let Err(error) = std::fs::write(&path, text) {
            eprintln!("x2_export: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    }
    println!("{:?}", export.counts);
    ExitCode::SUCCESS
}
