//! The command line of the hook:
//! `js-lease-server update-hook [--name=value ...] <step> [<artifact>] --params <file>`.
//!
//! The updater runs `<executable> <hook_args> <step> [args]`. This module parses what follows
//! the verb: the flags the declaration's `hook_args` put before the step ([`Flags`]), the step,
//! the staged release directory (`prepare` only) and the file of the `[hook.params]` table,
//! which every step needs. Flags only: no value comes from the environment.
//!
//! The flags carry the text values the hook needs, because the host updater refuses a string in
//! the `[hook.params]` of a hook that runs as root:
//!
//! - `--release-link`: the symbolic link the server's unit runs its program through, an
//!   absolute path;
//! - `--unit`: the unit that runs the server, [`DEFAULT_UNIT`] when absent;
//! - `--store`: the file that holds the store's connection, the one the server's own `--store`
//!   names, an absolute path;
//! - `--healthz-url`: the server's own address, `http://` and a loopback address and port
//!   (`http://127.0.0.1:7878`; `http://[::1]:7878` is read too, but the host updater refuses
//!   a `hook_args` word with brackets, so a declaration names an IPv4 loopback address). Only
//!   plain HTTP to the loopback: a server
//!   that serves TLS cannot be checked this way.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use js_update_hook::{Step, UnknownStep};

/// `--release-link`.
pub const RELEASE_LINK_FLAG: &str = "--release-link";
/// `--unit`.
pub const UNIT_FLAG: &str = "--unit";
/// `--store`.
pub const STORE_FLAG: &str = "--store";
/// `--healthz-url`.
pub const HEALTHZ_URL_FLAG: &str = "--healthz-url";

/// The unit `--unit` names when it is absent.
pub const DEFAULT_UNIT: &str = "js-lease-server.service";

/// Why a command line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    /// Nothing follows the verb.
    #[error("no step given")]
    MissingStep,
    /// The first word is not one of the five steps.
    #[error("unknown step {0:?}")]
    UnknownStep(String),
    /// `prepare` without the staged release directory.
    #[error("prepare needs the staged release directory")]
    MissingArtifact,
    /// An argument the step does not take.
    #[error("unexpected argument {0:?}")]
    Unexpected(OsString),
    /// `--params` is the last word.
    #[error("--params needs a file")]
    MissingParamsFile,
    /// `--params` twice.
    #[error("--params given twice")]
    DuplicateParams,
    /// No `--params`.
    #[error("--params <file> is required")]
    MissingParams,
    /// A word before the step that is not one of the [`Flags`].
    #[error("unknown flag {0:?} before the step")]
    UnknownFlag(String),
    /// A flag before the step without `=value`, or with an empty value.
    #[error("{0} needs a value, written {0}=<value>")]
    FlagValue(&'static str),
    /// A flag before the step given twice.
    #[error("{0} given twice")]
    DuplicateFlag(&'static str),
    /// A flag that names a path was given one that is not absolute.
    #[error("{0} must be an absolute path")]
    RelativeFlag(&'static str),
    /// `--unit` is not a unit name: it holds a `/` or starts with `-`.
    #[error("--unit {0:?} is not a unit name")]
    BadUnit(String),
    /// `--healthz-url` is not `http://` and a loopback address with a port.
    #[error(
        "--healthz-url {0:?} must be http:// and a loopback address and port, such as http://127.0.0.1:7878"
    )]
    BadHealthzUrl(String),
}

/// The values given as flags before the step. Each is `None` when its flag is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `--release-link`, an absolute path.
    pub release_link: Option<PathBuf>,
    /// `--unit`.
    pub unit: Option<String>,
    /// `--store`, an absolute path.
    pub store: Option<PathBuf>,
    /// `--healthz-url`: the loopback address it names.
    pub healthz: Option<SocketAddr>,
}

/// Puts `value` in `slot`, which must be empty.
fn fill<T>(slot: &mut Option<T>, name: &'static str, value: T) -> Result<(), ArgsError> {
    match slot.replace(value) {
        Some(_) => Err(ArgsError::DuplicateFlag(name)),
        None => Ok(()),
    }
}

/// `value` as an absolute path.
fn absolute(name: &'static str, value: &str) -> Result<PathBuf, ArgsError> {
    let path = PathBuf::from(value);
    match path.is_absolute() {
        true => Ok(path),
        false => Err(ArgsError::RelativeFlag(name)),
    }
}

/// `value` as a unit name: no `/`, and not read as an option.
fn unit(value: &str) -> Result<String, ArgsError> {
    match value.contains('/') || value.starts_with('-') {
        true => Err(ArgsError::BadUnit(value.to_owned())),
        false => Ok(value.to_owned()),
    }
}

/// `value` as `http://` and a loopback address with a port that is not 0, nothing after it.
///
/// # Errors
/// [`ArgsError::BadHealthzUrl`] for anything else.
pub fn healthz_address(value: &str) -> Result<SocketAddr, ArgsError> {
    value
        .strip_prefix("http://")
        .and_then(|rest| rest.parse::<SocketAddr>().ok())
        .filter(|addr| addr.ip().is_loopback() && addr.port() != 0)
        .ok_or_else(|| ArgsError::BadHealthzUrl(value.to_owned()))
}

impl Flags {
    /// Takes one word that comes before the step: `--name=value`, a name above and a value
    /// that is not empty.
    ///
    /// # Errors
    /// [`ArgsError::UnknownFlag`] for another name, [`ArgsError::FlagValue`] for a flag with no
    /// `=value` or an empty one, [`ArgsError::DuplicateFlag`] for a flag given twice, and the
    /// refusal of the value's own shape.
    pub fn take(&mut self, word: &str) -> Result<(), ArgsError> {
        let (name, value) = word.split_once('=').unwrap_or((word, ""));
        let name = [RELEASE_LINK_FLAG, UNIT_FLAG, STORE_FLAG, HEALTHZ_URL_FLAG]
            .into_iter()
            .find(|flag| *flag == name)
            .ok_or_else(|| ArgsError::UnknownFlag(word.to_owned()))?;
        if value.is_empty() {
            return Err(ArgsError::FlagValue(name));
        }
        match name {
            RELEASE_LINK_FLAG => fill(&mut self.release_link, name, absolute(name, value)?),
            UNIT_FLAG => fill(&mut self.unit, name, unit(value)?),
            STORE_FLAG => fill(&mut self.store, name, absolute(name, value)?),
            _ => fill(&mut self.healthz, name, healthz_address(value)?),
        }
    }

    /// The unit: `--unit`, or [`DEFAULT_UNIT`].
    #[must_use]
    pub fn unit(&self) -> &str {
        self.unit.as_deref().unwrap_or(DEFAULT_UNIT)
    }
}

/// The step a hook was asked to run, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The flags before the step.
    pub flags: Flags,
    /// The step.
    pub step: Step,
    /// The staged release directory: given to `prepare` and to no other step.
    pub artifact: Option<PathBuf>,
    /// The file of the `[hook.params]` table.
    pub params: PathBuf,
}

impl Invocation {
    /// Parses the arguments that follow the verb: the flags, then the step.
    ///
    /// # Errors
    /// [`ArgsError`] for a flag that is unknown, repeated or without a value, an unknown step,
    /// a missing or extra argument, or a missing `--params`.
    pub fn parse<I>(args: I) -> Result<Invocation, ArgsError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut args = args.into_iter();
        let mut flags = Flags::default();
        let first = loop {
            let word = args.next().ok_or(ArgsError::MissingStep)?;
            let text = word.to_string_lossy().into_owned();
            if !text.starts_with("--") {
                break text;
            }
            // A value is taken as the bytes given or not at all, never changed to fit text.
            if word.to_str().is_none() {
                return Err(ArgsError::UnknownFlag(text));
            }
            flags.take(&text)?;
        };
        let step: Step = first
            .parse()
            .map_err(|UnknownStep(word)| ArgsError::UnknownStep(word))?;
        let (mut artifact, mut params) = (None, None);
        while let Some(arg) = args.next() {
            if arg == "--params" {
                let file = args.next().ok_or(ArgsError::MissingParamsFile)?;
                if params.replace(PathBuf::from(file)).is_some() {
                    return Err(ArgsError::DuplicateParams);
                }
            } else if step == Step::Prepare && artifact.is_none() {
                artifact = Some(PathBuf::from(arg));
            } else {
                return Err(ArgsError::Unexpected(arg));
            }
        }
        if step == Step::Prepare && artifact.is_none() {
            return Err(ArgsError::MissingArtifact);
        }
        let params = params.ok_or(ArgsError::MissingParams)?;
        Ok(Invocation {
            flags,
            step,
            artifact,
            params,
        })
    }
}
