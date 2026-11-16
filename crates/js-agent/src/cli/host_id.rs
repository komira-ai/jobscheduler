//! The flags that name the id the host registers under, and the reader of
//! `--host-id-from-facts`: the id hostkeeper's daemon wrote to its facts file.
//!
//! The facts file is one JSON object, written whole by rename, such as
//! `{"schema":1,"host_id":"web-04","host_group":"web"}`. It is trusted like a flags file: a
//! regular file (a link is followed), owned by root or the user running the agent, not
//! writable by group or others, at most [`MAX_BYTES`] of UTF-8. `schema` must be the integer
//! [`SCHEMA`]; fields this agent does not know are ignored. `host_id` must be an id the hello
//! accepts ([`js_types::HostId`]). A file that cannot be used is an error, never a silent
//! fall back to the machine-id default: the host would then register under an id other than
//! the one asked for.

use std::io;
use std::path::{Path, PathBuf};

use clap::Args;
use js_types::HostId;
use serde_json::Value;

use super::Cli;
use crate::flags_file::{FileFacts, MAX_BYTES};

/// The facts file schema this agent reads.
pub const SCHEMA: u64 = 1;

/// The flags that choose the host id; at most one may be given.
#[derive(Debug, Args)]
pub struct HostIdFlags {
    /// The id this host registers under. By default a salted hash of the machine id
    /// (`/etc/machine-id`, or the platform UUID on a Mac), never the machine id itself. With
    /// `--runtime crun` one agent may hold a host id: the claim is a lock file in the
    /// directory above `--scratch-root`, so agents that share a host id must share that
    /// directory to be refused.
    #[arg(long, value_name = "ID", conflicts_with = "host_id_from_facts")]
    pub host_id: Option<String>,
    /// A facts file of hostkeeper's daemon (`/var/lib/hostkeeper/facts.json`): the id this
    /// host registers under is its `host_id`. Read when the agent starts; a file that cannot
    /// be read or used stops the agent.
    #[arg(long, value_name = "PATH")]
    pub host_id_from_facts: Option<PathBuf>,
}

/// Why a facts file was refused. The text names the flag and the file, and says why.
#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    /// The file cannot be opened or read.
    #[error("--host-id-from-facts {path}: cannot be read: {source}")]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: io::Error,
    },
    /// The file, after following links, is not a regular file.
    #[error("--host-id-from-facts {path}: not a regular file")]
    NotRegular {
        /// The file.
        path: PathBuf,
    },
    /// Group or others can write the file.
    #[error("--host-id-from-facts {path}: writable by group or others (mode {mode:04o})")]
    Writable {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The file is owned by neither root nor the user running the agent.
    #[error("--host-id-from-facts {path}: owned by uid {uid}, not root or the agent's user")]
    Owner {
        /// The file.
        path: PathBuf,
        /// Its owner.
        uid: u32,
    },
    /// The file is longer than [`MAX_BYTES`].
    #[error("--host-id-from-facts {path}: larger than {MAX_BYTES} bytes")]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The file is not UTF-8.
    #[error("--host-id-from-facts {path}: not UTF-8")]
    NotUtf8 {
        /// The file.
        path: PathBuf,
    },
    /// The file is not JSON.
    #[error("--host-id-from-facts {path}: not JSON: {reason}")]
    NotJson {
        /// The file.
        path: PathBuf,
        /// The parser's reason.
        reason: String,
    },
    /// The file is JSON, and not an object.
    #[error("--host-id-from-facts {path}: not a JSON object")]
    NotObject {
        /// The file.
        path: PathBuf,
    },
    /// The object has no `schema` that is an integer.
    #[error("--host-id-from-facts {path}: schema missing or not an integer")]
    SchemaMissing {
        /// The file.
        path: PathBuf,
    },
    /// The `schema` is an integer other than [`SCHEMA`].
    #[error("--host-id-from-facts {path}: schema {found}, this agent reads {SCHEMA}")]
    SchemaRefused {
        /// The file.
        path: PathBuf,
        /// The schema the file names.
        found: u64,
    },
    /// The object has no `host_id`.
    #[error("--host-id-from-facts {path}: host_id missing")]
    HostIdMissing {
        /// The file.
        path: PathBuf,
    },
    /// The `host_id` is not a string.
    #[error("--host-id-from-facts {path}: host_id is not a string")]
    HostIdNotString {
        /// The file.
        path: PathBuf,
    },
    /// The `host_id` is not an id the hello accepts.
    #[error("--host-id-from-facts {path}: host_id: {reason}")]
    HostIdRefused {
        /// The file.
        path: PathBuf,
        /// Why the id is refused.
        reason: String,
    },
}

/// The host id the flags of `cli` name: `--host-id`, or the `host_id` of the facts file of
/// `--host-id-from-facts` (read with `read` as the file's owner `euid` judges it), or none
/// when neither is given and the id is the default derived from the machine id.
///
/// # Errors
/// The facts file cannot be used (see [`FactsError`]).
pub fn resolve_host_id(
    cli: &Cli,
    euid: u32,
    read: &dyn Fn(&Path) -> io::Result<FileFacts>,
) -> Result<Option<String>, FactsError> {
    match &cli.host.host_id_from_facts {
        Some(path) => read_facts(path, euid, read).map(Some),
        None => Ok(cli.host.host_id.clone()),
    }
}

/// The `host_id` of the facts file at `path`.
fn read_facts(
    path: &Path,
    euid: u32,
    read: &dyn Fn(&Path) -> io::Result<FileFacts>,
) -> Result<String, FactsError> {
    let at = || path.to_path_buf();
    let facts = read(path).map_err(|source| FactsError::Unreadable { path: at(), source })?;
    if !facts.regular {
        return Err(FactsError::NotRegular { path: at() });
    }
    if facts.mode & 0o022 != 0 {
        return Err(FactsError::Writable {
            path: at(),
            mode: facts.mode,
        });
    }
    if facts.uid != euid && facts.uid != 0 {
        return Err(FactsError::Owner {
            path: at(),
            uid: facts.uid,
        });
    }
    if facts.bytes.len() as u64 > MAX_BYTES {
        return Err(FactsError::TooLarge { path: at() });
    }
    let text = std::str::from_utf8(&facts.bytes).map_err(|_| FactsError::NotUtf8 { path: at() })?;
    let value: Value = serde_json::from_str(text).map_err(|why| FactsError::NotJson {
        path: at(),
        reason: why.to_string(),
    })?;
    let Value::Object(object) = value else {
        return Err(FactsError::NotObject { path: at() });
    };
    let schema = object
        .get("schema")
        .and_then(Value::as_u64)
        .ok_or_else(|| FactsError::SchemaMissing { path: at() })?;
    if schema != SCHEMA {
        return Err(FactsError::SchemaRefused {
            path: at(),
            found: schema,
        });
    }
    let id = match object.get("host_id") {
        None => return Err(FactsError::HostIdMissing { path: at() }),
        Some(Value::String(id)) => id,
        Some(_) => return Err(FactsError::HostIdNotString { path: at() }),
    };
    HostId::new(id).map_err(|why| FactsError::HostIdRefused {
        path: at(),
        reason: why.to_string(),
    })?;
    Ok(id.clone())
}
