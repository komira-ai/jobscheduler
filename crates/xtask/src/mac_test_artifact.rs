//! `mac-test-artifact --test <name> --target <triple> --build-json <file> --out <dir>
//! --commit <sha> --summary <file> --github-env <file> [--target-dir <dir>]`: stages a Mac root
//! test binary for an operator, as the manual workflows of `js-agent`'s root tests upload it
//! (`.github/workflows/js-agent-native-*.yml`).
//!
//! From the messages `cargo test --no-run --message-format=json` wrote to `--build-json`, it
//! takes the one test executable of the integration test `--test` (a `compiler-artifact` of that
//! target's name, of kind `test`, built for tests, with an executable: never a guess by file
//! name), and copies it to `<out>/<test>` beside the `js-agent` binary cargo built for it
//! (`<target-dir>/<triple>/debug/js-agent`, which the test starts as its supervisor), each
//! checked to be an arm64 Mach-O executable. It writes `<out>/SHA256SUMS` (the test binary, then
//! `js-agent`, as `shasum -a 256` prints them), appends a summary of the two to `--summary` and
//! `SHORT=<first 12 hex of --commit>` to `--github-env`, and prints what it did.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The name of the supervisor binary staged beside the test binary.
pub const SUPERVISOR: &str = "js-agent";

/// The arguments of `mac-test-artifact`.
#[derive(clap::Args)]
pub struct Args {
    /// The integration test target of `js-agent` whose binary is staged.
    #[arg(long)]
    pub test: String,
    /// The target triple the test was built for.
    #[arg(long)]
    pub target: String,
    /// The JSON messages of `cargo test --no-run --message-format=json`.
    #[arg(long)]
    pub build_json: PathBuf,
    /// The directory the files are staged in; made, and must not exist.
    #[arg(long)]
    pub out: PathBuf,
    /// The commit the binaries were built from.
    #[arg(long)]
    pub commit: String,
    /// The file the summary is appended to (on Actions, `GITHUB_STEP_SUMMARY`).
    #[arg(long)]
    pub summary: PathBuf,
    /// The file `SHORT=<commit>` is appended to (on Actions, `GITHUB_ENV`).
    #[arg(long)]
    pub github_env: PathBuf,
    /// Cargo's target directory.
    #[arg(long, default_value = "target")]
    pub target_dir: PathBuf,
}

/// Why the binaries could not be staged.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file could not be read, written or copied.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// A line of the build messages is not JSON.
    #[error("{}: line {line} is not JSON: {why}", path.display())]
    Json {
        /// The file of build messages.
        path: PathBuf,
        /// The line, from 1.
        line: usize,
        /// Why it does not parse.
        why: String,
    },
    /// Not exactly one test executable of the target was built.
    #[error("expected exactly one {test} test executable, got {found:?}")]
    Executables {
        /// The test target.
        test: String,
        /// The executables found.
        found: Vec<String>,
    },
    /// A staged file is not an arm64 Mach-O executable.
    #[error("{} is not an arm64 Mach-O executable", path.display())]
    NotArm64 {
        /// The file.
        path: PathBuf,
    },
}

/// An error of `path`.
fn at(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The executable of the one test binary of the target `test` among the cargo messages `text`
/// (of the file `path`, for errors).
///
/// # Errors
/// A line is not JSON, or not exactly one such executable is named.
pub fn test_executable(text: &str, path: &Path, test: &str) -> Result<PathBuf, Error> {
    let mut found = Vec::new();
    for (n, line) in text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let message: serde_json::Value = serde_json::from_str(line).map_err(|why| Error::Json {
            path: path.to_path_buf(),
            line: n + 1,
            why: why.to_string(),
        })?;
        let is_test_kind = message["target"]["kind"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "test"));
        let wanted = message["reason"] == "compiler-artifact"
            && message["profile"]["test"] == true
            && message["target"]["name"] == test
            && is_test_kind;
        found.extend(
            message["executable"]
                .as_str()
                .filter(|_| wanted)
                .map(str::to_owned),
        );
    }
    match found.as_slice() {
        [one] => Ok(PathBuf::from(one)),
        _ => Err(Error::Executables {
            test: test.to_owned(),
            found,
        }),
    }
}

/// Whether `head` (a file's first bytes) is that of an arm64 Mach-O executable: the 64-bit
/// magic, the arm64 CPU type and the executable file type, little-endian.
#[must_use]
pub fn is_arm64_executable(head: &[u8]) -> bool {
    const MAGIC_64: [u8; 4] = 0xfeed_facf_u32.to_le_bytes();
    const CPU_ARM64: [u8; 4] = 0x0100_000c_u32.to_le_bytes();
    const EXECUTE: [u8; 4] = 2_u32.to_le_bytes();
    head.len() >= 16 && head[0..4] == MAGIC_64 && head[4..8] == CPU_ARM64 && head[12..16] == EXECUTE
}

/// Copies `from` to `to` and checks the copy is an arm64 Mach-O executable; its SHA-256.
fn stage(from: &Path, to: &Path) -> Result<String, Error> {
    let bytes = fs::copy(from, to)
        .and_then(|_| fs::read(to))
        .map_err(at(from))?;
    if !is_arm64_executable(&bytes) {
        return Err(Error::NotArm64 {
            path: to.to_path_buf(),
        });
    }
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Appends `text` to the file `path`, made when missing.
fn append(path: &Path, text: &str) -> Result<(), Error> {
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(at(path))
}

/// Stages the binaries as the module documentation says; what it did, a line each.
///
/// # Errors
/// What could not be read, found, checked or written.
pub fn run(args: &Args) -> Result<String, Error> {
    let text = fs::read_to_string(&args.build_json).map_err(at(&args.build_json))?;
    let exe = test_executable(&text, &args.build_json, &args.test)?;
    fs::create_dir(&args.out).map_err(at(&args.out))?;
    let test_sha = stage(&exe, &args.out.join(&args.test))?;
    let supervisor = args
        .target_dir
        .join(&args.target)
        .join("debug")
        .join(SUPERVISOR);
    let supervisor_sha = stage(&supervisor, &args.out.join(SUPERVISOR))?;
    let sums = format!(
        "{test_sha}  {}\n{supervisor_sha}  {SUPERVISOR}\n",
        args.test
    );
    let sums_path = args.out.join("SHA256SUMS");
    let short: String = args.commit.chars().take(12).collect();
    let mut summary = String::new();
    let _ = write!(
        summary,
        "### js-agent {} test binary\n\n- commit: `{}`\n- target: `{}`\n- sha256: `{test_sha}`\n\
         - {SUPERVISOR} sha256: `{supervisor_sha}`\n- file: arm64 Mach-O executables\n",
        args.test, args.commit, args.target
    );
    fs::write(&sums_path, &sums)
        .map_err(at(&sums_path))
        .and_then(|()| append(&args.summary, &summary))
        .and_then(|()| append(&args.github_env, &format!("SHORT={short}\n")))?;
    Ok(format!(
        "staged {} ({test_sha}) and {SUPERVISOR} ({supervisor_sha}) in {}",
        args.test,
        args.out.display()
    ))
}
