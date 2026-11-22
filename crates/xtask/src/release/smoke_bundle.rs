//! `release smoke-bundle pack|push|check`: the smoke bundle, a darwin bundle of one program
//! that says where a job ran (`packaging/js-smoke-bundle/`), packed from its committed tree,
//! published to the registry the release puts bundles in, and pulled back from it.
//!
//! The recipe is `packaging/js-smoke-bundle/bundle.toml` ([`Recipe`]), compiled in, so the
//! job that holds the push token needs no checkout:
//!
//! - `pack --tree <dir> --out <dir>` packs the tree with `js-bundle pack` and the recipe's
//!   fields into a new layout directory, and fails unless the manifest digest is the recipe's:
//!   the digest is a function of the tree and the recipe, and the recipe says what it is.
//! - `push --layout <dir> --user <name> --password-file <file>` is `js-bundle push` of that
//!   layout's manifest (by the recipe's digest) to the recipe's `repository:tag`: `pushed`, or
//!   `unchanged` when the tag names it already; a tag that names another manifest is refused
//!   and left as it is.
//! - `check --tree <dir> --scratch <dir>` pulls the bundle by the recipe's digest with the
//!   agent's registry client (`js_oci::Client`, anonymous, the repository alone on its
//!   allow-list), so it fails while the package is not public; checks it as a pull does (the
//!   manifest against the digest, the bundle kind, every blob's length and digest) and the
//!   config under the format's rules; extracts its one layer into the new directory
//!   `--scratch` with the extractor the agent's helper runs; and compares every entry with
//!   the tree: the same paths, kinds, execute bits, link targets and bytes, and nothing more.
//!   The scratch directory is removed when the check ends.

use std::collections::BTreeMap;
use std::os::fd::AsFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use js_bundle::media::LayerEncoding;
use js_bundle::{BundleConfig, DEFAULT_MODEL_BYTES, Limits, unpack_layer};
use js_bundle_cli::{Cli, Command, LayoutArgs, PushArgs};
use js_oci::{Allow, BundleSource, Client, Digest, Kind, Location, Policy};
use serde::Deserialize;
use sha2::Digest as _;

/// The recipe, as committed.
pub const RECIPE: &str = include_str!("../../../../packaging/js-smoke-bundle/bundle.toml");

/// The smoke bundle's directory, relative to the repository root: `bundle.toml` and `tree/`.
pub const DIR: &str = "packaging/js-smoke-bundle";

/// Why a smoke-bundle verb failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The recipe is not one.
    #[error("bundle.toml: {0}")]
    Recipe(String),
    /// `js-bundle` refused its arguments or failed.
    #[error("js-bundle: {0}")]
    Bundle(#[from] js_bundle_cli::Error),
    /// The tree packs to a digest that is not the recipe's.
    #[error(
        "the tree packs to {packed}, but bundle.toml says {pinned}: put the new digest in bundle.toml, with a new tag"
    )]
    Digest {
        /// What it packs to.
        packed: String,
        /// What the recipe says.
        pinned: String,
    },
    /// The registry, or what it served, was refused.
    #[error("{0}")]
    Oci(#[from] js_oci::Error),
    /// The bundle's config breaks a rule of the format.
    #[error("the bundle's config: {0}")]
    Config(#[from] js_bundle::ConfigError),
    /// The bundle does not hold exactly one layer, as the recipe makes it.
    #[error("the bundle has {manifest} layers and its config {config}; the smoke bundle has one")]
    Layers {
        /// How many the manifest lists.
        manifest: usize,
        /// How many the config describes.
        config: usize,
    },
    /// The layer's media type is not a bundle layer's (the bundle kind takes no other).
    #[error("the layer's media type is not a bundle layer's")]
    LayerType,
    /// The layer could not be extracted.
    #[error("extracting the layer: {0}")]
    Unpack(#[from] js_bundle::UnpackError),
    /// A file or directory could not be read or made.
    #[error("{path}: {why}")]
    Io {
        /// The path.
        path: PathBuf,
        /// What the system said.
        why: String,
    },
    /// An entry the format does not hold (a fifo, a socket, a device).
    #[error("{0}: neither a directory, a regular file nor a symbolic link")]
    Special(PathBuf),
    /// The pulled bundle is not the tree.
    #[error("{path}: the pulled bundle has {pulled}, the tree has {committed}")]
    Differs {
        /// The entry, relative to the tree.
        path: String,
        /// What the pulled bundle has there.
        pulled: String,
        /// What the tree has there.
        committed: String,
    },
}

/// The fields of `bundle.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Recipe {
    /// Where the release publishes it: `REGISTRY/REPOSITORY`. Its last part is the bundle's
    /// name, its tree's and its entrypoint's.
    pub repository: String,
    /// The tag, which is also the bundle's version.
    pub tag: String,
    /// The manifest digest the tree packs to.
    pub digest: String,
    /// The oldest macOS the bundle runs on.
    pub os_version: String,
    /// The tree's licence, SPDX.
    pub license: String,
    /// Where the tree's source is.
    pub source_url: String,
    /// The repository the package belongs to (`org.opencontainers.image.source`).
    pub source: String,
}

impl Recipe {
    /// The recipe in `text`.
    ///
    /// # Errors
    /// [`Error::Recipe`] when it is not TOML of exactly these fields, or the repository has
    /// no `/`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let recipe: Self = toml::from_str(text).map_err(|e| Error::Recipe(e.to_string()))?;
        if !recipe.repository.contains('/') {
            return Err(Error::Recipe(format!(
                "repository {:?} is not REGISTRY/REPOSITORY",
                recipe.repository
            )));
        }
        Ok(recipe)
    }

    /// The committed recipe.
    ///
    /// # Errors
    /// As [`Recipe::parse`].
    pub fn committed() -> Result<Self, Error> {
        Self::parse(RECIPE)
    }

    /// The bundle's name: the last part of the repository.
    #[must_use]
    pub fn name(&self) -> &str {
        self.repository
            .rsplit('/')
            .next()
            .unwrap_or(&self.repository)
    }

    /// `repository:tag@digest`, the reference a job names.
    #[must_use]
    pub fn reference(&self) -> String {
        format!("{}:{}@{}", self.repository, self.tag, self.digest)
    }

    /// The arguments of the `js-bundle pack` of `tree` into `out` that this recipe is.
    #[must_use]
    pub fn pack_args(&self, tree: &Path, out: &Path) -> Vec<String> {
        let name = self.name();
        [
            "js-bundle",
            "pack",
            "--out",
            &out.display().to_string(),
            "--name",
            name,
            "--bundle-version",
            &self.tag,
            "--os-version",
            &self.os_version,
            "--tree",
            &format!("{name}={}", tree.display()),
            "--license",
            &format!("{name}={}", self.license),
            "--source-url",
            &format!("{name}={}", self.source_url),
            "--entrypoint",
            &format!("{name}=${{tree:{name}}}/bin/{name}"),
            "--annotation",
            &format!("org.opencontainers.image.source={}", self.source),
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

/// The flags of `pack`.
#[derive(Debug, clap::Args)]
pub struct PackArgs {
    /// The bundle's tree: `packaging/js-smoke-bundle/tree` of a checkout.
    #[arg(long)]
    pub tree: PathBuf,
    /// The layout directory to make; it must not exist.
    #[arg(long)]
    pub out: PathBuf,
}

/// The flags of `push`.
#[derive(Debug, clap::Args)]
pub struct PushFlags {
    /// The layout `pack` made.
    #[arg(long)]
    pub layout: PathBuf,
    /// The user the registry's token realm is asked for a push token as.
    #[arg(long)]
    pub user: String,
    /// A file that holds the password or token, one line; `/dev/stdin` reads standard input.
    #[arg(long)]
    pub password_file: PathBuf,
}

/// The flags of `check`.
#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// The bundle's tree: `packaging/js-smoke-bundle/tree` of a checkout.
    #[arg(long)]
    pub tree: PathBuf,
    /// A directory to make and extract the pulled layer into; it must not exist, and it is
    /// removed when the check ends.
    #[arg(long)]
    pub scratch: PathBuf,
}

/// The `smoke-bundle` verbs.
#[derive(Debug, clap::Subcommand)]
pub enum Verb {
    /// Pack the committed tree and refuse a digest that is not the recipe's.
    Pack(PackArgs),
    /// Push the packed bundle to the recipe's repository and tag.
    Push(PushFlags),
    /// Pull the published bundle by digest and compare every byte with the tree.
    Check(CheckArgs),
}

/// Runs `verb` with the committed recipe: the line to print.
///
/// # Errors
/// The verb's failure.
pub fn run(verb: &Verb) -> Result<String, Error> {
    let recipe = Recipe::committed()?;
    match verb {
        Verb::Pack(args) => pack(&recipe, &args.tree, &args.out),
        Verb::Push(args) => push(&recipe, args),
        Verb::Check(args) => Ok(checked_line(&recipe, check(&recipe, args)?)),
    }
}

/// Packs `tree` by `recipe` into `out`, and refuses a digest that is not the recipe's.
///
/// # Errors
/// What `js-bundle pack` refuses, and [`Error::Digest`].
pub fn pack(recipe: &Recipe, tree: &Path, out: &Path) -> Result<String, Error> {
    let cli = Cli::try_parse_from(recipe.pack_args(tree, out))
        .map_err(|e| Error::Recipe(e.to_string()))?;
    let mut printed = Vec::new();
    js_bundle_cli::execute(cli.command, &mut printed)?;
    let packed = String::from_utf8_lossy(&printed).trim().to_owned();
    if packed != recipe.digest {
        return Err(Error::Digest {
            packed,
            pinned: recipe.digest.clone(),
        });
    }
    Ok(format!("packed {}", recipe.reference()))
}

/// The `js-bundle push` that `push` runs.
#[must_use]
pub fn push_args(recipe: &Recipe, flags: &PushFlags) -> PushArgs {
    PushArgs {
        source: LayoutArgs {
            layout: flags.layout.clone(),
            digest: Some(recipe.digest.clone()),
        },
        to: format!("{}:{}", recipe.repository, recipe.tag),
        user: Some(flags.user.clone()),
        password_file: Some(flags.password_file.clone()),
        plain_http: false,
    }
}

/// Pushes the layout `flags` names by `recipe`.
fn push(recipe: &Recipe, flags: &PushFlags) -> Result<String, Error> {
    let mut printed = Vec::new();
    let done = js_bundle_cli::execute(Command::Push(push_args(recipe, flags)), &mut printed);
    printed_line(done, &printed)
}

/// The line `js-bundle` printed, when it succeeded.
///
/// # Errors
/// Its failure.
pub fn printed_line(
    done: Result<(), js_bundle_cli::Error>,
    printed: &[u8],
) -> Result<String, Error> {
    done?;
    Ok(String::from_utf8_lossy(printed).trim().to_owned())
}

/// Pulls the bundle from the recipe's repository and compares it with the tree: the number of
/// entries compared.
fn check(recipe: &Recipe, args: &CheckArgs) -> Result<usize, Error> {
    let client = Client::new(Policy::new(vec![Allow::parse(&recipe.repository)?]));
    let repository = client.repository(Location::parse(&recipe.repository)?)?;
    let digest = Digest::parse(&recipe.digest)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io_at(&args.scratch))?;
    runtime.block_on(compare(&repository, &digest, &args.tree, &args.scratch))
}

/// What `check` prints when the pulled bundle is the tree's `entries` entries.
#[must_use]
pub fn checked_line(recipe: &Recipe, entries: usize) -> String {
    format!(
        "checked {}: {entries} entries, each the tree's",
        recipe.reference()
    )
}

/// Fetches the bundle `digest` from `source`, extracts its one layer into the new directory
/// `scratch` (removed before this returns), and compares it with `tree`, which is walked
/// first: the number of entries below the tree's root.
///
/// # Errors
/// What the pull or the extractor refuses, [`Error::Layers`], and [`Error::Differs`] at the
/// first path (in byte order) where the two differ.
pub async fn compare(
    source: &dyn BundleSource,
    digest: &Digest,
    tree: &Path,
    scratch: &Path,
) -> Result<usize, Error> {
    let committed = walk(tree)?;
    let kind = Kind::bundle();
    let manifest = source.fetch_manifest(digest, &kind).await?;
    let config = BundleConfig::parse(&source.fetch_config(&manifest, &kind).await?)?;
    let ([blob], [layer]) = (manifest.layers(), config.layers.as_slice()) else {
        return Err(Error::Layers {
            manifest: manifest.layers().len(),
            config: config.layers.len(),
        });
    };
    let mut bytes = Vec::new();
    source.fetch_blob(blob, &mut bytes).await?;
    let encoding = LayerEncoding::from_media_type(blob.media_type());
    let encoding = encoding.ok_or(Error::LayerType)?;
    let scratch = Scratch::create(scratch)?;
    let root = std::fs::File::open(&scratch.0).map_err(io_at(&scratch.0))?;
    let limits = Limits {
        entries: layer.entries,
        bytes: layer.uncompressed_bytes,
        model_bytes: DEFAULT_MODEL_BYTES,
    };
    unpack_layer(
        bytes.as_slice(),
        encoding,
        root.as_fd(),
        &layer.diff_id,
        &limits,
    )?;
    let pulled = walk(&scratch.0)?;
    differ(&pulled, &committed).map_or(Ok(committed.len()), Err)
}

/// The first difference between `pulled` and `committed`, in path order.
fn differ(pulled: &BTreeMap<String, Entry>, committed: &BTreeMap<String, Entry>) -> Option<Error> {
    let paths: std::collections::BTreeSet<&String> =
        pulled.keys().chain(committed.keys()).collect();
    paths.into_iter().find_map(|path| {
        let (have, want) = (pulled.get(path), committed.get(path));
        (have != want).then(|| Error::Differs {
            path: path.clone(),
            pulled: describe(have),
            committed: describe(want),
        })
    })
}

/// What an entry is, for an error.
fn describe(entry: Option<&Entry>) -> String {
    match entry {
        None => "nothing".to_owned(),
        Some(Entry::Dir) => "a directory".to_owned(),
        Some(Entry::Link(target)) => format!("a link to {target:?}"),
        Some(Entry::File { exec, bytes }) => format!(
            "{} of {} bytes, sha256 {}",
            if *exec {
                "an executable file"
            } else {
                "a file"
            },
            bytes.len(),
            hex::encode(sha2::Sha256::digest(bytes))
        ),
    }
}

/// One entry of a tree, as much of it as a bundle keeps.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File { exec: bool, bytes: Vec<u8> },
    Link(String),
}

/// Every entry below `root`, by its path relative to it (`/`-separated).
fn walk(root: &Path) -> Result<BTreeMap<String, Entry>, Error> {
    let mut found = BTreeMap::new();
    let mut todo = vec![(root.to_owned(), String::new())];
    while let Some((dir, prefix)) = todo.pop() {
        for item in std::fs::read_dir(&dir).map_err(io_at(&dir))? {
            let item = item.map_err(io_at(&dir))?;
            let path = item.path();
            let name = format!("{prefix}{}", item.file_name().to_string_lossy());
            let meta = std::fs::symlink_metadata(&path).map_err(io_at(&path))?;
            let entry = if meta.is_dir() {
                todo.push((path, format!("{name}/")));
                Entry::Dir
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).map_err(io_at(&path))?;
                let exec = meta.permissions().mode() & 0o111 != 0;
                Entry::File { exec, bytes }
            } else if meta.is_symlink() {
                let target = std::fs::read_link(&path).map_err(io_at(&path))?;
                Entry::Link(target.to_string_lossy().into_owned())
            } else {
                return Err(Error::Special(path));
            };
            found.insert(name, entry);
        }
    }
    Ok(found)
}

/// A directory this check made, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn create(path: &Path) -> Result<Self, Error> {
        std::fs::create_dir(path).map_err(io_at(path))?;
        Ok(Self(path.to_owned()))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: the directory is ours and holds only what the extractor wrote.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// [`Error::Io`] at `path`.
fn io_at(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Io {
        path: path.to_owned(),
        why: e.to_string(),
    }
}
