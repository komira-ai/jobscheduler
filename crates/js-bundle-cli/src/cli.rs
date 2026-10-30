//! The command line: flags only, no environment variables and no config file.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use js_bundle::{EnvOp, NetworkLevel, ReadGrantKind};
use serde::Deserialize as _;
use serde::de::value::{Error as ValueError, StrDeserializer};

/// `js-bundle`: pack a directory into a darwin bundle, print one, or verify one.
///
/// A bundle lives in an OCI image-layout directory (`oci-layout`, `index.json`,
/// `blobs/sha256/`). Pushing to a registry is not a command of this tool.
#[derive(Debug, Parser)]
#[command(name = "js-bundle", version)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Packs directories into a bundle in a new layout directory and prints the manifest
    /// digest.
    Pack(PackArgs),
    /// Prints a bundle's manifest and config.
    Inspect(LayoutArgs),
    /// Checks a bundle: the manifest, every blob's digest and length, the media types,
    /// the config, and every layer extracted into a scratch directory under the
    /// format's rules.
    Verify(VerifyArgs),
}

/// A `NAME=VALUE` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    /// The text before the first `=`.
    pub key: String,
    /// The text after it.
    pub value: String,
}

/// An `--env OP:NAME=VALUE` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFlag {
    /// How the rule combines (`set`, `prepend`, `append`, `default`).
    pub op: EnvOp,
    /// The variable.
    pub name: String,
    /// One value, a placeholder string.
    pub value: String,
}

const PAIR_SHAPE: &str = "expected NAME=VALUE";
const ENV_SHAPE: &str = "expected OP:NAME=VALUE";

fn pair(text: &str) -> Result<Pair, &'static str> {
    let (key, value) = text.split_once('=').ok_or(PAIR_SHAPE)?;
    Ok(Pair {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

fn env_flag(text: &str) -> Result<EnvFlag, String> {
    let (head, value) = text.split_once('=').ok_or(ENV_SHAPE)?;
    let (op, name) = head.split_once(':').ok_or(ENV_SHAPE)?;
    let op = EnvOp::deserialize(StrDeserializer::<ValueError>::new(op))
        .map_err(|error| error.to_string())?;
    Ok(EnvFlag {
        op,
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

fn read_grant(text: &str) -> Result<ReadGrantKind, String> {
    ReadGrantKind::deserialize(StrDeserializer::<ValueError>::new(text))
        .map_err(|error| error.to_string())
}

fn network(text: &str) -> Result<NetworkLevel, String> {
    NetworkLevel::deserialize(StrDeserializer::<ValueError>::new(text))
        .map_err(|error| error.to_string())
}

/// The flags of `pack`. A bundle is made of trees, each from a directory; the order of
/// the `--tree` flags is the order of the layers.
#[derive(Debug, Args)]
pub struct PackArgs {
    /// The layout directory to make. It must not exist; its parent must.
    #[arg(long)]
    pub out: PathBuf,
    /// The bundle's name: lowercase letters, digits, `.`, `_` and `-`.
    #[arg(long)]
    pub name: String,
    /// The bundle's version.
    #[arg(long = "bundle-version")]
    pub bundle_version: String,
    /// The lowest macOS version the bundle runs on, `major.minor[.patch]`.
    #[arg(long = "os-version")]
    pub os_version: String,
    /// A tree: its name and the directory it is packed from. Repeat for more trees.
    #[arg(long = "tree", value_name = "NAME=DIR", value_parser = pair)]
    pub trees: Vec<Pair>,
    /// The SPDX licence expression of a tree. Every tree needs one.
    #[arg(long = "license", value_name = "NAME=SPDX", value_parser = pair)]
    pub licenses: Vec<Pair>,
    /// The https URL of the source of a tree. Every tree needs one.
    #[arg(long = "source-url", value_name = "NAME=URL", value_parser = pair)]
    pub source_urls: Vec<Pair>,
    /// A program a job names as `@bundle/NAME`: `NAME=${tree:TREE}/path`.
    #[arg(long = "entrypoint", value_name = "NAME=PATH", value_parser = pair)]
    pub entrypoints: Vec<Pair>,
    /// An environment rule, `OP:NAME=VALUE` where OP is set, prepend, append or
    /// default. A name given with the same OP again adds a value to its rule.
    #[arg(long = "env", value_name = "OP:NAME=VALUE", value_parser = env_flag)]
    pub env: Vec<EnvFlag>,
    /// Refuses a job's environment that sets this variable; the variable needs an
    /// `--env` rule.
    #[arg(long = "lock-env", value_name = "NAME")]
    pub locked_env: Vec<String>,
    /// The system directories follow the bundle's `PATH` entries.
    #[arg(long = "system-path")]
    pub system_path: bool,
    /// A tree that needs a writable clone. Discouraged.
    #[arg(long = "writable-tree", value_name = "NAME")]
    pub writable_trees: Vec<String>,
    /// A read the tools need from the host: system-certs, timezone, resolver-config,
    /// sysctl-info or locale.
    #[arg(long = "read", value_name = "KIND", value_parser = read_grant)]
    pub reads: Vec<ReadGrantKind>,
    /// The highest network level the tools need: off, loopback or on.
    #[arg(long, default_value = "off", value_parser = network)]
    pub network: NetworkLevel,
}

/// A layout directory and the manifest in it.
#[derive(Debug, Args)]
pub struct LayoutArgs {
    /// The layout directory.
    pub layout: PathBuf,
    /// The manifest digest. Without it the layout must list exactly one manifest.
    #[arg(long)]
    pub digest: Option<String>,
}

/// The flags of `verify`.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// The layout and manifest to check.
    #[command(flatten)]
    pub source: LayoutArgs,
    /// A directory to make and extract the layers into; it must not exist and is
    /// removed when the command ends.
    #[arg(long)]
    pub scratch: PathBuf,
}
