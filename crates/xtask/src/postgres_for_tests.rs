//! `postgres-for-tests --bin-dir <dir> --data-dir <dir> --socket-dir <dir> [--port <n>]
//! --github-env <file>`: starts a Postgres server for the tests of the `macos` job of
//! `.github/workflows/ci.yml` that need one, from the programs of an installed Postgres.
//!
//! Refuses a `--data-dir` that exists and is not empty, before running anything. Then runs, each
//! as one process with its arguments as separate entries (no shell), and stops at the first that
//! fails:
//!
//! 1. `<bin-dir>/initdb --username postgres --auth trust --encoding UTF8 -D <data-dir>`;
//! 2. `<bin-dir>/pg_ctl -D <data-dir> -l <data-dir>.log -o "-h 127.0.0.1 -p <port> -k
//!    <socket-dir>" -w start`;
//! 3. `<bin-dir>/pg_isready -h 127.0.0.1 -p <port>`.
//!
//! Then appends the line `KBF_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:<port>/postgres`
//! to `--github-env` (GitHub's environment file for the later steps; its content is kept).
//! Prints one line for each step done.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The port the server listens on unless `--port` says otherwise.
pub const DEFAULT_PORT: u16 = 5432;

/// The arguments of `postgres-for-tests`.
#[derive(clap::Args)]
pub struct Args {
    /// The directory holding `initdb`, `pg_ctl` and `pg_isready`.
    #[arg(long)]
    pub bin_dir: PathBuf,
    /// The directory the database cluster is made in; it must not exist or must be empty.
    #[arg(long)]
    pub data_dir: PathBuf,
    /// The directory of the server's Unix socket.
    #[arg(long)]
    pub socket_dir: PathBuf,
    /// The port the server listens on, on 127.0.0.1.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// The file the server's URL is appended to (on Actions, `GITHUB_ENV`).
    #[arg(long)]
    pub github_env: PathBuf,
}

/// Why the server could not be started.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The data directory already holds files.
    #[error("{} already exists and is not empty", path.display())]
    DataDirNotEmpty {
        /// The data directory.
        path: PathBuf,
    },
    /// A program could not be started.
    #[error("cannot run {}: {source}", path.display())]
    Spawn {
        /// The program.
        path: PathBuf,
        /// Why it could not be started.
        #[source]
        source: std::io::Error,
    },
    /// A program ended with a failure status.
    #[error("{program} failed: {status}")]
    Failed {
        /// The program's name.
        program: &'static str,
        /// How it ended, as `std::process::ExitStatus` shows it.
        status: std::process::ExitStatus,
    },
    /// The environment file could not be appended to.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The environment file.
        path: PathBuf,
        /// Why it could not be written.
        #[source]
        source: std::io::Error,
    },
}

/// Whether `dir` exists and holds at least one entry.
#[must_use]
fn is_non_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// `<data-dir>.log`: the data directory's path with `.log` appended.
#[must_use]
fn log_path(data_dir: &Path) -> OsString {
    let mut log = data_dir.as_os_str().to_owned();
    log.push(".log");
    log
}

/// The three steps as (program name, its arguments), in the order they run.
#[must_use]
fn steps(args: &Args) -> [(&'static str, Vec<OsString>); 3] {
    let port = args.port.to_string();
    let options = format!("-h 127.0.0.1 -p {port} -k {}", args.socket_dir.display());
    let own = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
    let mut initdb = own(&[
        "--username",
        "postgres",
        "--auth",
        "trust",
        "--encoding",
        "UTF8",
        "-D",
    ]);
    initdb.push(args.data_dir.clone().into());
    let mut pg_ctl = own(&["-D"]);
    pg_ctl.push(args.data_dir.clone().into());
    pg_ctl.extend(own(&["-l"]));
    pg_ctl.push(log_path(&args.data_dir));
    pg_ctl.extend(own(&["-o", &options, "-w", "start"]));
    let pg_isready = own(&["-h", "127.0.0.1", "-p", &port]);
    [
        ("initdb", initdb),
        ("pg_ctl", pg_ctl),
        ("pg_isready", pg_isready),
    ]
}

/// Starts the server and records its URL; returns what was done, one line each.
///
/// # Errors
/// [`Error::DataDirNotEmpty`] before anything runs; [`Error::Spawn`] when a program cannot be
/// started; [`Error::Failed`] when one ends with a failure status (the later ones do not run);
/// [`Error::Write`] when the environment file cannot be appended to.
pub fn run(args: &Args) -> Result<String, Error> {
    if is_non_empty(&args.data_dir) {
        return Err(Error::DataDirNotEmpty {
            path: args.data_dir.clone(),
        });
    }
    let mut done = Vec::new();
    for (program, argv) in steps(args) {
        let path = args.bin_dir.join(program);
        // initdb's progress text is long and of no use to the later steps.
        let stdout = if program == "initdb" {
            Stdio::null()
        } else {
            Stdio::inherit()
        };
        let status = Command::new(&path)
            .args(&argv)
            .stdout(stdout)
            .status()
            .map_err(|source| Error::Spawn {
                path: path.clone(),
                source,
            })?;
        if !status.success() {
            return Err(Error::Failed { program, status });
        }
        done.push(format!("ran {program}"));
    }
    let url = format!("postgres://postgres@127.0.0.1:{}/postgres", args.port);
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.github_env)
        .and_then(|mut file| writeln!(file, "KBF_TEST_POSTGRES_URL={url}"));
    write.map_err(|source| Error::Write {
        path: args.github_env.clone(),
        source,
    })?;
    done.push(format!(
        "recorded KBF_TEST_POSTGRES_URL={url} in {}",
        args.github_env.display()
    ));
    Ok(done.join("\n"))
}
