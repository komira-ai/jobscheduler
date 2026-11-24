//! `mac-farm --address <grpc://host:port> --scratch <dir> [--output <path>]... [--env NAME=VALUE]...
//! -- <cargo arguments>` ([`run`]): runs one Cargo command of this workspace on a Mac of the
//! build farm, from a Linux runner, as one REAPI action (docs/mac-farm.md), and fetches the files
//! it makes.
//!
//! A farm Mac runs an action as an ordinary user under a sandbox with no network, and has the
//! Command Line Tools and no Rust. So everything the command needs is an input of the action,
//! gathered here:
//!
//! 1. the files git tracks in the checkout, at their paths, as they are on disk (a version the
//!    job stamped is sent);
//! 2. the crates of `Cargo.lock` for `aarch64-apple-darwin`, which `cargo fetch` downloads into
//!    a Cargo home of this run's own (in `--scratch`), below [`CARGO_HOME`];
//! 3. the pinned Rust toolchain for `aarch64-apple-darwin` ([`toolchain`]), below [`TOOLCHAIN`].
//!
//! The Command is the toolchain's `cargo` with the arguments after `--`, from the checkout's
//! root, offline, with the toolchain's `rustc` and `rustdoc`, the toolchain's `bin` and then the
//! Mac's `/usr/bin` on `PATH` (so a `cargo xtask` verb finds `cargo`, and a build its C compiler
//! and linker), and each `--env` (`MACOSX_DEPLOYMENT_TARGET=11.0`). Its outputs are the
//! `--output` paths, relative to the checkout's root. The action's platform is `--properties`,
//! the farm's Mac platform.
//!
//! The blobs the farm's CAS lacks are uploaded, the action is executed and followed until it is
//! done (a stage change is printed: queued while no Mac is free, then running), and the
//! command's stdout and stderr are printed. When the command exited 0, each output is fetched
//! from the CAS, held to its digest and written at its path below `--download-to` (by default
//! the checkout's root, where the command made it on the Mac) ([`outputs`]). The verb ends 0
//! when the command exited 0 and every output was written, and 1 for every other end: the
//! command's own non-zero exit, an output it did not make, or the farm's refusal or failure,
//! whose status is printed (with the job's output tail when the farm attached one).
//!
//! The verb's first lines mask the address in a workflow's log (`::add-mask::`), so nothing it
//! prints after them shows it. (A workflow step's own header still shows the values of its
//! `env:`; that is the workflow's to keep out.)
//!
//! With an empty `--address` the verb fails, naming the variable that feeds it: a job that runs
//! it means to build on the farm, and one with no farm must not pass having built nothing.

pub mod client;
pub mod outputs;
pub mod proto;
pub mod toolchain;
pub mod tree;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::Args as ClapArgs;
use prost::Message as _;
use tonic::Code;

use self::client::{Farm, FarmError, Progress};
use self::outputs::OutputError;
use self::proto::google::rpc;
use self::proto::reapi::{
    Action, Command, Digest, ExecuteResponse, Platform, command::EnvironmentVariable,
    platform::Property,
};
use self::toolchain::ToolchainError;
use self::tree::{Entry, Tree, TreeError};

/// The directory of the input root the run's own inputs are in, which the checkout must not
/// have.
pub const OURS: &str = ".mac-farm";

/// Where the toolchain is in the input root.
pub const TOOLCHAIN: &str = ".mac-farm/toolchain";

/// Where the Cargo home is in the input root.
pub const CARGO_HOME: &str = ".mac-farm/cargo-home";

/// The target the crates are fetched for, and the Mac's.
pub const TARGET: &str = "aarch64-apple-darwin";

/// The action's timeout when `--timeout-secs` is not given: under the farm's default limit of
/// 3000 s.
pub const DEFAULT_TIMEOUT_SECS: u64 = 2400;

/// The Mac's programs a build uses after the toolchain's: its C compiler and linker, `git`,
/// `xcrun`, `otool`.
const MAC_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The entries of a Cargo home's top directory that are not sent: Cargo's locks and its cache
/// tracker, which change on every run, and the installed programs.
const HOME_SKIPPED: [&str; 4] = [
    ".global-cache",
    ".package-cache",
    ".package-cache-mutate",
    "bin",
];

/// The reason of the `ErrorInfo` the farm attaches to a failed job: its output tail.
const OUTPUT_TAIL: &str = "JOB_OUTPUT_TAIL";

/// The flags of the verb.
#[derive(Debug, Clone, ClapArgs)]
pub struct Args {
    /// The farm's REAPI address, `grpc://<host>:<port>`; empty is refused.
    #[arg(long)]
    pub address: String,
    /// The instance name sent on every request. Empty: the empty instance.
    #[arg(long, default_value = "")]
    pub instance: String,
    /// Whether the address is served over TLS: `true` or `false`.
    #[arg(long, default_value = "")]
    pub tls: String,
    /// The farm's Mac platform properties, `key=value` separated by commas.
    #[arg(long, default_value = "")]
    pub properties: String,
    /// A directory of this run's own: the Cargo home of `cargo fetch` is made in it.
    #[arg(long)]
    pub scratch: PathBuf,
    /// The action's timeout, in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout_secs: u64,
    /// A file or directory the command makes, relative to the checkout's root, fetched once it
    /// passed. Repeated for each.
    #[arg(long = "output")]
    pub outputs: Vec<String>,
    /// The directory the outputs are written below, each at its path.
    #[arg(long, default_value = ".")]
    pub download_to: PathBuf,
    /// A variable of the command's environment, `NAME=VALUE`. Repeated for each.
    #[arg(long = "env")]
    pub env: Vec<String>,
    /// The git program that lists the checkout's files.
    #[arg(long, default_value = "git")]
    pub git: PathBuf,
    /// The Cargo program that fetches the crates on this machine.
    #[arg(long = "cargo", default_value = "cargo")]
    pub cargo_program: PathBuf,
    /// The Cargo command to run on the Mac, after `--`: `build --locked --release`.
    #[arg(last = true, required = true)]
    pub cargo: Vec<String>,
}

/// Why the flags do not make a run.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    /// `--address` is empty.
    #[error("the farm is not configured: --address is empty (FARM_ADDRESS is not set)")]
    NoAddress,
    /// `--address` is not `grpc://<host>:<port>`.
    #[error("the address {0:?} is not grpc://<host>:<port>")]
    Address(String),
    /// `--tls` is neither `true` nor `false`.
    #[error("--tls is {0:?}: it must be `true` or `false`")]
    Tls(String),
    /// `--properties` is empty.
    #[error("the properties are empty: the platform needs at least one key=value")]
    NoProperties,
    /// An entry of `--properties` that is not `key=value`.
    #[error("the property {0:?} is not key=value with a key and a value")]
    Property(String),
    /// A key of `--properties` given twice.
    #[error("the property {0:?} is given twice")]
    DuplicateProperty(String),
    /// `--timeout-secs` is 0.
    #[error("--timeout-secs is 0")]
    NoTimeout,
    /// The Cargo command is empty or does not start with its subcommand.
    #[error(
        "the Cargo command is empty, or starts with an option ({0:?}): it starts with its subcommand"
    )]
    Cargo(String),
    /// An `--output` that is not a relative path of plain components, or is in [`OURS`].
    #[error("--output {0:?} is not a relative path of plain components outside {OURS}")]
    Output(String),
    /// An `--output` given twice.
    #[error("--output {0:?} is given twice")]
    OutputTwice(String),
    /// An `--env` that is not `NAME=VALUE` with a name.
    #[error("--env {0:?} is not NAME=VALUE with a name")]
    Env(String),
    /// An `--env` name the verb sets itself, or given twice.
    #[error("--env {0} is set by the verb or given twice")]
    EnvTaken(String),
}

/// The checked settings of a run.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    /// The URL of the farm, `http://` or `https://`.
    pub endpoint: String,
    /// Whether the channel is TLS.
    pub tls: bool,
    /// The instance name.
    pub instance: String,
    /// The platform properties, sorted by key.
    pub properties: Vec<(String, String)>,
    /// The action's timeout, in seconds.
    pub timeout_secs: u64,
    /// The Command's outputs, sorted.
    pub outputs: BTreeSet<String>,
    /// The Command's environment: the verb's own and each `--env`, by name.
    pub env: BTreeMap<String, String>,
    /// The Cargo arguments.
    pub cargo: Vec<String>,
}

/// The variables of the Command the verb sets itself.
#[must_use]
pub fn own_env() -> BTreeMap<String, String> {
    [
        ("CARGO_HOME", CARGO_HOME.to_owned()),
        ("CARGO_INCREMENTAL", "0".to_owned()),
        ("CARGO_NET_OFFLINE", "true".to_owned()),
        ("CARGO_TERM_COLOR", "always".to_owned()),
        ("PATH", format!("{TOOLCHAIN}/bin:{MAC_PATH}")),
        ("RUSTC", format!("{TOOLCHAIN}/bin/rustc")),
        ("RUSTDOC", format!("{TOOLCHAIN}/bin/rustdoc")),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

/// The pairs of `text`: `key=value` entries separated by commas, each part trimmed. No entry
/// may lack the `=`, have an empty key or value, or repeat a key; the list may not be empty.
///
/// # Errors
/// The [`Problem`] naming the first entry that is wrong.
pub fn parse_properties(text: &str) -> Result<Vec<(String, String)>, Problem> {
    if text.trim().is_empty() {
        return Err(Problem::NoProperties);
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        let (key, value) = match entry.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => (k.trim(), v.trim()),
            _ => return Err(Problem::Property(entry.to_owned())),
        };
        if out.iter().any(|(k, _)| k == key) {
            return Err(Problem::DuplicateProperty(key.to_owned()));
        }
        out.push((key.to_owned(), value.to_owned()));
    }
    out.sort();
    Ok(out)
}

impl Settings {
    /// The settings `args` name.
    ///
    /// # Errors
    /// The [`Problem`] of the first flag that is wrong.
    pub fn of(args: &Args) -> Result<Settings, Problem> {
        let address = args.address.trim();
        if address.is_empty() {
            return Err(Problem::NoAddress);
        }
        let host = address.strip_prefix("grpc://").unwrap_or_default();
        let port = host.rsplit_once(':').map(|(_, p)| p).unwrap_or_default();
        let plain = !host.contains('/') && !host.chars().any(char::is_whitespace);
        if !plain || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Problem::Address(address.to_owned()));
        }
        let tls = match args.tls.trim() {
            "true" => true,
            "false" => false,
            other => return Err(Problem::Tls(other.to_owned())),
        };
        let properties = parse_properties(&args.properties)?;
        if args.timeout_secs == 0 {
            return Err(Problem::NoTimeout);
        }
        match args.cargo.first() {
            Some(sub) if !sub.starts_with('-') && !sub.starts_with('+') => {}
            first => return Err(Problem::Cargo(first.cloned().unwrap_or_default())),
        }
        let mut outputs = BTreeSet::new();
        for path in &args.outputs {
            let ours = path == OURS || path.starts_with(&format!("{OURS}/"));
            if ours || tree::components(path).is_err() {
                return Err(Problem::Output(path.clone()));
            }
            if !outputs.insert(path.clone()) {
                return Err(Problem::OutputTwice(path.clone()));
            }
        }
        let mut env = own_env();
        for pair in &args.env {
            let Some((name, value)) = pair.split_once('=').filter(|(n, _)| !n.is_empty()) else {
                return Err(Problem::Env(pair.clone()));
            };
            if env.insert(name.to_owned(), value.to_owned()).is_some() {
                return Err(Problem::EnvTaken(name.to_owned()));
            }
        }
        let scheme = if tls { "https" } else { "http" };
        Ok(Settings {
            endpoint: format!("{scheme}://{host}"),
            tls,
            instance: args.instance.trim().to_owned(),
            properties,
            timeout_secs: args.timeout_secs,
            outputs,
            env,
            cargo: args.cargo.clone(),
        })
    }
}

/// Why a run failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A flag that is wrong.
    #[error(transparent)]
    Flags(#[from] Problem),
    /// A file or directory of this machine that could not be read or made, or a program that
    /// could not be started.
    #[error("cannot {what}: {why}")]
    Io {
        /// What was being done.
        what: String,
        /// Why it failed.
        why: String,
    },
    /// A program of this machine that failed.
    #[error("{what} failed: {stderr}")]
    Program {
        /// The program and its verb: `git ls-files`.
        what: String,
        /// What it printed on stderr.
        stderr: String,
    },
    /// The checkout tracks a path in [`OURS`].
    #[error("the checkout tracks {0}, where the run puts its toolchain and crates")]
    Tracked(String),
    /// A tracked path that is neither a file nor a symlink.
    #[error("{0} is neither a file nor a symlink")]
    NotAFile(String),
    /// The toolchain could not be made.
    #[error(transparent)]
    Toolchain(#[from] ToolchainError),
    /// The inputs make no tree.
    #[error(transparent)]
    Tree(#[from] TreeError),
    /// A call to the farm failed.
    #[error(transparent)]
    Farm(#[from] FarmError),
    /// The farm did not run the command to its end.
    #[error("the farm did not run `cargo {command}` to its end: {code:?}: {message}")]
    NotRun {
        /// The Cargo arguments, joined by spaces.
        command: String,
        /// The status code of the farm's answer.
        code: Code,
        /// Its message.
        message: String,
    },
    /// The farm answered OK with no result.
    #[error("the farm answered `cargo {0}` with no result")]
    NoResult(String),
    /// The command exited with a status other than 0.
    #[error("`cargo {command}` exited {code} on a farm Mac{}", if *cached { ", a cached result" } else { "" })]
    Exited {
        /// The Cargo arguments, joined by spaces.
        command: String,
        /// The exit code.
        code: i32,
        /// Whether the farm answered from its cache.
        cached: bool,
    },
    /// The outputs could not be fetched or written.
    #[error(transparent)]
    Output(#[from] OutputError),
}

/// A closure that makes an [`Error::Io`] of an I/O error while doing `what`.
fn io(what: String) -> impl FnOnce(std::io::Error) -> Error {
    move |e| Error::Io {
        what,
        why: e.to_string(),
    }
}

/// The Command: the toolchain's `cargo` with the run's arguments, from the input root, its
/// environment sorted by name and its outputs sorted. Its platform is the Action's
/// ([`action`]).
#[must_use]
pub fn command(s: &Settings) -> Command {
    Command {
        arguments: std::iter::once(format!("{TOOLCHAIN}/bin/cargo"))
            .chain(s.cargo.iter().cloned())
            .collect(),
        environment_variables: s
            .env
            .iter()
            .map(|(name, value)| EnvironmentVariable {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        output_paths: s.outputs.iter().cloned().collect(),
        ..Command::default()
    }
}

/// The Action of `command` over the input root `root`; its blobs go into `tree`.
pub fn action(s: &Settings, root: &Digest, tree: &mut Tree) -> Digest {
    let command = tree::add(&mut tree.blobs, command(s).encode_to_vec());
    let action = Action {
        command_digest: Some(command),
        input_root_digest: Some(root.clone()),
        timeout: Some(prost_types::Duration {
            seconds: i64::try_from(s.timeout_secs).unwrap_or(i64::MAX),
            nanos: 0,
        }),
        platform: Some(Platform {
            properties: s
                .properties
                .iter()
                .map(|(name, value)| Property {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        }),
        ..Action::default()
    };
    tree::add(&mut tree.blobs, action.encode_to_vec())
}

/// The entry of the file or symlink at `path`.
fn entry_at(path: &Path) -> Result<Entry, Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let what = || format!("read {}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(io(what()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(io(what()))?;
        return Ok(Entry::Symlink(target.to_string_lossy().into_owned()));
    }
    if !meta.is_file() {
        return Err(Error::NotAFile(path.display().to_string()));
    }
    let executable = meta.permissions().mode() & 0o111 != 0;
    let bytes = std::fs::read(path).map_err(io(what()))?;
    Ok(Entry::File { bytes, executable })
}

/// The entries of the checkout `root`: each of `tracked` (paths relative to it).
///
/// # Errors
/// [`Error`] for a path that cannot be read, or one in the directory the run's own inputs use.
pub fn checkout(root: &Path, tracked: &[String]) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    for path in tracked {
        if path == OURS || path.starts_with(&format!("{OURS}/")) {
            return Err(Error::Tracked(path.clone()));
        }
        out.push((path.clone(), entry_at(&root.join(path))?));
    }
    Ok(out)
}

/// The entries of the Cargo home `home`, below [`CARGO_HOME`], but the top entries of
/// [`HOME_SKIPPED`].
///
/// # Errors
/// [`Error`] for a directory or file that cannot be read.
pub fn cargo_home(home: &Path) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    walk(home, CARGO_HOME, true, &mut out)?;
    Ok(out)
}

/// The entries below `dir`, at `at` and below, into `out`; at the `top`, not those of
/// [`HOME_SKIPPED`].
fn walk(dir: &Path, at: &str, top: bool, out: &mut Vec<(String, Entry)>) -> Result<(), Error> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .and_then(|d| d.map(|e| e.map(|e| e.file_name())).collect())
        .map_err(io(format!("list {}", dir.display())))?;
    names.sort();
    for name in names {
        let name = name.to_string_lossy().into_owned();
        if top && HOME_SKIPPED.contains(&name.as_str()) {
            continue;
        }
        let path = dir.join(&name);
        let inside = format!("{at}/{name}");
        let meta =
            std::fs::symlink_metadata(&path).map_err(io(format!("read {}", path.display())))?;
        if meta.is_dir() {
            walk(&path, &inside, false, out)?;
        } else {
            out.push((inside, entry_at(&path)?));
        }
    }
    Ok(())
}

/// What a run takes from the machine it runs on.
pub struct Host<'a> {
    /// The paths git tracks in the checkout.
    pub tracked: &'a mut dyn FnMut() -> Result<Vec<String>, Error>,
    /// Fetches the crates of `Cargo.lock` for [`TARGET`] into the Cargo home it is given.
    pub fetch: &'a mut dyn FnMut(&Path) -> Result<(), Error>,
    /// The toolchain's pins ([`toolchain::PINS`]).
    pub pins: toolchain::Pins,
    /// Downloads a toolchain tarball.
    pub download: &'a mut toolchain::Download<'a>,
}

/// The input root of the run and its action: every blob of both, and the action's digest.
///
/// # Errors
/// [`Error`] for an input that cannot be gathered.
pub fn inputs(
    s: &Settings,
    root: &Path,
    scratch: &Path,
    host: &mut Host<'_>,
) -> Result<(Tree, Digest), Error> {
    let mut entries = checkout(root, &(host.tracked)()?)?;
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).map_err(io(format!("create {}", home.display())))?;
    (host.fetch)(&home)?;
    entries.extend(cargo_home(&home)?);
    entries.extend(toolchain::entries(&host.pins, TOOLCHAIN, host.download)?);
    let mut tree = tree::build(entries)?;
    let root = tree.root.clone();
    let action = action(s, &root, &mut tree);
    Ok((tree, action))
}

/// What the farm answered, printed into `out` and `err`; the verb's lines, or its failure. The
/// outputs of a command that passed are written below `into`.
///
/// # Errors
/// [`Error`] for a status that is not OK, no result, a non-zero exit, or an output that cannot
/// be fetched or written.
pub async fn report(
    farm: &Farm,
    s: &Settings,
    response: ExecuteResponse,
    into: &Path,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Vec<String>, Error> {
    let shown = s.cargo.join(" ");
    if let Some(result) = &response.result {
        // A farm may send a small output inline (`stdout_raw`), a copy of the blob its digest
        // names: the inline bytes when there are any, else the blob.
        for (raw, digest, to) in [
            (
                &result.stdout_raw,
                &result.stdout_digest,
                &mut *out as &mut dyn Write,
            ),
            (
                &result.stderr_raw,
                &result.stderr_digest,
                &mut *err as &mut dyn Write,
            ),
        ] {
            let bytes = match digest {
                _ if !raw.is_empty() => raw.clone(),
                Some(digest) => farm.read(digest).await?,
                None => Vec::new(),
            };
            // A log line that cannot be written must not hide the verdict.
            let _ = to.write_all(&bytes);
        }
    }
    let status = response.status.unwrap_or_default();
    if status.code != 0 {
        let _ = err.write_all(tail(&status).as_bytes());
        return Err(Error::NotRun {
            command: shown,
            code: Code::from(status.code),
            message: status.message,
        });
    }
    let Some(result) = response.result else {
        return Err(Error::NoResult(shown));
    };
    let cached = response.cached_result;
    if result.exit_code != 0 {
        return Err(Error::Exited {
            command: shown,
            code: result.exit_code,
            cached,
        });
    }
    let mut lines = vec![format!(
        "ok: `cargo {shown}` passed on a farm Mac{}",
        if cached { ", a cached result" } else { "" }
    )];
    if !s.outputs.is_empty() {
        let (files, bytes) = outputs::fetch(farm, &s.outputs, &result, into).await?;
        lines.push(format!(
            "mac-farm: fetched {files} files ({bytes} bytes) of {} into {}",
            s.outputs.iter().cloned().collect::<Vec<_>>().join(", "),
            into.display()
        ));
    }
    Ok(lines)
}

/// The stdout and stderr tails of a failed job the farm attached to `status`, as text.
fn tail(status: &rpc::Status) -> String {
    let mut text = String::new();
    for detail in &status.details {
        let Ok(info) = rpc::ErrorInfo::decode(detail.value.as_slice()) else {
            continue;
        };
        if info.reason != OUTPUT_TAIL {
            continue;
        }
        for key in ["stdout", "stderr"] {
            if let Some(value) = info.metadata.get(key) {
                text.push_str(&format!("--- the job's {key} (its end) ---\n{value}\n"));
            }
        }
    }
    text
}

/// The line a stage change prints.
#[must_use]
pub fn progress_line(progress: Progress) -> &'static str {
    match progress {
        Progress::Queued => "mac-farm: queued: waiting for a free Mac",
        Progress::Executing => "mac-farm: running on a farm Mac",
    }
}

/// The run against the farm: upload what it lacks, execute, report.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 and outputs that were written.
pub async fn remote(
    s: &Settings,
    tree: &Tree,
    action: &Digest,
    into: &Path,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Vec<String>, Error> {
    let farm = Farm::connect(&s.endpoint, s.tls, &s.instance).await?;
    let digests: Vec<Digest> = tree
        .blobs
        .iter()
        .map(|(hash, bytes)| Digest {
            hash: hash.clone(),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        })
        .collect();
    let missing = farm.missing(&digests).await?;
    let bytes: i64 = missing.iter().map(|d| d.size_bytes).sum();
    let _ = writeln!(
        err,
        "mac-farm: {} blobs, {} missing from the farm ({bytes} bytes to upload)",
        digests.len(),
        missing.len()
    );
    farm.upload(&tree.blobs, &missing).await?;
    let response = farm
        .execute(action, &mut |p| {
            eprintln!("{}", progress_line(p));
        })
        .await?;
    report(&farm, s, response, into, out, err).await
}

/// The verb with the machine's inputs given; the command's output is written to `out` and
/// `err`.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac and outputs that were
/// written.
pub fn run_with(
    args: &Args,
    root: &Path,
    host: &mut Host<'_>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Vec<String>, Error> {
    // The logs of a public repository's runs are public: keep the farm's address out of what
    // follows, the refusal of a wrong address included.
    let address = args.address.trim();
    let _ = writeln!(out, "::add-mask::{address}");
    let _ = writeln!(out, "::add-mask::{}", address.trim_start_matches("grpc://"));
    let settings = Settings::of(args)?;
    let scratch = std::path::absolute(&args.scratch)
        .map_err(io(format!("resolve {}", args.scratch.display())))?;
    let (tree, action) = inputs(&settings, root, &scratch, host)?;
    let into = root.join(&args.download_to);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io("start the runtime".to_owned()))?;
    runtime.block_on(remote(&settings, &tree, &action, &into, out, err))
}

/// The verb: the lines to print.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac and outputs that were
/// written.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let root = std::env::current_dir().map_err(io("read the working directory".to_owned()))?;
    run_in(args, &root, toolchain::parse(toolchain::PINS)?, &mut https)
}

/// The verb in the checkout `root`, with the toolchain `pins` downloaded by `download`: git and
/// Cargo are `--git` and `--cargo`, and the command's output goes to this process's stdout and
/// stderr.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac and outputs that were
/// written.
pub fn run_in(
    args: &Args,
    root: &Path,
    pins: toolchain::Pins,
    download: &mut toolchain::Download<'_>,
) -> Result<Vec<String>, Error> {
    let mut tracked = || git_files(&args.git, root);
    let mut fetch = |home: &Path| fetch_crates(&args.cargo_program, root, home);
    let mut host = Host {
        tracked: &mut tracked,
        fetch: &mut fetch,
        pins,
        download,
    };
    run_with(
        args,
        root,
        &mut host,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// The bytes at the HTTPS `url`, trusting the system's roots.
///
/// # Errors
/// Why the download failed, as text.
pub fn https(url: &str) -> Result<Vec<u8>, String> {
    crate::fetch::download(url, &crate::fetch::host_roots()).map_err(|e| e.to_string())
}

/// The stdout of `program`, run as `what`, when it succeeded.
///
/// # Errors
/// [`Error::Io`] when it cannot be started, [`Error::Program`] naming `what` and its stderr
/// when it fails.
pub fn output_of(what: &str, program: &mut Process) -> Result<String, Error> {
    let ran = program.output().map_err(io(format!("run {what}")))?;
    if !ran.status.success() {
        return Err(Error::Program {
            what: what.to_owned(),
            stderr: String::from_utf8_lossy(&ran.stderr).trim_end().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&ran.stdout).into_owned())
}

/// The paths `<git> ls-files -z` lists in `root`.
///
/// # Errors
/// [`Error`] when git cannot be started or fails.
pub fn git_files(git: &Path, root: &Path) -> Result<Vec<String>, Error> {
    let listed = output_of(
        "git ls-files",
        Process::new(git).args(["ls-files", "-z"]).current_dir(root),
    )?;
    Ok(split_nul(&listed))
}

/// The entries of a NUL-separated list.
#[must_use]
pub fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `<cargo> fetch` of the crates of `root`'s lock file for [`TARGET`] into the Cargo home
/// `home`.
///
/// # Errors
/// [`Error`] when Cargo cannot be started or fails.
pub fn fetch_crates(cargo: &Path, root: &Path, home: &Path) -> Result<(), Error> {
    output_of(
        "cargo fetch",
        Process::new(cargo)
            .args(["fetch", "--locked", "--target", TARGET])
            .env("CARGO_HOME", home)
            .current_dir(root),
    )
    .map(drop)
}
