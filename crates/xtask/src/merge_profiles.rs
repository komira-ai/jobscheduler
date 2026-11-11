//! `merge-profiles --dir <dir> --out <file> [--rustc <program>]`: one indexed coverage
//! profile from every raw one a coverage shard of `.github/workflows/ci.yml` wrote.
//!
//! cargo-llvm-cov puts each test process's raw profile at the top of its target directory,
//! as `<name>.profraw`. This verb lists the regular files there whose name ends `.profraw`
//! (not those of its subdirectories, which hold the build), sorted, one per line, in
//! `<out>.inputs`; refuses a directory with none; then runs the toolchain's llvm-profdata,
//! `<sysroot>/lib/rustlib/<host>/bin/llvm-profdata merge -sparse -f <out>.inputs -o <out>`,
//! as `llvm-cov report` merges them. The inputs go in a file, so thousands of names never
//! go on a command line. The sysroot and the host are what `<rustc> --print sysroot` and
//! `<rustc> --print host-tuple` print; under `cargo +<toolchain> xtask`, `rustc` is that
//! toolchain's. Each program runs as one process with its arguments as separate entries
//! (no shell). Prints one line: how many raw profiles went into `<out>`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The arguments of `merge-profiles`.
#[derive(clap::Args)]
pub struct Args {
    /// The directory whose `*.profraw` files (at its top level) are merged.
    #[arg(long)]
    pub dir: PathBuf,
    /// The indexed profile to write; its list of inputs is written beside it, as
    /// `<out>.inputs`.
    #[arg(long)]
    pub out: PathBuf,
    /// The rustc whose sysroot holds llvm-profdata (the `llvm-tools` component).
    #[arg(long, default_value = "rustc")]
    pub rustc: OsString,
}

/// Why the profiles could not be merged.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The directory could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The directory.
        path: PathBuf,
        /// Why it could not be read.
        #[source]
        source: std::io::Error,
    },
    /// The directory holds no raw profile.
    #[error("no *.profraw file in {}", path.display())]
    NoProfiles {
        /// The directory.
        path: PathBuf,
    },
    /// The list of inputs could not be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The list file.
        path: PathBuf,
        /// Why it could not be written.
        #[source]
        source: std::io::Error,
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
    #[error("{} failed: {status}", path.display())]
    Failed {
        /// The program.
        path: PathBuf,
        /// How it ended, as `std::process::ExitStatus` shows it.
        status: std::process::ExitStatus,
    },
    /// rustc printed no answer, or more than one line.
    #[error("{} --print {what} printed {printed:?}, not one line", path.display())]
    Answer {
        /// The rustc.
        path: PathBuf,
        /// What it was asked to print (`sysroot` or `host-tuple`).
        what: &'static str,
        /// What it printed.
        printed: String,
    },
}

/// Runs `program` with `args` and fails unless it ends with success; its standard output.
fn output(program: &Path, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, Error> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            path: program.to_path_buf(),
            source,
        })?;
    match out.status.success() {
        true => Ok(out.stdout),
        false => Err(Error::Failed {
            path: program.to_path_buf(),
            status: out.status,
        }),
    }
}

/// The one line rustc prints for `--print <what>`.
fn ask(rustc: &Path, what: &'static str) -> Result<String, Error> {
    let printed = output(rustc, &["--print".as_ref(), what.as_ref()])?;
    let text = String::from_utf8_lossy(&printed);
    let line = text.trim_end_matches('\n');
    match line.is_empty() || line.contains('\n') {
        true => Err(Error::Answer {
            path: rustc.to_path_buf(),
            what,
            printed: text.into_owned(),
        }),
        false => Ok(line.to_owned()),
    }
}

/// The raw profiles at the top of `dir`, sorted.
fn raw_profiles(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let read = |source| Error::Read {
        path: dir.to_path_buf(),
        source,
    };
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(read)? {
        let entry = entry.map_err(read)?;
        let path = entry.path();
        let raw = path.extension().is_some_and(|e| e == "profraw");
        if raw && entry.file_type().map_err(read)?.is_file() {
            found.push(path);
        }
    }
    found.sort();
    match found.is_empty() {
        true => Err(Error::NoProfiles {
            path: dir.to_path_buf(),
        }),
        false => Ok(found),
    }
}

/// Merges the raw profiles of `args.dir` into `args.out`: the line to print.
pub fn run(args: &Args) -> Result<String, Error> {
    let inputs = raw_profiles(&args.dir)?;
    let mut list = args.out.clone().into_os_string();
    list.push(".inputs");
    let list = PathBuf::from(list);
    let mut text = String::new();
    for input in &inputs {
        text.push_str(&input.to_string_lossy());
        text.push('\n');
    }
    std::fs::write(&list, text).map_err(|source| Error::Write {
        path: list.clone(),
        source,
    })?;
    let rustc = Path::new(&args.rustc);
    let sysroot = ask(rustc, "sysroot")?;
    let host = ask(rustc, "host-tuple")?;
    let profdata = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin/llvm-profdata");
    output(
        &profdata,
        &[
            "merge".as_ref(),
            "-sparse".as_ref(),
            "-f".as_ref(),
            list.as_os_str(),
            "-o".as_ref(),
            args.out.as_os_str(),
        ],
    )?;
    Ok(format!(
        "merged {} raw profiles into {}",
        inputs.len(),
        args.out.display()
    ))
}
