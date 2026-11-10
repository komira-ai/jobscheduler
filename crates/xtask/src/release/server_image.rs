//! `release image --binary <path> --version <X.Y.Z> --target <rust target> --out <dir>`: the
//! container image of one Linux platform's `js-lease-server`, as an OCI image layout in a tar,
//! `<out>/js-lease-server-<version>-<target>.oci.tar`. The target is
//! `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`. The file may not exist already.
//! Nothing here runs a container tool; `artifacts.yml` lists the file in `SHA256SUMS` and its
//! `attest` job signs it with a bundle of its own, so no sums file is written.
//!
//! The tar is what `docker load` reads, and what an OCI layout reader (`skopeo`, `crane`)
//! reads from a directory once unpacked:
//!
//! - `oci-layout`: `{"imageLayoutVersion":"1.0.0"}`.
//! - `index.json`: one manifest descriptor, with the platform and no `org.opencontainers.image.ref.name`
//!   annotation. The image has no name or tag, only a digest: the `sha256` of the manifest that
//!   `index.json` names.
//! - `blobs/sha256/<hex>`: the manifest, the config and the one layer
//!   (`application/vnd.oci.image.layer.v1.tar+gzip`).
//! - `manifest.json`: the list `docker load` of an engine older than the OCI layout reads
//!   (`Config`, `RepoTags` null, `Layers`), naming the same blobs.
//!
//! The layer holds [`PROGRAM`] (mode 0755) and the repository's `LICENSE` at [`LICENSE_PATH`]
//! (mode 0644) with the directories above them, nothing else: no shell, no libraries and no
//! certificate bundle (the server is a static musl program that trusts the public roots built
//! into it, and reads the root of its state store's TLS from a file the operator mounts). The
//! config runs [`PROGRAM`] as user [`USER`] from `/`, stops on `SIGTERM`, exposes no port (the
//! server listens where its flags say) and carries no creation time.
//!
//! The binary is never run. It must hold its version as text ([`holds_version`]), so an image
//! never names a version its program does not report; `artifacts.yml` runs `--version` on the
//! same binary before this step.
//!
//! The file is the same bytes for the same inputs on every runner: entries sorted by path with
//! each directory before what is in it, every entry owned by `root` and dated
//! [`MTIME`](super::deb::MTIME), the layer in a gzip stream with no file name and no time.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::deb::MTIME;
use super::{Error, Version, io_error};

/// The image's name in the file name, and the name of its program.
pub const NAME: &str = "js-lease-server";

/// Where the program is in the image, and what the config runs.
pub const PROGRAM: &str = "/usr/local/bin/js-lease-server";

/// Where the repository's `LICENSE` is in the image.
pub const LICENSE_PATH: &str = "/usr/share/doc/js-lease-server/LICENSE";

/// The user the config runs the program as: a numeric user and group that the image's files need
/// not name (the image has no `/etc/passwd`).
pub const USER: &str = "65532:65532";

/// The repository, as the `org.opencontainers.image.source` label.
pub const SOURCE: &str = "https://github.com/komira-ai/jobscheduler";

/// The licence of the repository's `LICENSE`, as the `org.opencontainers.image.licenses` label.
pub const LICENSES: &str = "Apache-2.0";

/// The repository's `LICENSE`, as the layer carries it.
const LICENSE_TEXT: &[u8] = include_bytes!("../../../../LICENSE");

/// A Rust target that has an image, spelled as `cargo build --target` spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Target {
    /// `aarch64-unknown-linux-musl`, the image architecture `arm64`.
    #[value(name = "aarch64-unknown-linux-musl")]
    Aarch64,
    /// `x86_64-unknown-linux-musl`, the image architecture `amd64`.
    #[value(name = "x86_64-unknown-linux-musl")]
    X86_64,
}

impl Target {
    /// Every target, in the order of their spellings.
    pub const ALL: [Target; 2] = [Target::Aarch64, Target::X86_64];

    /// The spelling: `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Target::Aarch64 => "aarch64-unknown-linux-musl",
            Target::X86_64 => "x86_64-unknown-linux-musl",
        }
    }

    /// The image architecture: `amd64` or `arm64`.
    #[must_use]
    pub const fn architecture(self) -> &'static str {
        match self {
            Target::Aarch64 => "arm64",
            Target::X86_64 => "amd64",
        }
    }
}

/// The arguments of `release image`.
#[derive(clap::Args)]
pub struct ImageArgs {
    /// The built program, static, of the target `--target` names. Never run.
    #[arg(long)]
    pub binary: PathBuf,
    /// The release version, `MAJOR.MINOR.PATCH`: what `js-lease-server --version` prints.
    #[arg(long)]
    pub version: Version,
    /// The target the program was built for.
    #[arg(long, value_enum)]
    pub target: Target,
    /// The directory the image is written to; made if missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// An image: the bytes of its tar, and the `sha256` (hex) of its manifest, which is the digest
/// the image is deployed by.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    /// The OCI layout tar.
    pub tar: Vec<u8>,
    /// The hex `sha256` of the manifest `index.json` names.
    pub manifest_digest: String,
}

/// One entry of a tar: path, mode and content; a directory has none.
type Entry = (String, u32, Option<Vec<u8>>);

/// `js-lease-server-<version>-<target>.oci.tar`.
#[must_use]
pub fn file_name(version: Version, target: Target) -> String {
    format!("{NAME}-{version}-{}.oci.tar", target.name())
}

/// Whether `haystack` holds `version` as a whole version: the bytes before it are not a digit
/// or a dot, and the byte after it is not a digit, so `1.2.30` does not pass for `1.2.3` and
/// `11.2.3` and `0.1.2.3` do not either. The program holds its version as text, the one
/// `--version` prints; no longer context of it is the same in every build.
#[must_use]
pub fn holds_version(haystack: &[u8], version: Version) -> bool {
    let text = version.to_string();
    let needle = text.as_bytes();
    haystack
        .windows(needle.len())
        .enumerate()
        .any(|(at, window)| {
            window == needle
                && !haystack[..at]
                    .last()
                    .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
                && !haystack
                    .get(at + needle.len())
                    .is_some_and(u8::is_ascii_digit)
        })
}

/// The lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `files` as tar entries: the directories above them (every prefix of a path, each once,
/// mode 0755) as `a/b/` and the files as `a/b/c`, sorted by path, each directory before what is
/// in it.
fn tree(files: Vec<(String, u32, Vec<u8>)>) -> Vec<Entry> {
    let mut dirs = BTreeSet::new();
    for (path, _, _) in &files {
        let mut end = 0;
        while let Some(slash) = path[end..].find('/') {
            end += slash;
            dirs.insert(path[..end].to_owned());
            end += 1;
        }
    }
    let mut entries: Vec<Entry> = dirs
        .into_iter()
        .map(|dir| (format!("{dir}/"), 0o755, None))
        .collect();
    entries.extend(
        files
            .into_iter()
            .map(|(path, mode, content)| (path, mode, Some(content))),
    );
    entries.sort_by(|a, b| a.0.trim_end_matches('/').cmp(b.0.trim_end_matches('/')));
    entries
}

/// The entries of the layer for `binary`.
fn layer_entries(binary: &[u8]) -> Vec<Entry> {
    tree(vec![
        (
            PROGRAM.trim_start_matches('/').to_owned(),
            0o755,
            binary.to_vec(),
        ),
        (
            LICENSE_PATH.trim_start_matches('/').to_owned(),
            0o644,
            LICENSE_TEXT.to_vec(),
        ),
    ])
}

/// An [`Error::Io`] for the stream being built (`what`).
fn stream_error(what: &str, e: &std::io::Error) -> Error {
    Error::Io {
        path: what.to_owned(),
        why: e.to_string(),
    }
}

/// Writes the tar of `entries` to `sink`, in order, each owned by `root` and dated [`MTIME`], and
/// gives the sink back once the archive is finished. A write to a `Vec` does not fail; one to
/// any other sink may.
fn tar_to<W: Write>(sink: W, entries: &[Entry]) -> Result<W, Error> {
    let mut tar = tar::Builder::new(sink);
    for (path, mode, content) in entries {
        let mut header = tar::Header::new_ustar();
        // Every path of the image is far below the 100 bytes of the name field; the prefix
        // field is not used.
        header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        let bytes = content.as_deref().unwrap_or_default();
        header.set_entry_type(if content.is_some() {
            tar::EntryType::Regular
        } else {
            tar::EntryType::Directory
        });
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        // "root" fits the 32 bytes ustar has for a name.
        let _ = header.set_username("root");
        let _ = header.set_groupname("root");
        header.set_mtime(u64::from(MTIME));
        header.set_cksum();
        tar.append(&header, bytes)
            .map_err(|e| stream_error(path, &e))?;
    }
    tar.into_inner()
        .map_err(|e| stream_error("the tar archive", &e))
}

/// Writes the gzip stream of `bytes` to `sink`, with no file name and no time, and gives the
/// sink back once the stream is finished.
fn gzip_to<W: Write>(sink: W, bytes: &[u8]) -> Result<W, Error> {
    let mut gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(sink, Compression::best());
    gzip.write_all(bytes)
        .map_err(|e| stream_error("the layer", &e))?;
    gzip.finish().map_err(|e| stream_error("the layer", &e))
}

/// What makes a tar of entries: [`tar_to`] into a `Vec`, or a failing one in a test.
type Tarer<'a> = dyn Fn(&[Entry]) -> Result<Vec<u8>, Error> + 'a;

/// What makes a gzip stream of bytes: [`gzip_to`] into a `Vec`, or a failing one in a test.
type Gzipper<'a> = dyn Fn(&[u8]) -> Result<Vec<u8>, Error> + 'a;

/// The descriptor of a blob: its media type, digest and size, which every manifest and index of
/// the layout states.
fn descriptor(media_type: &str, bytes: &[u8]) -> Value {
    json!({
        "mediaType": media_type,
        "digest": format!("sha256:{}", sha256_hex(bytes)),
        "size": bytes.len(),
    })
}

/// The image config for `target`: the platform, how the program runs, the labels and the
/// uncompressed layer's digest. It has no creation time and no history.
fn config(version: Version, target: Target, diff_id: &str) -> Value {
    json!({
        "architecture": target.architecture(),
        "os": "linux",
        "config": {
            "User": USER,
            "Entrypoint": [PROGRAM],
            "WorkingDir": "/",
            "StopSignal": "SIGTERM",
            "Labels": {
                "org.opencontainers.image.version": version.to_string(),
                "org.opencontainers.image.source": SOURCE,
                "org.opencontainers.image.licenses": LICENSES,
            },
        },
        "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{diff_id}")]},
    })
}

/// The tar of the image of `binary` for `target` at `version`, with `tar` making each tar (the
/// layer, then the layout) and `gzip` compressing the layer.
fn build_with(
    version: Version,
    target: Target,
    binary: &[u8],
    tar: &Tarer<'_>,
    gzip: &Gzipper<'_>,
) -> Result<Image, Error> {
    let layer_tar = tar(&layer_entries(binary))?;
    let layer = gzip(&layer_tar)?;
    let config = config(version, target, &sha256_hex(&layer_tar))
        .to_string()
        .into_bytes();
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": descriptor("application/vnd.oci.image.config.v1+json", &config),
        "layers": [descriptor("application/vnd.oci.image.layer.v1.tar+gzip", &layer)],
    })
    .to_string()
    .into_bytes();
    let mut manifest_entry = descriptor("application/vnd.oci.image.manifest.v1+json", &manifest);
    manifest_entry["platform"] = json!({"architecture": target.architecture(), "os": "linux"});
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [manifest_entry],
    })
    .to_string()
    .into_bytes();
    let manifest_digest = sha256_hex(&manifest);
    let blob = |bytes: &[u8]| format!("blobs/sha256/{}", sha256_hex(bytes));
    let docker = json!([{
        "Config": blob(&config),
        "RepoTags": null,
        "Layers": [blob(&layer)],
    }])
    .to_string()
    .into_bytes();
    let files = vec![
        (
            "oci-layout".to_owned(),
            0o644,
            br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
        ),
        ("index.json".to_owned(), 0o644, index),
        ("manifest.json".to_owned(), 0o644, docker),
        (blob(&config), 0o644, config),
        (blob(&layer), 0o644, layer),
        (blob(&manifest), 0o644, manifest),
    ];
    Ok(Image {
        tar: tar(&tree(files))?,
        manifest_digest,
    })
}

/// The image of `binary` for `target` at `version`.
///
/// # Errors
/// [`Error::Io`] if a tar or gzip stream cannot be finished.
pub fn build(version: Version, target: Target, binary: &[u8]) -> Result<Image, Error> {
    build_with(
        version,
        target,
        binary,
        &|entries| tar_to(Vec::new(), entries),
        &|bytes| gzip_to(Vec::new(), bytes),
    )
}

/// Writes `bytes` to a file that must not exist yet.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::File::create_new(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|e| io_error(path, &e))
}

/// Runs `release image`: the line to print,
/// `<name>: sha256 <hex>, <n> bytes, image sha256:<hex>`, the first digest that of the file
/// and the second that of the image's manifest.
///
/// The binary is read and judged before anything is written.
///
/// # Errors
/// [`Error::Io`] for the binary, the directory or the image (an existing image is refused);
/// [`Error::NotOfVersion`] for a binary that does not hold the version.
pub fn run(args: &ImageArgs) -> Result<String, Error> {
    let binary = std::fs::read(&args.binary).map_err(|e| io_error(&args.binary, &e))?;
    if !holds_version(&binary, args.version) {
        return Err(Error::NotOfVersion {
            path: args.binary.display().to_string(),
            version: args.version.to_string(),
        });
    }
    let image = build(args.version, args.target, &binary)?;
    std::fs::create_dir_all(&args.out).map_err(|e| io_error(&args.out, &e))?;
    let name = file_name(args.version, args.target);
    write_new(&args.out.join(&name), &image.tar)?;
    Ok(format!(
        "{name}: sha256 {}, {} bytes, image sha256:{}",
        sha256_hex(&image.tar),
        image.tar.len(),
        image.manifest_digest
    ))
}

#[cfg(test)]
mod tests;
