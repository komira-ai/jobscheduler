//! `merge-profiles --dir <dir> --out <file> [--rustc <program>]`: one indexed coverage
//! profile from every raw one a coverage shard of `.github/workflows/ci.yml` wrote.
//!
//! cargo-llvm-cov puts each test process's raw profile at the top of its target directory,
//! as `<name>-<pid>-<signature>.profraw` (its `%p` and `%m`). This verb lists the regular
//! files there whose name ends `.profraw` (not those of its subdirectories, which hold the
//! build), sorted, and refuses a directory with none.
//!
//! A raw profile that is cut short (see [`raw`]: empty, or ending before the sections its
//! header gives, as a process killed while it wrote its profile at exit leaves it) is left
//! out of the merge: each is named on standard error with its size and the process that
//! wrote it (the pid of its name), and copied into the directory `<out>.left-out`, which is
//! made for them; the result line counts them. Nothing else is left out, and a directory
//! whose every profile is cut short is refused.
//!
//! The others are listed, one per line, in `<out>.inputs`, and the toolchain's
//! llvm-profdata merges them, `<sysroot>/lib/rustlib/<host>/bin/llvm-profdata merge -sparse
//! -f <out>.inputs -o <out>`, as `llvm-cov report` merges them. The inputs go in a file, so
//! thousands of names never go on a command line. What llvm-profdata writes on its standard
//! error is passed on. When it fails, each listed profile is given to it alone
//! (`merge -sparse <profile> -o <out>.check`, removed after), and the error names every one
//! it refuses with its size (and the size it had when listed, when that changed: a process
//! was still writing it), the process that wrote it and what llvm-profdata said of it.
//! The sysroot and the host are what `<rustc> --print sysroot` and `<rustc> --print
//! host-tuple` print; under `cargo +<toolchain> xtask`, `rustc` is that toolchain's. Each
//! program runs as one process with its arguments as separate entries (no shell). Prints one
//! line: how many raw profiles went into `<out>`, and how many were left out.

mod raw;

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The arguments of `merge-profiles`.
#[derive(clap::Args)]
pub struct Args {
    /// The directory whose `*.profraw` files (at its top level) are merged.
    #[arg(long)]
    pub dir: PathBuf,
    /// The indexed profile to write; its list of inputs is written beside it, as
    /// `<out>.inputs`, and the profiles left out as cut short are copied into
    /// `<out>.left-out`.
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
    /// llvm-profdata refused the merge; `why` says which profiles it refuses alone.
    #[error("{} failed: {status}; {why}", path.display())]
    Refused {
        /// llvm-profdata.
        path: PathBuf,
        /// How the merge ended, as `std::process::ExitStatus` shows it.
        status: std::process::ExitStatus,
        /// The profiles it refuses alone, each with its size, its writer and what
        /// llvm-profdata said of it; or that it refuses none alone.
        why: String,
    },
    /// Every raw profile of the directory is cut short: none is left to merge.
    #[error("every raw profile in {} is cut short ({count} of them)", path.display())]
    AllCutShort {
        /// The directory.
        path: PathBuf,
        /// How many raw profiles it holds.
        count: usize,
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

/// Runs `program` with `args` to its end: its status and what it printed.
fn run_program(program: &Path, args: &[&std::ffi::OsStr]) -> Result<Output, Error> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            path: program.to_path_buf(),
            source,
        })
}

/// Runs `program` with `args` and fails unless it ends with success; its standard output.
fn output(program: &Path, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, Error> {
    let out = run_program(program, args)?;
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

/// The size of the raw profile `path` and its first [`raw::HEADER`] bytes (all of it when it
/// is shorter).
fn head_of(path: &Path) -> Result<(u64, Vec<u8>), Error> {
    let read = |source| Error::Read {
        path: path.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(path).map_err(read)?;
    let size = file.metadata().map_err(read)?.len();
    let mut head = Vec::with_capacity(raw::HEADER);
    file.take(raw::HEADER as u64)
        .read_to_end(&mut head)
        .map_err(read)?;
    Ok((size, head))
}

/// The process that wrote the raw profile `path`, as its name (`<name>-<pid>-<signature>`)
/// gives it.
fn writer(path: &Path) -> String {
    let pid = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit('-').nth(1))
        .and_then(|pid| pid.parse::<u32>().ok());
    match pid {
        Some(pid) => format!("process {pid}"),
        None => "a process its name does not give".to_owned(),
    }
}

/// `path` with `suffix` appended to its last component.
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Copies the profiles `left_out` into the directory `dir`, which it makes (one already there
/// fails it: a coverage shard's target directory is new).
fn keep(left_out: &[(PathBuf, String)], dir: &Path) -> Result<(), Error> {
    std::fs::create_dir(dir)
        .and_then(|()| {
            left_out.iter().try_for_each(|(path, _)| {
                let name = path.file_name().unwrap_or_default();
                std::fs::copy(path, dir.join(name)).map(drop)
            })
        })
        .map_err(|source| Error::Write {
            path: dir.to_path_buf(),
            source,
        })
}

/// Gives llvm-profdata `profdata` each of the profiles `kept` (with the size each had when
/// listed) alone, writing to `check`: which it refuses, with their sizes, their writers and
/// what it said of each.
fn alone(profdata: &Path, kept: &[(PathBuf, u64)], check: &Path) -> Result<String, Error> {
    let mut refused = Vec::new();
    for (path, listed) in kept {
        let args = [
            "merge".as_ref(),
            "-sparse".as_ref(),
            path.as_os_str(),
            "-o".as_ref(),
            check.as_os_str(),
        ];
        let ran = run_program(profdata, &args)?;
        if ran.status.success() {
            continue;
        }
        let size = match std::fs::metadata(path).map(|now| now.len()) {
            Ok(now) if now == *listed => format!("{now} bytes"),
            Ok(now) => format!("{listed} bytes when listed and {now} now, still being written"),
            Err(_) => format!("{listed} bytes when listed and gone now"),
        };
        let said = String::from_utf8_lossy(&ran.stderr);
        let said: Vec<&str> = said.lines().collect();
        refused.push(format!(
            "\n  {} ({size}, written by {}): {}",
            path.display(),
            writer(path),
            said.join(" / ")
        ));
    }
    let _ = std::fs::remove_file(check);
    Ok(match refused.is_empty() {
        true => format!("it accepts each of the {} raw profiles alone", kept.len()),
        false => format!(
            "it refuses {} of the {} raw profiles alone:{}",
            refused.len(),
            kept.len(),
            refused.concat()
        ),
    })
}

/// Merges the raw profiles of `args.dir` into `args.out`: the line to print.
pub fn run(args: &Args) -> Result<String, Error> {
    let mut kept = Vec::new();
    let mut left_out = Vec::new();
    for path in raw_profiles(&args.dir)? {
        let (size, head) = head_of(&path)?;
        match raw::cut_short(&head, size) {
            Some(why) => left_out.push((path, why)),
            None => kept.push((path, size)),
        }
    }
    let left_dir = suffixed(&args.out, ".left-out");
    for (path, why) in &left_out {
        eprintln!(
            "xtask: merge-profiles: left out {}: cut short ({why}), written by {}",
            path.display(),
            writer(path)
        );
    }
    if !left_out.is_empty() {
        keep(&left_out, &left_dir)?;
    }
    if kept.is_empty() {
        return Err(Error::AllCutShort {
            path: args.dir.clone(),
            count: left_out.len(),
        });
    }
    let list = suffixed(&args.out, ".inputs");
    let mut text = String::new();
    for (input, _) in &kept {
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
    let merged = run_program(
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
    eprint!("{}", String::from_utf8_lossy(&merged.stderr));
    if !merged.status.success() {
        let why = alone(&profdata, &kept, &suffixed(&args.out, ".check"))?;
        return Err(Error::Refused {
            path: profdata,
            status: merged.status,
            why,
        });
    }
    let kept_in = match left_out.is_empty() {
        true => String::new(),
        false => format!(", kept in {}", left_dir.display()),
    };
    Ok(format!(
        "merged {} raw profiles into {}; left out {} cut short{kept_in}",
        kept.len(),
        args.out.display(),
        left_out.len(),
    ))
}
