//! The js-agent installer package for macOS (`packaging/js-agent-macos`).
//!
//! `pkg stage-macos` lays the payload and the scripts down in a directory from the built
//! programs, the licence and the files of the package; the workflow gives that directory to
//! `pkgbuild` and `productbuild`. `pkg check-macos` reads the package back with Apple's
//! `pkgutil` and `lsbom` and refuses it unless it holds exactly [`entries`], each with its
//! mode and root as owner, the one script [`postinstall_script`] and the identifier and
//! version of the release. The logic of the postinstall is not in the script, which only runs
//! `js-mac-provision postinstall` from the release it installed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::release::Version;

/// The package identifier.
pub const IDENTIFIER: &str = "com.komira.js-agent";

/// Whether an entry is a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// One entry of the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path below the volume, without a leading slash.
    pub path: String,
    /// File or directory.
    pub kind: Kind,
    /// The permission bits.
    pub mode: u32,
}

/// The release directory of `version`, below the volume.
fn release_dir(version: &str) -> String {
    format!("usr/local/js-agent/releases/{version}")
}

/// The files of the payload: path, mode.
fn files(version: &str) -> Vec<(String, u32)> {
    let release = release_dir(version);
    vec![
        (format!("{release}/js-agent"), 0o755),
        (format!("{release}/js-mac-session"), 0o755),
        (format!("{release}/js-mac-provision"), 0o755),
        (format!("{release}/LICENSE"), 0o644),
        (
            "usr/local/js-agent/share/com.komira.js-agent.plist".to_owned(),
            0o644,
        ),
        (
            "usr/local/js-agent/share/com.komira.js-mac-session.plist".to_owned(),
            0o644,
        ),
        (
            "usr/local/js-agent/share/updater.d/js-agent.toml".to_owned(),
            0o644,
        ),
    ]
}

/// The directories the package makes that hold no file of its own: the link's directory
/// (the update hook swaps the link by writing next to it), the log directory and the hook's
/// state directory.
const EMPTY_DIRS: [(&str, u32); 3] = [
    ("usr/local/js-agent/link", 0o755),
    ("Library/Logs/js-agent", 0o755),
    ("var/db/js-agent-update", 0o700),
];

/// The payload of the package of `version`: every file and every directory, parents first.
#[must_use]
pub fn entries(version: &str) -> Vec<Entry> {
    let mut all: BTreeMap<String, Entry> = BTreeMap::new();
    let leaves = files(version)
        .into_iter()
        .map(|(path, mode)| (path, Kind::File, mode))
        .chain(
            EMPTY_DIRS
                .iter()
                .map(|(path, mode)| ((*path).to_owned(), Kind::Dir, *mode)),
        );
    for (path, kind, mode) in leaves {
        for parent in Path::new(&path).ancestors().skip(1) {
            let parent = parent.to_string_lossy().into_owned();
            if !parent.is_empty() {
                all.entry(parent.clone()).or_insert(Entry {
                    path: parent,
                    kind: Kind::Dir,
                    mode: 0o755,
                });
            }
        }
        all.insert(path.clone(), Entry { path, kind, mode });
    }
    all.into_values().collect()
}

/// The `postinstall` script of the package of `version`.
#[must_use]
pub fn postinstall_script(version: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Written by `cargo xtask pkg stage-macos`. The logic is `js-mac-provision postinstall`.\n\
         exec \"${{3%/}}/{}/js-mac-provision\" postinstall --version {version} --volume \"$3\"\n",
        release_dir(version)
    )
}

/// The arguments of `pkg stage-macos`.
#[derive(clap::Args)]
pub struct StageArgs {
    /// The release version.
    #[arg(long)]
    pub version: Version,
    /// The directory with the built `js-agent`, `js-mac-session` and `js-mac-provision`.
    #[arg(long)]
    pub bin_dir: PathBuf,
    /// The license file.
    #[arg(long)]
    pub license: PathBuf,
    /// The agent's plist.
    #[arg(long)]
    pub agent_plist: PathBuf,
    /// The helper's plist.
    #[arg(long)]
    pub session_plist: PathBuf,
    /// The update hook's declaration.
    #[arg(long)]
    pub declaration: PathBuf,
    /// The directory to make, which must not exist: `root/` and `scripts/` go in it.
    #[arg(long)]
    pub out: PathBuf,
}

/// The arguments of `pkg check-macos`.
#[derive(clap::Args)]
pub struct CheckArgs {
    /// The package.
    #[arg(long)]
    pub pkg: PathBuf,
    /// The version it must be.
    #[arg(long)]
    pub version: Version,
}

/// What was read back from a package.
pub struct Inspected {
    /// `lsbom` of the component's bill of materials.
    pub bom: String,
    /// The files of the component's scripts: name and bytes.
    pub scripts: Vec<(String, Vec<u8>)>,
    /// The component's `PackageInfo`.
    pub package_info: String,
}

/// Why a verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A file could not be read or written.
    #[error("{0}")]
    Io(String),
    /// The package is not the package.
    #[error("the package is not the js-agent package: {}", .0.join("; "))]
    Refused(Vec<String>),
    /// `pkgutil` or `lsbom` failed.
    #[error("{0}")]
    Tool(String),
    /// The package is read with Apple's tools.
    #[error("pkg check-macos reads the package with pkgutil and lsbom, which only a Mac has")]
    NeedsMac,
}

fn io(path: &Path, why: impl std::fmt::Display) -> Error {
    Error::Io(format!("{}: {why}", path.display()))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| io(path, e))
}

/// Lays the payload and the scripts down in `args.out`: the line to print.
///
/// # Errors
/// [`Error`]: a source is unreadable, or the output directory exists.
pub fn stage(args: &StageArgs) -> Result<String, Error> {
    let version = args.version.to_string();
    let release = release_dir(&version);
    let sources: Vec<(String, PathBuf)> = vec![
        (format!("{release}/js-agent"), args.bin_dir.join("js-agent")),
        (
            format!("{release}/js-mac-session"),
            args.bin_dir.join("js-mac-session"),
        ),
        (
            format!("{release}/js-mac-provision"),
            args.bin_dir.join("js-mac-provision"),
        ),
        (format!("{release}/LICENSE"), args.license.clone()),
        (
            "usr/local/js-agent/share/com.komira.js-agent.plist".to_owned(),
            args.agent_plist.clone(),
        ),
        (
            "usr/local/js-agent/share/com.komira.js-mac-session.plist".to_owned(),
            args.session_plist.clone(),
        ),
        (
            "usr/local/js-agent/share/updater.d/js-agent.toml".to_owned(),
            args.declaration.clone(),
        ),
    ];
    let mut bytes = BTreeMap::new();
    for (dest, source) in sources {
        bytes.insert(dest, std::fs::read(&source).map_err(|e| io(&source, e))?);
    }
    if args.out.exists() {
        return Err(io(
            &args.out,
            "exists; stage into a directory that is not there",
        ));
    }
    let root = args.out.join("root");
    let all = entries(&version);
    for e in &all {
        let at = root.join(&e.path);
        match e.kind {
            Kind::Dir => std::fs::create_dir_all(&at).map_err(|x| io(&at, x))?,
            Kind::File => std::fs::write(&at, &bytes[&e.path]).map_err(|x| io(&at, x))?,
        }
        set_mode(&at, e.mode)?;
    }
    let scripts = args.out.join("scripts");
    std::fs::create_dir_all(&scripts).map_err(|e| io(&scripts, e))?;
    let script = scripts.join("postinstall");
    std::fs::write(&script, postinstall_script(&version)).map_err(|e| io(&script, e))?;
    set_mode(&script, 0o755)?;
    Ok(format!(
        "staged js-agent {version} with {} files and {} directories in {}",
        bytes.len(),
        all.len() - bytes.len(),
        args.out.display()
    ))
}

/// One line of `lsbom`: path, mode with the file type bits, owner and group.
fn bom_line(line: &str) -> Option<(String, u32, u32, u32)> {
    let mut fields = line.split('\t');
    let path = fields.next()?;
    let path = if path == "." {
        String::new()
    } else {
        path.strip_prefix("./")?.to_owned()
    };
    let mode = u32::from_str_radix(fields.next()?, 8).ok()?;
    let (uid, gid) = fields.next()?.split_once('/')?;
    Some((path, mode, uid.parse().ok()?, gid.parse().ok()?))
}

/// What is wrong with a package, one line each; empty when it is the package of `version`.
#[must_use]
pub fn problems(inspected: &Inspected, version: &str) -> Vec<String> {
    let mut found = Vec::new();
    let expected: BTreeMap<String, Entry> = entries(version)
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut listed = BTreeMap::new();
    for line in inspected.bom.lines().filter(|l| !l.trim().is_empty()) {
        let Some((path, mode, uid, gid)) = bom_line(line) else {
            found.push(format!("cannot read the bill of materials line {line:?}"));
            continue;
        };
        if path.is_empty() {
            continue;
        }
        if listed.insert(path.clone(), ()).is_some() {
            found.push(format!("{path} is listed twice"));
            continue;
        }
        let kind = match mode & 0o170_000 {
            0o040_000 => Kind::Dir,
            0o100_000 => Kind::File,
            _ => {
                found.push(format!(
                    "{path}: is not a file or a directory (mode {mode:o})"
                ));
                continue;
            }
        };
        let Some(want) = expected.get(&path) else {
            found.push(format!("not part of the package: {path}"));
            continue;
        };
        if want.kind != kind {
            let (got, wanted) = match kind {
                Kind::File => ("file", "directory"),
                Kind::Dir => ("directory", "file"),
            };
            found.push(format!("{path}: is a {got}, not a {wanted}"));
        } else if mode & 0o7777 != want.mode {
            found.push(format!(
                "{path}: mode is {:04o}, not {:04o}",
                mode & 0o7777,
                want.mode
            ));
        } else if uid != 0 || (gid != 0 && gid != 80) {
            found.push(format!("{path}: owned by {uid}/{gid}, not root:wheel"));
        }
    }
    for path in expected.keys().filter(|p| !listed.contains_key(*p)) {
        found.push(format!("missing from the package: {path}"));
    }
    let mut names: Vec<&str> = inspected.scripts.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    if names != ["postinstall"] {
        found.push(format!("the scripts are {names:?}, not [\"postinstall\"]"));
    }
    let script = postinstall_script(version);
    if inspected
        .scripts
        .iter()
        .any(|(n, b)| n == "postinstall" && b.as_slice() != script.as_bytes())
    {
        found.push(format!(
            "postinstall is not the script of version {version}"
        ));
    }
    if !inspected
        .package_info
        .contains(&format!("identifier=\"{IDENTIFIER}\""))
    {
        found.push(format!(
            "PackageInfo does not name the identifier {IDENTIFIER}"
        ));
    }
    if !inspected
        .package_info
        .contains(&format!(" version=\"{version}\""))
    {
        found.push(format!("PackageInfo does not name the version {version}"));
    }
    found
}

/// Reads the package with `pkgutil` and `lsbom`.
#[cfg(target_os = "macos")]
fn inspect(pkg: &Path) -> Result<Inspected, Error> {
    use std::process::Command;
    let tool = |program: &str, args: &[&Path]| -> Result<String, Error> {
        let out = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| Error::Tool(format!("{program}: {e}")))?;
        if !out.status.success() {
            return Err(Error::Tool(format!(
                "{program} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // One directory per call: tests of one process read packages side by side.
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("xtask-macpkg-{}-{call}", std::process::id()));
    tool("pkgutil", &[Path::new("--expand-full"), pkg, &dir])?;
    let read = || -> Result<Inspected, Error> {
        // A product archive holds the component in a directory of its own.
        let mut component = dir.clone();
        if !component.join("Bom").exists() {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(|e| io(&dir, e))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("Bom").exists())
                .collect();
            if inner.len() != 1 {
                return Err(Error::Tool(format!(
                    "{} holds {} components, not one",
                    pkg.display(),
                    inner.len()
                )));
            }
            component = inner.remove(0);
        }
        let bom = tool("lsbom", &[&component.join("Bom")])?;
        let mut scripts = Vec::new();
        if let Ok(items) = std::fs::read_dir(component.join("Scripts")) {
            for item in items.flatten() {
                let bytes = std::fs::read(item.path()).map_err(|e| io(&item.path(), e))?;
                scripts.push((item.file_name().to_string_lossy().into_owned(), bytes));
            }
        }
        let info = component.join("PackageInfo");
        let package_info = std::fs::read_to_string(&info).map_err(|e| io(&info, e))?;
        Ok(Inspected {
            bom,
            scripts,
            package_info,
        })
    };
    let inspected = read();
    let _ = std::fs::remove_dir_all(&dir);
    inspected
}

#[cfg(not(target_os = "macos"))]
fn inspect(_pkg: &Path) -> Result<Inspected, Error> {
    Err(Error::NeedsMac)
}

/// The line to print when `inspected` is the package of `version`; the problems otherwise.
///
/// # Errors
/// [`Error::Refused`].
pub fn verdict(inspected: &Inspected, version: &str, pkg: &Path) -> Result<String, Error> {
    let found = problems(inspected, version);
    if !found.is_empty() {
        return Err(Error::Refused(found));
    }
    Ok(format!(
        "{}: js-agent {version} with {} entries",
        pkg.display(),
        entries(version).len()
    ))
}

/// Reads the package with `pkgutil` and `lsbom` and refuses it unless it is the package.
///
/// # Errors
/// [`Error`].
pub fn check(args: &CheckArgs) -> Result<String, Error> {
    verdict(&inspect(&args.pkg)?, &args.version.to_string(), &args.pkg)
}
