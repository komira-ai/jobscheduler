//! `release deb --binary <path> --version <X.Y.Z> --platform linux-x86_64|linux-arm64 --out
//! <dir>`: the Debian package of one Linux platform's `js-agent`,
//! `<out>/js-agent_<version>_<arch>.deb` (`amd64` for `linux-x86_64`, `arm64` for `linux-arm64`).
//! The package may not exist already. Nothing here runs `dpkg`; `artifacts.yml` runs
//! `dpkg-deb --info` and `dpkg-deb --contents` on the result and its `sums` job lists the file
//! in `SHA256SUMS`, so no sums file is written.
//!
//! The package holds [`FILES`], every one owned by `root:root`: what hostkeeper's native apt
//! installer puts on a host, and what a host without hostkeeper places by hand.
//!
//! The package depends on [`DEPENDS`] alone (the binary is static musl): the program its
//! [`POSTINST`] runs, which every host that runs the package's units already has, so
//! hostkeeper's `apt-get install --simulate` still finds nothing to install there but the
//! package. The postinst makes the job user of the package's `sysusers.d` file and nothing else;
//! the package has no other maintainer script and no conffiles, and it enables nothing: the
//! host's operator (or hostkeeper, from the hook manifest of the release's targets) starts a unit.
//!
//! The binary is never run. It must hold [`version_marker`], the text its `--version` is made
//! of, so a package never names a version its program does not report.
//!
//! The file is the same bytes for the same inputs on every runner: an `ar` archive of
//! `debian-binary`, `control.tar.gz` and `data.tar.gz`, in that order, every member and entry
//! owned by `root`, dated [`MTIME`] (not 1970, which `lintian` refuses), the entries sorted by
//! path with each directory before what is in it, in gzip streams with no file name and the
//! same time.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use md5::{Digest, Md5};
use sha2::Sha256;

use super::{Error, Version, io_error};

/// The package's name, and the name of its program.
pub const NAME: &str = "js-agent";

/// The time of every member and entry: 2026-09-01T00:00:00Z. A fixed date so the same inputs
/// give the same bytes, and not the epoch, which `lintian` refuses (`package-contains-ancient-file`).
pub const MTIME: u32 = 1_788_220_800;

/// The maintainer field: the project, as a noreply address.
pub const MAINTAINER: &str = "komira-ai <noreply@komira.com>";

/// The project's home page.
pub const HOMEPAGE: &str = "https://github.com/komira-ai/jobscheduler";

/// The first line of the description.
pub const SYNOPSIS: &str = "job scheduler agent: runs the jobs a lease server leases to this host";

/// The description after its first line, one string per line; an empty one is written ` .`.
pub const DESCRIPTION: [&str; 4] = [
    "js-agent joins a js-lease-server pool and runs the jobs it is leased in containers.",
    "",
    "The package installs the program, the systemd units of a host that runs jobs with crun, their",
    "job user and the host setup script. It starts and enables nothing: place the host's files, then start a unit.",
];

/// The `Depends` field: what provides `systemd-sysusers`, which [`POSTINST`] runs. `systemd`
/// first, the package a host that runs the units has; Debian's `dh_installsysusers` names the
/// same alternatives.
pub const DEPENDS: &str = "systemd | systemd-standalone-sysusers";

/// The `postinst` maintainer script (`packaging/js-agent/deb/postinst`): it runs
/// `systemd-sysusers` on the package's `sysusers.d` file, so the job user that
/// `js-agent-crun.service` names (`--job-user jsjob`) exists once the package is configured.
pub const POSTINST: &str = include_str!("../../../../packaging/js-agent/deb/postinst");

/// One file of the package.
#[derive(Clone, Copy, Debug)]
pub struct PackagedFile {
    /// The absolute path the package installs it at.
    pub path: &'static str,
    /// Its mode.
    pub mode: u32,
    /// Its text, compiled in from the repository; `None` for the program, which is `--binary`.
    pub text: Option<&'static str>,
}

/// The files of the package; `release deb` writes them and `channels check-apt` looks for them
/// under its root.
pub const FILES: [PackagedFile; 9] = [
    PackagedFile {
        path: "/usr/bin/js-agent",
        mode: 0o755,
        text: None,
    },
    PackagedFile {
        path: "/usr/lib/systemd/system/js-agent-crun.service",
        mode: 0o644,
        text: Some(include_str!(
            "../../../../packaging/js-agent/deb/js-agent-crun.service"
        )),
    },
    PackagedFile {
        path: "/usr/lib/systemd/system/js-agent-crun-gpu.service",
        mode: 0o644,
        text: Some(include_str!(
            "../../../../packaging/js-agent/deb/js-agent-crun-gpu.service"
        )),
    },
    PackagedFile {
        path: "/usr/lib/tmpfiles.d/js-agent-crun.conf",
        mode: 0o644,
        text: Some(include_str!(
            "../../../../packaging/js-agent/deb/tmpfiles.conf"
        )),
    },
    PackagedFile {
        path: "/usr/lib/sysusers.d/js-agent.conf",
        mode: 0o644,
        text: Some(include_str!(
            "../../../../packaging/js-agent/deb/sysusers.conf"
        )),
    },
    PackagedFile {
        path: "/usr/lib/systemd/system/js-agent.service",
        mode: 0o644,
        text: Some(include_str!(
            "../../../../packaging/js-agent/js-agent.service"
        )),
    },
    PackagedFile {
        path: "/usr/lib/tmpfiles.d/js-agent.conf",
        mode: 0o644,
        text: Some(include_str!("../../../../packaging/js-agent/tmpfiles.conf")),
    },
    PackagedFile {
        path: "/usr/share/js-agent/crun_host_setup.sh",
        mode: 0o755,
        text: Some(include_str!(
            "../../../../packaging/js-agent/crun_host_setup.sh"
        )),
    },
    PackagedFile {
        path: "/usr/share/doc/js-agent/copyright",
        mode: 0o644,
        text: Some(include_str!("../../../../LICENSE")),
    },
];

/// A platform that has a package, spelled as the workflow and hostkeeper spell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Platform {
    /// `linux-arm64`, Debian's `arm64`.
    #[value(name = "linux-arm64")]
    LinuxArm64,
    /// `linux-x86_64`, Debian's `amd64`.
    #[value(name = "linux-x86_64")]
    LinuxX86_64,
}

impl Platform {
    /// Every platform, in the order of their spellings.
    pub const ALL: [Platform; 2] = [Platform::LinuxArm64, Platform::LinuxX86_64];

    /// The spelling: `linux-x86_64` or `linux-arm64`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Platform::LinuxArm64 => "linux-arm64",
            Platform::LinuxX86_64 => "linux-x86_64",
        }
    }

    /// The Debian architecture: `amd64` or `arm64`.
    #[must_use]
    pub const fn architecture(self) -> &'static str {
        match self {
            Platform::LinuxArm64 => "arm64",
            Platform::LinuxX86_64 => "amd64",
        }
    }
}

/// The arguments of `release deb`.
#[derive(clap::Args)]
pub struct DebArgs {
    /// The built program, of the platform `--platform` names. Never run.
    #[arg(long)]
    pub binary: PathBuf,
    /// The release version, `MAJOR.MINOR.PATCH`: what `js-agent --version` prints.
    #[arg(long)]
    pub version: Version,
    /// The platform the program was built for.
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// The directory the package is written to; made if missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// One file of the package with its content: path from the root with no leading slash, mode and
/// bytes.
pub type File = (String, u32, Vec<u8>);

/// `js-agent_<version>_<architecture>.deb`.
#[must_use]
pub fn file_name(version: Version, platform: Platform) -> String {
    format!("{NAME}_{version}_{}.deb", platform.architecture())
}

/// The text of a js-agent binary that its `--version` is made of: the version, a newline and the
/// start of the runtimes line (`js_agent::cli::LONG_VERSION`, whose runtimes line starts with
/// `docker` on every platform). The binary holds no `js-agent <version>` as one piece of text:
/// clap puts the name before the version when it prints. The newline after the version keeps
/// `1.2.30` from passing for `1.2.3`.
#[must_use]
pub fn version_marker(version: Version) -> String {
    format!("{version}\nruntimes: docker")
}

/// Whether `haystack` holds `needle`.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The files of the package of `binary`, paths without the leading slash.
#[must_use]
pub fn files(binary: &[u8]) -> Vec<File> {
    FILES
        .iter()
        .map(|file| {
            let bytes = file
                .text
                .map_or_else(|| binary.to_vec(), |text| text.as_bytes().to_vec());
            (
                file.path.trim_start_matches('/').to_owned(),
                file.mode,
                bytes,
            )
        })
        .collect()
}

/// The `control` file of `version` for `architecture` holding `files`.
#[must_use]
pub fn control(version: Version, architecture: &str, files: &[File]) -> String {
    let bytes: usize = files.iter().map(|(_, _, content)| content.len()).sum();
    let mut text = format!(
        "Package: {NAME}\nVersion: {version}\nArchitecture: {architecture}\n\
         Maintainer: {MAINTAINER}\nInstalled-Size: {}\nDepends: {DEPENDS}\n\
         Section: admin\nPriority: optional\nHomepage: {HOMEPAGE}\nDescription: {SYNOPSIS}\n",
        bytes.div_ceil(1024),
    );
    for line in DESCRIPTION {
        if line.is_empty() {
            text.push_str(" .\n");
        } else {
            text.push_str(&format!(" {line}\n"));
        }
    }
    text
}

/// `md5sums`: the MD5 of each file, in path order, as `md5sum` writes them.
#[must_use]
pub fn md5sums(files: &[File]) -> String {
    let mut sorted: Vec<&File> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted
        .iter()
        .map(|(path, _, content)| format!("{}  {path}\n", hex::encode(Md5::digest(content))))
        .collect()
}

/// One entry of a tar: path, mode and content; a directory has none.
type Entry = (String, u32, Option<Vec<u8>>);

/// The directories above `files` (every prefix of a path, each once) as `./a/b/` entries, and
/// the files as `./a/b/c`, sorted by path, each directory before what is in it.
fn data_entries(files: &[File]) -> Vec<Entry> {
    let mut dirs = BTreeSet::new();
    for (path, _, _) in files {
        let mut end = 0;
        while let Some(slash) = path[end..].find('/') {
            end += slash;
            dirs.insert(path[..end].to_owned());
            end += 1;
        }
    }
    let mut entries: Vec<Entry> = vec![("./".to_owned(), 0o755, None)];
    entries.extend(
        dirs.into_iter()
            .map(|dir| (format!("./{dir}/"), 0o755, None)),
    );
    entries.extend(
        files
            .iter()
            .map(|(path, mode, content)| (format!("./{path}"), *mode, Some(content.clone()))),
    );
    entries.sort_by(|a, b| a.0.trim_end_matches('/').cmp(b.0.trim_end_matches('/')));
    entries
}

/// An [`Error::Io`] for the archive being built (`what`).
fn archive_error(what: &str, e: &std::io::Error) -> Error {
    Error::Io {
        path: what.to_owned(),
        why: e.to_string(),
    }
}

/// The gzip-compressed tar of `entries`, in order, each owned by `root` and dated [`MTIME`].
fn tar_gz(entries: &[Entry]) -> Result<Vec<u8>, Error> {
    tar_gz_to(Vec::new(), entries)
}

/// Writes the gzip-compressed tar of `entries` to `sink` and gives the sink back, once the
/// compressed stream is finished. A write to a `Vec` does not fail; one to any other sink may.
fn tar_gz_to<W: Write>(sink: W, entries: &[Entry]) -> Result<W, Error> {
    let gzip = flate2::GzBuilder::new()
        .mtime(MTIME)
        .write(sink, Compression::best());
    let mut tar = tar::Builder::new(gzip);
    for (path, mode, content) in entries {
        let mut header = tar::Header::new_ustar();
        // The name is written as it is, with its `./`, which `set_path` would drop. Every
        // path of the package is far below the 100 bytes of the field; the prefix field is
        // not used.
        let field = &mut header.as_old_mut().name;
        field[..path.len()].copy_from_slice(path.as_bytes());
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
            .map_err(|e| archive_error(path, &e))?;
    }
    tar.into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .map_err(|e| archive_error("the tar archive", &e))
}

/// The `ar` archive of the members `(name, content)`, in order, each padded to an even length.
/// A member is far below the 10 digits of a header's size field.
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, content) in members {
        let header = format!(
            "{name:<16}{MTIME:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            0,
            0,
            "100644",
            content.len()
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(content);
        if content.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The package file of `version` for `platform` holding `files`.
///
/// # Errors
/// [`Error::Io`] if a tar or gzip stream cannot be finished.
pub fn build(version: Version, platform: Platform, files: &[File]) -> Result<Vec<u8>, Error> {
    build_with(version, platform, files, &tar_gz)
}

/// What makes a gzip-compressed tar of entries: [`tar_gz`], or a failing one in a test.
type Archiver<'a> = dyn Fn(&[Entry]) -> Result<Vec<u8>, Error> + 'a;

/// [`build`], with `archive` making each gzip-compressed tar (the control one, then the data one).
fn build_with(
    version: Version,
    platform: Platform,
    files: &[File],
    archive: &Archiver<'_>,
) -> Result<Vec<u8>, Error> {
    let text = |text: String| Some(text.into_bytes());
    let control_entries = [
        ("./".to_owned(), 0o755, None),
        (
            "./control".to_owned(),
            0o644,
            text(control(version, platform.architecture(), files)),
        ),
        ("./md5sums".to_owned(), 0o644, text(md5sums(files))),
        ("./postinst".to_owned(), 0o755, text(POSTINST.to_owned())),
    ];
    let control_tar = archive(&control_entries)?;
    let data_tar = archive(&data_entries(files))?;
    Ok(ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", &control_tar),
        ("data.tar.gz", &data_tar),
    ]))
}

/// Writes `bytes` to a file that must not exist yet.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::File::create_new(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|e| io_error(path, &e))
}

/// Runs `release deb`: the line to print, `<name>: sha256 <hex>, <n> bytes`.
///
/// The binary is read and judged before anything is written.
///
/// # Errors
/// [`Error::Io`] for the binary, the directory or the package (an existing package is
/// refused); [`Error::VersionNotInBinary`] for a binary that does not hold [`version_marker`].
pub fn run(args: &DebArgs) -> Result<String, Error> {
    let binary = std::fs::read(&args.binary).map_err(|e| io_error(&args.binary, &e))?;
    if !holds(&binary, version_marker(args.version).as_bytes()) {
        return Err(Error::VersionNotInBinary {
            path: args.binary.display().to_string(),
            version: args.version.to_string(),
        });
    }
    let package = build(args.version, args.platform, &files(&binary))?;
    std::fs::create_dir_all(&args.out).map_err(|e| io_error(&args.out, &e))?;
    let name = file_name(args.version, args.platform);
    write_new(&args.out.join(&name), &package)?;
    Ok(format!(
        "{name}: sha256 {}, {} bytes",
        hex::encode(Sha256::digest(&package)),
        package.len()
    ))
}

#[cfg(test)]
mod tests;
