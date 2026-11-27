//! `js-lease-server job-store`: the job store's cutover between a SQLite file and the store.
//!
//! ```text
//! js-lease-server job-store copy --from <state file>.jobs --store <file> [--write]
//! js-lease-server job-store export --store <file> --jobs-to <file> --usage-to <file>
//! ```
//!
//! A server without a store setting keeps its job store in the SQLite file `<state file>.jobs`;
//! with `--store` the job store is rows of that store, and the file is not read. These verbs
//! move the rows across, with every server on them stopped (the cutover is a short pause):
//!
//! - **`copy`** reads every row of the file and says, per key prefix, how many rows it holds
//!   and a SHA-256 of them ([`rows::Tally`]), and how many of them the store does not hold, holds
//!   with the same value, or holds with another. Without `--write` that is all it does: a dry
//!   run that writes nothing. With `--write` it writes the rows the store does not hold, a page
//!   per transaction, each put conditional on its key being absent; a row the store holds with
//!   the same value is left as it is, so a copy that stopped part way is run again. A copy that
//!   finds a row the store holds with another value writes nothing, with or without `--write`,
//!   and names each such key. The dry run after a write is the check of it: it reports every
//!   row as in the store with the same value. A row with a time to live (a cordon) is written
//!   with what is left of it.
//! - **`export`** is the way back, for a rollback to a server without a store setting: it writes
//!   the store's job store rows ([`rows::JOB_ROWS`], the `usage/ended` rows with them, as a
//!   server without the setting keeps them) into a new SQLite file to put in the place of
//!   `<state file>.jobs`, and its usage rows ([`rows::USAGE_ROWS`]) into a new SQLite file to
//!   put in the place of `usage.db`, and says the same per prefix counts and checksums of each.
//!   Neither file may exist; both are made owner-only, as the server makes its own. The store's
//!   host bindings, join tokens and scaler rows are not exported: a server without the setting
//!   keeps them in the files the store's first start imported. The store is only read.
//!
//! `--store` names a file holding one line, as the server's `--store` does (owner-only;
//! `postgres://...`, or `sqlite:<absolute path>`). Both verbs read the SQLite files through the
//! store's own lock, so one that a running server holds is refused. The verb is answered
//! before the server's flags are read; it binds no listener. Exit status: 0 when the verb did
//! what it says, 1 when it refused or failed (the line on standard error says why), 2 for a
//! usage error.

mod rows;

use std::ffi::OsString;
use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Store, SystemClock};

use crate::config::{StoreClock, StoreNames, StoreSetting, hook_store_target, read_private};
use crate::error::StartupError;
use crate::store::ServerStore;
use rows::{JOB_ROWS, RowsError, Tally, USAGE_ROWS};

/// The first argument that selects the verb.
pub(crate) const VERB: &str = "job-store";

/// The verb's command line, after `js-lease-server`.
#[derive(Debug, Parser)]
#[command(
    name = "js-lease-server job-store",
    bin_name = "js-lease-server job-store",
    about = "Copy the job store's SQLite file into the store, or export the store's job and \
             usage rows to SQLite files"
)]
struct Args {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// The verb's two commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Copy every row of a job store's SQLite file into the store: a dry run unless --write.
    Copy {
        /// The job store's file, `<state file>.jobs`.
        #[arg(long)]
        from: PathBuf,
        /// The file holding the store's one line, as the server's --store.
        #[arg(long)]
        store: PathBuf,
        /// Write the rows the store does not hold; without it nothing is written.
        #[arg(long)]
        write: bool,
    },
    /// Export the store's job and usage rows to two new SQLite files.
    Export {
        /// The file holding the store's one line, as the server's --store.
        #[arg(long)]
        store: PathBuf,
        /// The new file for the job store's rows, to put in the place of `<state file>.jobs`.
        #[arg(long)]
        jobs_to: PathBuf,
        /// The new file for the usage rows, to put in the place of `usage.db`.
        #[arg(long)]
        usage_to: PathBuf,
    },
}

/// Why the verb did not do what it says.
#[derive(Debug, thiserror::Error)]
enum Refusal {
    /// `--store` could not be read or opened.
    #[error("{0}")]
    Store(#[from] StartupError),
    /// A SQLite file could not be opened or made.
    #[error("{}: {reason}", path.display())]
    File {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The rows could not be read or written.
    #[error("{0}")]
    Rows(#[from] RowsError),
    /// The store holds rows of the file with other values.
    #[error(
        "the store holds {0} of the file's rows with other values (named above): nothing was \
         written"
    )]
    Different(usize),
}

/// Whether `args` (the program name first) select the verb.
pub(crate) fn is_verb(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|verb| verb == VERB)
}

/// Runs the verb on `args` (the program name and the verb first): the report goes to `out`,
/// a refusal to `err`.
pub(crate) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let parsed = Args::try_parse_from(&args[1..]);
    let command = match parsed {
        Ok(parsed) => parsed.command,
        Err(usage) => return crate::usage_exit(&usage, out, err),
    };
    let done = match command {
        Command::Copy { from, store, write } => copy(&from, &store, write, out),
        Command::Export {
            store,
            jobs_to,
            usage_to,
        } => export(&store, (&jobs_to, &usage_to), out),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(refusal) => {
            let _ = writeln!(err, "js-lease-server job-store: {refusal}");
            ExitCode::FAILURE
        }
    }
}

/// `copy`: the plan of copying the file `from` into the store `store_file` names, and, with
/// `write`, the write.
fn copy(from: &Path, store_file: &Path, write: bool, out: &mut dyn Write) -> Result<(), Refusal> {
    let source = existing_file(from)?;
    let target = open_store(store_file)?;
    let plan = rows::plan(&source, &*target)?;
    let _ = writeln!(out, "{}:", from.display());
    let _ = plan.tally.write_to(out);
    let _ = writeln!(
        out,
        "the store: {} rows to write, {} already there with the same value, {} with another",
        plan.missing,
        plan.same,
        plan.different.len()
    );
    for key in &plan.different {
        let _ = writeln!(out, "different: {key}");
    }
    if !plan.different.is_empty() {
        return Err(Refusal::Different(plan.different.len()));
    }
    if write {
        let wrote = rows::write(&source, &*target)?;
        let _ = writeln!(out, "wrote {wrote} rows into the store");
    } else {
        let _ = writeln!(out, "dry run: nothing was written (--write writes them)");
    }
    Ok(())
}

/// `export`: the store `store_file` names, its job store rows into the new file `jobs_to` and
/// its usage rows into the new file `usage_to`.
fn export(
    store_file: &Path,
    (jobs_to, usage_to): (&Path, &Path),
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    if jobs_to == usage_to {
        return Err(file_error(jobs_to)(
            "--jobs-to and --usage-to name one file",
        ));
    }
    // Both are looked for before either is made, so a refusal leaves no file behind.
    for to in [jobs_to, usage_to] {
        if to.symlink_metadata().is_ok() {
            return Err(file_error(to)(
                "the file is there: an export makes new files only",
            ));
        }
    }
    let store = open_store(store_file)?;
    for (to, prefixes) in [(jobs_to, JOB_ROWS), (usage_to, USAGE_ROWS)] {
        let file = new_file(to)?;
        let tally = rows::export(&*store, prefixes, &file)?;
        report(out, to, &tally);
    }
    Ok(())
}

/// Writes the tally of the rows written to `to`.
fn report(out: &mut dyn Write, to: &Path, tally: &Tally) {
    let _ = writeln!(out, "{}:", to.display());
    let _ = tally.write_to(out);
    let _ = writeln!(out, "wrote {} rows to {}", tally.rows(), to.display());
}

/// The store the file `store_file` names, read as the server reads `--store`.
fn open_store(store_file: &Path) -> Result<Arc<dyn Store>, Refusal> {
    let bytes = read_private(store_file, "--store")?;
    let setting = StoreSetting {
        clock: StoreClock::default(),
        target: hook_store_target(&bytes, store_file)?,
        names: StoreNames::Flags,
    };
    Ok(ServerStore::open(setting)?.store())
}

/// The SQLite state file at `path`, which must be there: opening one that is not would make
/// it.
fn existing_file(path: &Path) -> Result<SqliteStore, Refusal> {
    if !path.is_file() {
        return Err(file_error(path)("there is no such file"));
    }
    sqlite(path)
}

/// A new SQLite state file at `path`, owner-only; one already there is refused.
fn new_file(path: &Path) -> Result<SqliteStore, Refusal> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(file_error(path))?;
    sqlite(path)
}

/// The SQLite state file at `path`, on the system's clock.
fn sqlite(path: &Path) -> Result<SqliteStore, Refusal> {
    let clock = Arc::new(SystemClock::default());
    SqliteStore::open(path, clock, Options::default()).map_err(file_error(path))
}

/// The refusal of the file `path` for a reason.
fn file_error<E: Display>(path: &Path) -> impl FnOnce(E) -> Refusal + '_ {
    move |reason| Refusal::File {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
#[path = "cutover_tests.rs"]
mod tests;
