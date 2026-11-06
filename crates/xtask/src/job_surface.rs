//! `gen-job-surface --out <dir> [--descriptor <path>] [--check]`: the job commands of `js` and
//! the methods of `js_client::jobs::JobClient`, generated from the job service's descriptor
//! set, so that a call added to the definition appears in both with no hand-written code
//! (design section 3.6), and the generated blocks of the gateway's page. `docs/job-surface.md`
//! says how to add a call.
//!
//! [`generate`] reads the descriptor set into a typed surface (per call its name, its request
//! with every field and the field's comment, its response, whether the response streams,
//! the idempotency level) and renders two files from it and from nothing else:
//!
//! - [`CLIENT_FILE`]: one method of `JobClient` per call, taking and returning the
//!   messages of `js-proto`, and the reader of a streamed answer when a call streams.
//! - [`CLI_FILE`]: `SURFACE`, the table of commands (the words of each command, one flag per
//!   request field, the call), and `dispatch`, which sends a request through the client.
//!   The interpreter in `js-cli` builds the commands from the table and names no call.
//!
//! Both files are checked in, so a build needs no protobuf compiler and the generated code
//! is reviewed with the change that makes it. Without `--descriptor` the verb reads the set
//! `js-proto` builds ([`js_proto::FILE_DESCRIPTOR_SET`]).
//!
//! A third file, [`DOC_FILE`], is hand-written but for two blocks between marker lines
//! (`docs`, private): the route table, from the gateway's routes (`js_api_gateway::ROUTES`,
//! which the gateway's build reads from the `google.api.http` options of js-proto's
//! descriptor set), and the paragraph that says which calls the server serves, from
//! `js_lease_server::SERVED_CALLS`. Neither depends on `--descriptor`: the page is the job
//! service's. The verb replaces what stands between the markers of the page under `--out`
//! and nothing else.
//!
//! The verb writes the files under `--out` (the repository root), or with `--check` renders
//! them in memory and fails naming each checked-in file that differs (the page also when it
//! is missing or its markers are not each there once, begin before end); the test
//! `generated_files_are_current` runs it so.
//!
//! How a call becomes a command and a field a flag is in the module docs of `cli` (private)
//! and, for users, in the crate docs of js-cli.

use std::path::{Path, PathBuf};

use prost::Message as _;
use prost_types::FileDescriptorSet;

mod cli;
mod client;
mod docs;
mod surface;
mod words;

#[cfg(test)]
mod tests;

/// The generated client methods, from the repository root.
pub const CLIENT_FILE: &str = "crates/js-client/src/jobs/generated.rs";

/// The generated command table and dispatcher, from the repository root.
pub const CLI_FILE: &str = "crates/js-cli/src/jobs/generated.rs";

/// The gateway's page, from the repository root: hand-written but for its generated blocks.
pub const DOC_FILE: &str = "docs/http-gateway.md";

/// Why the surface could not be generated, or the checked-in files are not current.
#[derive(Debug, thiserror::Error)]
pub enum GenError {
    /// The descriptor set file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The bytes are not an encoded `FileDescriptorSet`.
    #[error("the descriptor set does not decode: {0}")]
    Decode(#[from] prost::DecodeError),
    /// The set does not hold exactly one service with calls.
    #[error("the descriptor set must hold one service with calls, not {0}")]
    Services(usize),
    /// A type name names no message or enum of the set.
    #[error("the descriptor set has no type {0}")]
    Unresolved(String),
    /// An entry of `surface::RETRY_KEYS` names a call or a field the definition does not
    /// have.
    #[error("{rpc} {why}, which the generator's retry-key entry names")]
    RetryKey {
        /// The call, with its service.
        rpc: String,
        /// What is missing.
        why: String,
    },
    /// A call's request or response is a message of another package, which the client
    /// cannot name: js-proto exposes the messages of the service's own package.
    #[error("{rpc}: {message} is not in the service's package")]
    Foreign {
        /// The call.
        rpc: String,
        /// The message's full name.
        message: String,
    },
    /// A call streams its requests, which a command line cannot send.
    #[error("{0} streams its requests; a generated command sends one request")]
    ClientStreaming(String),
    /// A call's command has no words of its own, or lands where another command is.
    #[error("{rpc}: {why}")]
    Path {
        /// The call.
        rpc: String,
        /// What is wrong with its command.
        why: String,
    },
    /// A call's request has a field whose flag is taken.
    #[error("{rpc}: the flag --{flag} {why}")]
    Flag {
        /// The call.
        rpc: String,
        /// The flag, without its dashes.
        flag: String,
        /// Who has it.
        why: String,
    },
    /// A generated file could not be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The markers of a generated block of [`DOC_FILE`] are missing, doubled or out of
    /// order.
    #[error("{}: {why}", path.display())]
    Markers {
        /// The page.
        path: PathBuf,
        /// What is wrong with its markers.
        why: String,
    },
    /// `--check`: these checked-in files differ from what the generator renders.
    #[error(
        "not what `cargo xtask gen-job-surface --out .` renders from the job service: {}",
        .0.join(", ")
    )]
    Stale(Vec<String>),
}

/// The two generated files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// The text of [`CLIENT_FILE`].
    pub client: String,
    /// The text of [`CLI_FILE`].
    pub cli: String,
}

/// Renders both files from the encoded `FileDescriptorSet` `descriptor`.
///
/// # Errors
///
/// [`GenError::Decode`] for bytes that are not a descriptor set; the errors of reading the
/// surface (no single service with calls, an unresolved type, a request that streams, a message of
/// another package, a retry-key entry that no longer matches); and those of the command
/// table (a command without words, on a taken word or where another is; a reserved or
/// doubled flag).
pub fn generate(descriptor: &[u8]) -> Result<Generated, GenError> {
    let set = FileDescriptorSet::decode(descriptor)?;
    let surface = surface::read(&set)?;
    let table = cli::table(&surface)?;
    Ok(Generated {
        client: client::render(&surface),
        cli: cli::render(&surface, &table),
    })
}

/// The flags of `gen-job-surface`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The encoded `FileDescriptorSet` to read (default: the set js-proto builds from its
    /// `.proto` files).
    #[arg(long)]
    pub descriptor: Option<PathBuf>,
    /// The repository root the files are written under.
    #[arg(long)]
    pub out: PathBuf,
    /// Write nothing: fail when a checked-in file differs from what the generator renders.
    #[arg(long)]
    pub check: bool,
}

/// Runs the verb: the line to print when it passed.
///
/// # Errors
///
/// [`GenError::Read`] for a `--descriptor` that cannot be read, the errors of
/// [`generate`], [`GenError::Write`] for a file that cannot be written, [`GenError::Read`]
/// and [`GenError::Markers`] for a page under `--out` that cannot be read or whose markers
/// are wrong (in writing) and, with `--check`, [`GenError::Stale`].
pub fn run(args: &Args) -> Result<String, GenError> {
    let descriptor = match &args.descriptor {
        Some(path) => std::fs::read(path).map_err(|source| GenError::Read {
            path: path.clone(),
            source,
        })?,
        None => js_proto::FILE_DESCRIPTOR_SET.to_vec(),
    };
    let generated = generate(&descriptor)?;
    let files = [(CLIENT_FILE, generated.client), (CLI_FILE, generated.cli)];
    match args.check {
        true => check(&args.out, &files),
        false => write(&args.out, &files),
    }
}

/// Fails naming each file under `root` whose text is not the rendered one, then the page
/// when its generated blocks are not the rendered ones or it cannot be rendered.
fn check(root: &Path, files: &[(&str, String)]) -> Result<String, GenError> {
    let mut stale: Vec<String> = files
        .iter()
        .filter(|(path, text)| std::fs::read_to_string(root.join(path)).ok().as_ref() != Some(text))
        .map(|(path, _)| (*path).to_owned())
        .collect();
    let page = std::fs::read_to_string(root.join(DOC_FILE)).ok();
    let rendered = page.as_deref().and_then(|text| docs::page(text).ok());
    let current = page.as_ref().zip(rendered.as_ref());
    if current.is_none_or(|(page, rendered)| page != rendered) {
        stale.push(DOC_FILE.to_owned());
    }
    match stale.is_empty() {
        true => Ok(format!(
            "{CLIENT_FILE}, {CLI_FILE} and {DOC_FILE} are current"
        )),
        false => Err(GenError::Stale(stale)),
    }
}

/// Writes each file under `root`, making its directory when it is missing, then the
/// generated blocks of the page under `root`.
fn write(root: &Path, files: &[(&str, String)]) -> Result<String, GenError> {
    let written = |path: &Path, text: &str| {
        let failed = |source| GenError::Write {
            path: path.to_path_buf(),
            source,
        };
        let parent = path.parent().unwrap_or(root);
        std::fs::create_dir_all(parent).map_err(failed)?;
        std::fs::write(path, text).map_err(failed)
    };
    files
        .iter()
        .try_for_each(|(path, text)| written(&root.join(path), text))?;
    let page = root.join(DOC_FILE);
    let text = std::fs::read_to_string(&page).map_err(|source| GenError::Read {
        path: page.clone(),
        source,
    })?;
    let text = docs::page(&text).map_err(|why| GenError::Markers {
        path: page.clone(),
        why,
    })?;
    written(&page, &text)?;
    Ok(format!(
        "wrote {CLIENT_FILE} and {CLI_FILE}, and the generated blocks of {DOC_FILE}"
    ))
}
