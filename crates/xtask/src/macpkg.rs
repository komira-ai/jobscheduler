//! The js-agent installer package for macOS (`packaging/js-agent-macos`).
//!
//! The package is a hostkeeper `macos-pkg` service: it lays its files down and runs no script.
//! `pkg entitle-macos` signs the one program that needs an entitlement, `js-vmm`, ad hoc with
//! the hardened runtime and the virtualization entitlement, in place, before staging.
//! `pkg stage-macos` lays the payload down in a directory from the built programs, the
//! licence and the two plists; the workflow gives that directory to `pkgbuild` without
//! `--scripts`. `pkg check-macos` reads the package back with Apple's `pkgutil`, `lsbom` and
//! `codesign` and refuses it unless it holds exactly [`entries`], each with its mode and root
//! as owner, no script at all, the identifier and version of the release, and programs whose
//! signatures verify and carry exactly the entitlements of [`entitlements_of`].

use std::collections::{BTreeMap, BTreeSet};
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

/// The directory of the package's own files, below the volume.
const PREFIX: &str = "Library/Application Support/js-agent";

/// The programs of the package, in `bin/` below [`PREFIX`]: the agent, its root helper, the
/// install-time tool and the VM launcher the agent starts (`--vm-driver`).
pub const PROGRAMS: [&str; 4] = ["js-agent", "js-mac-session", "js-mac-provision", "js-vmm"];

/// The entitlement Virtualization.framework asks of a process that starts a virtual machine.
/// It needs no provisioning profile, so an ad hoc signature can carry it.
pub const VIRTUALIZATION: &str = "com.apple.security.virtualization";

/// The entitlements `program` is signed with: [`VIRTUALIZATION`] for `js-vmm`, none for every
/// other program.
#[must_use]
pub fn entitlements_of(program: &str) -> &'static [&'static str] {
    if program == "js-vmm" {
        &[VIRTUALIZATION]
    } else {
        &[]
    }
}

/// The entitlements file `codesign --entitlements` reads: each key of `keys` set to true.
#[must_use]
pub fn entitlements_plist(keys: &[&str]) -> String {
    let mut text = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n",
    ));
    for key in keys {
        text.push_str(&format!("\t<key>{key}</key>\n\t<true/>\n"));
    }
    text.push_str("</dict>\n</plist>\n");
    text
}

/// The files of the payload: path, mode.
fn files() -> Vec<(String, u32)> {
    let mut files: Vec<(String, u32)> = PROGRAMS
        .iter()
        .map(|program| (format!("{PREFIX}/bin/{program}"), 0o755))
        .collect();
    files.extend([
        (format!("{PREFIX}/LICENSE"), 0o644),
        ("Library/LaunchDaemons/js-agent.plist".to_owned(), 0o644),
        (
            "Library/LaunchDaemons/js-agent.session.plist".to_owned(),
            0o644,
        ),
    ]);
    files
}

/// The directories the package makes that hold no file of its own: the log directory the
/// daemons write to and the update hook's state directory (`state_dir` of the hostkeeper
/// package's params).
const EMPTY_DIRS: [(&str, u32); 2] = [
    ("Library/Application Support/js-agent/log", 0o755),
    ("Library/Application Support/js-agent/update", 0o700),
];

/// The payload of the package: every file and every directory, parents first.
#[must_use]
pub fn entries() -> Vec<Entry> {
    let mut all: BTreeMap<String, Entry> = BTreeMap::new();
    let leaves = files()
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

/// The arguments of `pkg stage-macos`.
#[derive(clap::Args)]
pub struct StageArgs {
    /// The release version.
    #[arg(long)]
    pub version: Version,
    /// The directory with the built `js-agent`, `js-mac-session`, `js-mac-provision` and
    /// `js-vmm` (the last signed by `pkg entitle-macos`).
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
    /// The directory to make, which must not exist: `root/` goes in it.
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

/// The arguments of `pkg entitle-macos`.
#[derive(clap::Args)]
pub struct EntitleArgs {
    /// The directory with the built `js-vmm`, which is signed in place.
    #[arg(long)]
    pub bin_dir: PathBuf,
}

/// What `codesign` says of one program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The program's name, one of [`PROGRAMS`].
    pub program: String,
    /// `codesign --verify --strict`: what it printed when it refused the program.
    pub verified: Result<(), String>,
    /// `codesign --display --verbose=4` (its standard error, which holds the code directory
    /// flags).
    pub display: String,
    /// `codesign --display --entitlements - --xml` (its standard output: empty when the
    /// program has no entitlement).
    pub entitlements: String,
}

/// What was read back from a package.
pub struct Inspected {
    /// `lsbom` of the component's bill of materials.
    pub bom: String,
    /// The names of the files of the component's scripts: none in the package.
    pub scripts: Vec<String>,
    /// The component's `PackageInfo`.
    pub package_info: String,
    /// The signatures of the payload's programs.
    pub signatures: Vec<Signature>,
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
    /// `pkgutil`, `lsbom` or `codesign` failed.
    #[error("{0}")]
    Tool(String),
    /// A program `pkg entitle-macos` signed does not carry the signature it was given.
    #[error("the signed program is not as the package needs it: {}", .0.join("; "))]
    Unsigned(Vec<String>),
    /// The package is read, and `js-vmm` signed, with Apple's tools.
    #[error(
        "pkg check-macos and pkg entitle-macos run pkgutil, lsbom and codesign, which only a Mac has"
    )]
    NeedsMac,
}

fn io(path: &Path, why: impl std::fmt::Display) -> Error {
    Error::Io(format!("{}: {why}", path.display()))
}

/// The error of a failed operation on `path`, for `map_err`.
fn failure(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |why| io(path, why)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(failure(path))
}

/// Lays the payload down in `args.out`: the line to print.
///
/// # Errors
/// [`Error`]: a source is unreadable, or the output directory exists.
pub fn stage(args: &StageArgs) -> Result<String, Error> {
    let version = args.version.to_string();
    let mut sources: Vec<(String, PathBuf)> = PROGRAMS
        .iter()
        .map(|program| {
            (
                format!("{PREFIX}/bin/{program}"),
                args.bin_dir.join(program),
            )
        })
        .collect();
    sources.extend([
        (format!("{PREFIX}/LICENSE"), args.license.clone()),
        (
            "Library/LaunchDaemons/js-agent.plist".to_owned(),
            args.agent_plist.clone(),
        ),
        (
            "Library/LaunchDaemons/js-agent.session.plist".to_owned(),
            args.session_plist.clone(),
        ),
    ]);
    let mut bytes = BTreeMap::new();
    for (dest, source) in sources {
        bytes.insert(dest, std::fs::read(&source).map_err(failure(&source))?);
    }
    if args.out.exists() {
        return Err(io(
            &args.out,
            "exists; stage into a directory that is not there",
        ));
    }
    let root = args.out.join("root");
    let all = entries();
    for e in &all {
        let at = root.join(&e.path);
        match e.kind {
            Kind::Dir => std::fs::create_dir_all(&at).map_err(failure(&at))?,
            Kind::File => std::fs::write(&at, &bytes[&e.path]).map_err(failure(&at))?,
        }
        set_mode(&at, e.mode)?;
    }
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
    let expected: BTreeMap<String, Entry> =
        entries().into_iter().map(|e| (e.path.clone(), e)).collect();
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
    let mut names: Vec<&str> = inspected.scripts.iter().map(String::as_str).collect();
    names.sort_unstable();
    if !names.is_empty() {
        found.push(format!(
            "the package holds scripts, and must hold none: {names:?}"
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
    for program in PROGRAMS {
        match inspected.signatures.iter().find(|s| s.program == program) {
            Some(signature) => found.extend(signature_problems(signature)),
            None => found.push(format!("bin/{program}: its signature was not read")),
        }
    }
    found
}

/// The names of the code directory flags of `codesign --display --verbose=4`: `adhoc` and
/// `runtime` of `flags=0x10002(adhoc,runtime)`.
fn code_flags(display: &str) -> BTreeSet<String> {
    display
        .lines()
        .find_map(|l| l.split_once("flags=").map(|(_, rest)| rest))
        .and_then(|flags| flags.split_once('('))
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(inside, _)| inside.split(',').map(|f| f.trim().to_owned()).collect())
        .unwrap_or_default()
}

/// The entitlements of `codesign --display --entitlements - --xml`, each key with its value
/// (`true`, or what else it is).
fn granted(xml: &str) -> Result<BTreeMap<String, String>, String> {
    if xml.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let value = plist::Value::from_reader_xml(xml.as_bytes()).map_err(|e| e.to_string())?;
    let dict = value
        .into_dictionary()
        .ok_or_else(|| "it is not a dictionary".to_owned())?;
    Ok(dict
        .into_iter()
        .map(|(key, value)| {
            let shown = match value {
                plist::Value::Boolean(true) => "true".to_owned(),
                other => format!("{other:?}"),
            };
            (key, shown)
        })
        .collect())
}

/// What is wrong with the signature of one program, one line each: `codesign --verify
/// --strict` refuses it, its entitlements are not exactly [`entitlements_of`] it, each set to
/// true, or it has an entitlement and no hardened runtime.
#[must_use]
pub fn signature_problems(signature: &Signature) -> Vec<String> {
    let program = &signature.program;
    let mut found = Vec::new();
    if let Err(why) = &signature.verified {
        found.push(format!(
            "bin/{program}: codesign --verify --strict refuses it: {}",
            why.trim()
        ));
    }
    let want: BTreeMap<String, String> = entitlements_of(program)
        .iter()
        .map(|key| ((*key).to_owned(), "true".to_owned()))
        .collect();
    match granted(&signature.entitlements) {
        Err(why) => found.push(format!(
            "bin/{program}: cannot read its entitlements: {why}"
        )),
        Ok(got) if got != want => found.push(format!(
            "bin/{program}: carries the entitlements {got:?}, not {want:?}"
        )),
        Ok(_) => {}
    }
    if !want.is_empty() && !code_flags(&signature.display).contains("runtime") {
        found.push(format!(
            "bin/{program}: is not signed with the hardened runtime"
        ));
    }
    found
}

/// Apple's code signing tool, by its absolute path.
#[cfg(target_os = "macos")]
const CODESIGN: &str = "/usr/bin/codesign";

/// Runs `program`: its exit status and both outputs, whatever the status.
#[cfg(target_os = "macos")]
fn output(program: &str, args: &[&std::ffi::OsStr]) -> Result<std::process::Output, Error> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Tool(format!("{program}: {e}")))
}

/// What `codesign` says of `program` at `path`.
#[cfg(target_os = "macos")]
fn read_signature(program: &str, path: &Path) -> Result<Signature, Error> {
    let path = path.as_os_str();
    let verify = output(CODESIGN, &["--verify".as_ref(), "--strict".as_ref(), path])?;
    let display = output(
        CODESIGN,
        &["--display".as_ref(), "--verbose=4".as_ref(), path],
    )?;
    let entitlements = output(
        CODESIGN,
        &[
            "--display".as_ref(),
            "--entitlements".as_ref(),
            "-".as_ref(),
            "--xml".as_ref(),
            path,
        ],
    )?;
    Ok(Signature {
        program: program.to_owned(),
        verified: if verify.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&verify.stderr).into_owned())
        },
        display: String::from_utf8_lossy(&display.stderr).into_owned(),
        entitlements: String::from_utf8_lossy(&entitlements.stdout).into_owned(),
    })
}

/// Signs each program of `bin_dir` that has an entitlement ad hoc, with the hardened runtime
/// and its entitlements, and reads the signature back.
#[cfg(target_os = "macos")]
fn sign_programs(bin_dir: &Path) -> Result<String, Error> {
    let mut found = Vec::new();
    let mut signed = Vec::new();
    for program in PROGRAMS.iter().filter(|p| !entitlements_of(p).is_empty()) {
        let path = bin_dir.join(program);
        // One file per call: tests of one process sign side by side.
        static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let plist = std::env::temp_dir().join(format!(
            "xtask-entitle-{}-{call}-{program}.plist",
            std::process::id()
        ));
        std::fs::write(&plist, entitlements_plist(entitlements_of(program)))
            .map_err(failure(&plist))?;
        let out = output(
            CODESIGN,
            &[
                "--force".as_ref(),
                "--sign".as_ref(),
                "-".as_ref(),
                "--options".as_ref(),
                "runtime".as_ref(),
                "--entitlements".as_ref(),
                plist.as_os_str(),
                path.as_os_str(),
            ],
        );
        let _ = std::fs::remove_file(&plist);
        let out = out?;
        if !out.status.success() {
            return Err(Error::Tool(format!(
                "codesign {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        found.extend(signature_problems(&read_signature(program, &path)?));
        signed.push(*program);
    }
    if !found.is_empty() {
        return Err(Error::Unsigned(found));
    }
    Ok(format!(
        "signed {} ad hoc with the hardened runtime and its entitlements",
        signed.join(", ")
    ))
}

#[cfg(not(target_os = "macos"))]
fn sign_programs(_bin_dir: &Path) -> Result<String, Error> {
    Err(Error::NeedsMac)
}

/// Signs `js-vmm` in `args.bin_dir` in place: ad hoc, with the hardened runtime and
/// [`VIRTUALIZATION`], and refuses it unless `codesign` then reads that signature back.
/// The other programs keep the linker's signature (the helper's plist names `js-agent` by the
/// code directory hash of that signature).
///
/// # Errors
/// [`Error`]: not a Mac, `codesign` failed, or the signature read back is not the one given.
pub fn entitle(args: &EntitleArgs) -> Result<String, Error> {
    sign_programs(&args.bin_dir)
}

/// Reads the package with `pkgutil`, `lsbom` and `codesign`.
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
                scripts.push(item.file_name().to_string_lossy().into_owned());
            }
        }
        let info = component.join("PackageInfo");
        let package_info = std::fs::read_to_string(&info).map_err(|e| io(&info, e))?;
        // `--expand-full` lays the payload down under `Payload/`; a program it lacks is
        // reported by the bill of materials and as a signature not read.
        let mut signatures = Vec::new();
        for program in PROGRAMS {
            let path = component
                .join("Payload")
                .join(PREFIX)
                .join("bin")
                .join(program);
            if path.is_file() {
                signatures.push(read_signature(program, &path)?);
            }
        }
        Ok(Inspected {
            bom,
            scripts,
            package_info,
            signatures,
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
        entries().len()
    ))
}

/// Reads the package with `pkgutil`, `lsbom` and `codesign` and refuses it unless it is the
/// package.
///
/// # Errors
/// [`Error`].
pub fn check(args: &CheckArgs) -> Result<String, Error> {
    verdict(&inspect(&args.pkg)?, &args.version.to_string(), &args.pkg)
}
