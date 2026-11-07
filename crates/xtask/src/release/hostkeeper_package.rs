//! `release hostkeeper-package --binary <path> --target <triple> --version X.Y.Z --out <dir>`:
//! the hostkeeper package of one platform, `<out>/js-agent-<version>-<platform>.tar.zst`
//! (docs/release.md, packaging/js-agent/hostkeeper/README.md).
//!
//! A package is the tree hostkeeper unpacks into `/var/lib/hostkeeper/packages/js-agent/<version>/`:
//! `manifest.toml`, `bin/js-agent`, the service manager's units in `units/` and, on Linux, the
//! tmpfiles file in `files/`. The files are the repository's own (`packaging/js-agent/hostkeeper`
//! for Linux, `packaging/js-agent-macos/hostkeeper` for a Mac), compiled in; only the manifest is
//! filled in, with the version, the platform and [`MIN_HOSTKEEPER`].
//!
//! The bytes are the same whatever builds them. The tar is written by the `tar` crate with ustar
//! headers, owner 0, no owner names, time 0, mode 0755 (directories and the binary) or 0644
//! (every other file), each directory before its files, names in order; it is compressed by
//! `ruzstd` at its fastest level into one frame with a content checksum, no dictionary and a
//! window below hostkeeper's bound. Nothing is read from the input binary's name, mode, owner or
//! time, and the binary is never run, so a runner of one platform packages another's binary.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{Error, Version, io_error};

/// The service's name in hostkeeper, and the name of the program in the package.
pub const SERVICE: &str = "js-agent";

/// The oldest hostkeeper the packages need, unless `--min-hostkeeper` says otherwise: the
/// release of hostkeeper the workspace pins and tests the packages against.
pub const MIN_HOSTKEEPER: &str = "0.1.92";

/// The program, inside the package.
pub const BINARY: &str = "bin/js-agent";

/// The mode of directories and of the binary.
pub const EXECUTABLE: u32 = 0o755;

/// The mode of every other file.
pub const FILE: u32 = 0o644;

/// The manifest of the Linux packages, with `@VERSION@`, `@PLATFORM@` and `@MIN_HOSTKEEPER@`.
pub const LINUX_MANIFEST: &str =
    include_str!("../../../../packaging/js-agent/hostkeeper/manifest.toml.in");

/// The manifest of the Mac package, with the same placeholders.
pub const MACOS_MANIFEST: &str =
    include_str!("../../../../packaging/js-agent-macos/hostkeeper/manifest.toml.in");

/// The placeholders of a manifest template.
pub const PLACEHOLDERS: [&str; 3] = ["@VERSION@", "@PLATFORM@", "@MIN_HOSTKEEPER@"];

/// The unit of the agent on a Linux node, `units/js-agent-crun.service`.
pub const LINUX_UNIT: &str =
    include_str!("../../../../packaging/js-agent/hostkeeper/units/js-agent-crun.service");

/// The GPU oneshot of that unit, `units/js-agent-crun-gpu.service`.
pub const LINUX_GPU_UNIT: &str =
    include_str!("../../../../packaging/js-agent/hostkeeper/units/js-agent-crun-gpu.service");

/// The tmpfiles file of the unit, `files/tmpfiles.conf`.
pub const LINUX_TMPFILES: &str =
    include_str!("../../../../packaging/js-agent/hostkeeper/files/tmpfiles.conf");

/// The launchd job of the agent on a Mac, `units/js-agent.plist`.
pub const MACOS_PLIST: &str =
    include_str!("../../../../packaging/js-agent-macos/hostkeeper/units/js-agent.plist");

/// A platform hostkeeper installs packages on, spelled as hostkeeper spells it
/// (`hostkeeper_config::platform::Platform`: `tests/hostkeeper_package` checks the two agree).
/// Declared in the order of the spellings, which the derived order follows: the order of the
/// packages in the release index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Platform {
    /// `linux-arm64`.
    LinuxArm64,
    /// `linux-x86_64`.
    LinuxX86_64,
    /// `macos-arm64`.
    MacosArm64,
}

impl Platform {
    /// Every platform, in the order of their spellings.
    pub const ALL: [Platform; 3] = [
        Platform::LinuxArm64,
        Platform::LinuxX86_64,
        Platform::MacosArm64,
    ];

    /// The spelling: `linux-x86_64`, `linux-arm64` or `macos-arm64`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Platform::LinuxArm64 => "linux-arm64",
            Platform::LinuxX86_64 => "linux-x86_64",
            Platform::MacosArm64 => "macos-arm64",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The Rust targets a package is made for, and the platform of each.
pub const TARGETS: [(&str, Platform); 3] = [
    ("x86_64-unknown-linux-musl", Platform::LinuxX86_64),
    ("aarch64-unknown-linux-musl", Platform::LinuxArm64),
    ("aarch64-apple-darwin", Platform::MacosArm64),
];

/// One entry of the tar: its name, mode and content (`None` for a directory).
pub type Entry = (String, u32, Option<Vec<u8>>);

/// The arguments of `release hostkeeper-package`.
#[derive(clap::Args)]
pub struct PackageArgs {
    /// The built program, of the platform `--target` names. Never run.
    #[arg(long)]
    pub binary: PathBuf,
    /// The Rust target it was built for (`x86_64-unknown-linux-musl`,
    /// `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`).
    #[arg(long)]
    pub target: String,
    /// The release version, `MAJOR.MINOR.PATCH`: what `js-agent --version` prints after the name.
    #[arg(long)]
    pub version: String,
    /// The oldest hostkeeper that may install the package, `MAJOR.MINOR.PATCH`: written to the
    /// manifest's `min_hostkeeper`.
    #[arg(long, default_value = MIN_HOSTKEEPER)]
    pub min_hostkeeper: String,
    /// The directory the package is written to; made if missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// The platform a build for `target` runs on.
///
/// # Errors
/// [`Error::UnknownTarget`] for a target that is none of [`TARGETS`].
pub fn platform_of(target: &str) -> Result<Platform, Error> {
    TARGETS
        .iter()
        .find(|(name, _)| *name == target)
        .map(|(_, platform)| *platform)
        .ok_or_else(|| Error::UnknownTarget(target.to_owned()))
}

/// The manifest of the package of `platform` and `version`, for hostkeeper `min_hostkeeper` and
/// newer.
#[must_use]
pub fn manifest(platform: Platform, version: Version, min_hostkeeper: &str) -> String {
    let template = match platform {
        Platform::MacosArm64 => MACOS_MANIFEST,
        Platform::LinuxX86_64 | Platform::LinuxArm64 => LINUX_MANIFEST,
    };
    template
        .replace("@VERSION@", &version.to_string())
        .replace("@PLATFORM@", platform.as_str())
        .replace("@MIN_HOSTKEEPER@", min_hostkeeper)
}

/// The files of the package of `platform` other than the manifest and the binary: name and text.
fn texts(platform: Platform) -> Vec<(&'static str, &'static str)> {
    match platform {
        Platform::MacosArm64 => vec![("units/js-agent.plist", MACOS_PLIST)],
        Platform::LinuxX86_64 | Platform::LinuxArm64 => vec![
            ("files/tmpfiles.conf", LINUX_TMPFILES),
            ("units/js-agent-crun.service", LINUX_UNIT),
            ("units/js-agent-crun-gpu.service", LINUX_GPU_UNIT),
        ],
    }
}

/// The entries of the package, sorted by name so that each directory comes before its files.
#[must_use]
pub fn entries(
    platform: Platform,
    version: Version,
    min_hostkeeper: &str,
    binary: &[u8],
) -> Vec<Entry> {
    let mut entries: Vec<Entry> = vec![
        (
            "manifest.toml".to_owned(),
            FILE,
            Some(manifest(platform, version, min_hostkeeper).into_bytes()),
        ),
        ("bin".to_owned(), EXECUTABLE, None),
        (BINARY.to_owned(), EXECUTABLE, Some(binary.to_vec())),
    ];
    for (name, text) in texts(platform) {
        let dir = name.rsplit_once('/').map_or("", |(dir, _)| dir);
        if !entries.iter().any(|(held, _, _)| held == dir) {
            entries.push((dir.to_owned(), EXECUTABLE, None));
        }
        entries.push((name.to_owned(), FILE, Some(text.as_bytes().to_vec())));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Writes `entries` as a tar to `sink` and gives the sink back, its end marker written.
///
/// # Errors
/// [`Error::Archive`] naming the entry, or the end, that the sink refused.
pub fn tar_to<W: Write>(sink: W, entries: &[Entry]) -> Result<W, Error> {
    let mut tar = tar::Builder::new(sink);
    for (name, mode, content) in entries {
        let mut header = tar::Header::new_ustar();
        let kind = if content.is_some() {
            tar::EntryType::Regular
        } else {
            tar::EntryType::Directory
        };
        let bytes = content.as_deref().unwrap_or_default();
        header.set_entry_type(kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        tar.append_data(&mut header, name, bytes)
            .map_err(|e| archive_error(&format!("the entry {name}"), &e))?;
    }
    tar.into_inner()
        .map_err(|e| archive_error("the end of the archive", &e))
}

/// An [`Error::Archive`].
fn archive_error(what: &str, e: &std::io::Error) -> Error {
    Error::Archive {
        what: what.to_owned(),
        why: e.to_string(),
    }
}

/// The bytes of the package: the tar of [`entries`], compressed.
///
/// # Errors
/// As [`tar_to`].
pub fn archive(
    platform: Platform,
    version: Version,
    min_hostkeeper: &str,
    binary: &[u8],
) -> Result<Vec<u8>, Error> {
    let tar = tar_to(
        Vec::new(),
        &entries(platform, version, min_hostkeeper, binary),
    )?;
    Ok(ruzstd::encoding::compress_to_vec(
        &tar[..],
        ruzstd::encoding::CompressionLevel::Fastest,
    ))
}

/// The file name of the package: `js-agent-<version>-<platform>.tar.zst`.
#[must_use]
pub fn file_name(version: Version, platform: Platform) -> String {
    format!("{SERVICE}-{version}-{}.tar.zst", platform.as_str())
}

/// Whether `haystack` holds `needle`.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The text of a js-agent binary that its `--version` is made of: the version, a newline and the
/// start of the runtimes line (`js_agent::cli::LONG_VERSION`, whose runtimes line starts with
/// `docker` on every platform). The binary holds no `js-agent <version>` as one piece of text:
/// clap puts the name before the version when it prints.
#[must_use]
pub fn version_marker(version: Version) -> String {
    format!("{version}\nruntimes: docker")
}

/// Writes `bytes` to a file that must not exist yet.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::File::create_new(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|e| io_error(path, &e))
}

/// Runs `release hostkeeper-package`: the line to print, `<name>: sha256 <hex>, <n> bytes`.
///
/// The target and the version are judged, and the binary read, before anything is written. The
/// binary must hold [`version_marker`], the text its `--version` is made of, so a package never
/// carries a manifest version its program does not report; the newline after the version keeps
/// `1.2.30` from passing for `1.2.3`.
///
/// # Errors
/// [`Error::UnknownTarget`]; [`Error::NotAVersion`] (for the version or for `--min-hostkeeper`); [`Error::Io`] for the binary or the package
/// (an existing package is refused, and so is a directory that cannot be made);
/// [`Error::VersionNotInBinary`]; [`Error::Archive`].
pub fn run(a: &PackageArgs) -> Result<String, Error> {
    let platform = platform_of(&a.target)?;
    let version: Version = a.version.parse()?;
    let min_hostkeeper: Version = a.min_hostkeeper.parse()?;
    let binary = std::fs::read(&a.binary).map_err(|e| io_error(&a.binary, &e))?;
    if !holds(&binary, version_marker(version).as_bytes()) {
        return Err(Error::VersionNotInBinary {
            path: a.binary.display().to_string(),
            version: version.to_string(),
        });
    }
    let package = archive(platform, version, &min_hostkeeper.to_string(), &binary)?;
    std::fs::create_dir_all(&a.out).map_err(|e| io_error(&a.out, &e))?;
    let name = file_name(version, platform);
    write_new(&a.out.join(&name), &package)?;
    Ok(format!(
        "{name}: sha256 {}, {} bytes",
        hex::encode(Sha256::digest(&package)),
        package.len()
    ))
}

#[cfg(test)]
mod tests;
