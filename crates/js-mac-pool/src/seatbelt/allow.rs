//! The allow-list read model: the base set of a macOS major version and the profile
//! lines that turn a [`Policy`](super::Policy)'s grants into reads.
//!
//! The base set is a starting point from the sandbox's own trace facility on one macOS
//! major version, reviewed line by line; a trace line enters it as a literal or a
//! narrow subpath, never as a directory-wide grant ([`is_directory_wide`]). A major
//! version without a golden has no base set, so [`MacosMajor`] has no value for it and
//! no profile of the allow model exists for it: the host's own version decides, and an
//! unknown one fails closed. The set is tested twice: as text, against a golden per
//! major version, and on the running host, by the start-up self-test
//! ([`SelfTest`](super::SelfTest)), which is the only check that tracks the macOS build
//! actually running.

use super::grant::{Match, XCODE_SUPPORT, kind_rules};
use super::path::AbsPath;
use super::policy::AllowGrants;
use super::scan::{PrivilegedScan, scan_privileged};

/// A macOS major version that has a base set.
///
/// The agent's duties around it: a host whose version has no value here refuses the allow
/// model (`from_product_version` returns `None`; never default to the newest known
/// version), and an [`Assurance`](super::Assurance) is made again at every start and after
/// every OS update, never kept across them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MacosMajor {
    /// macOS 14 (Sonoma).
    Macos14,
    /// macOS 15 (Sequoia).
    Macos15,
}

impl MacosMajor {
    /// Every version that has a base set.
    pub const ALL: [Self; 2] = [Self::Macos14, Self::Macos15];

    /// The version with major number `major`, if it has a base set.
    #[must_use]
    pub fn from_major(major: u32) -> Option<Self> {
        match major {
            14 => Some(Self::Macos14),
            15 => Some(Self::Macos15),
            _ => None,
        }
    }

    /// The version of a `sw_vers -productVersion` text (`14.5`, `15.8.2`), if it has a
    /// base set.
    #[must_use]
    pub fn from_product_version(text: &str) -> Option<Self> {
        let first = text.trim().split('.').next();
        first
            .and_then(|major| major.parse().ok())
            .and_then(Self::from_major)
    }

    /// The major number.
    #[must_use]
    pub fn major(self) -> u32 {
        match self {
            Self::Macos14 => 14,
            Self::Macos15 => 15,
        }
    }

    /// The setuid and setgid files of this host under the directories the base set
    /// grants. Run at start and put in the fixture of the self-test; a file it finds
    /// that [`BaseSet::setuid`] does not list refuses the allow model. The agent runs it
    /// as root: a directory it cannot read is skipped silently.
    #[must_use]
    pub fn scan(self) -> PrivilegedScan {
        let roots: Vec<&str> = self
            .base()
            .reads()
            .iter()
            .filter(|(how, _)| *how == Match::Subpath)
            .map(|(_, path)| *path)
            .collect();
        scan_privileged(&roots)
    }

    /// The base set of this version.
    #[must_use]
    pub fn base(self) -> &'static BaseSet {
        match self {
            Self::Macos14 => &MACOS_14,
            Self::Macos15 => &MACOS_15,
        }
    }
}

/// What every run of the allow-list model may read, and the setuid files it may not.
#[derive(Debug, PartialEq, Eq)]
pub struct BaseSet {
    names: &'static [&'static str],
    reads: &'static [(Match, &'static str)],
    setuid: &'static [&'static str],
}

impl BaseSet {
    /// The names (file metadata only, not content) of the links at the root and the
    /// directories above the base set: a path spelled through `/var` or `/etc` is
    /// resolved by looking at the link, and a tool that is refused that gets `EPERM`
    /// for a file it may write.
    #[must_use]
    pub fn names(&self) -> &'static [&'static str] {
        self.names
    }

    /// The reads: the system directories a process needs to start and link, and the
    /// few device files.
    #[must_use]
    pub fn reads(&self) -> &'static [(Match, &'static str)] {
        self.reads
    }

    /// The setuid files of the version's image, denied by literal after the grants:
    /// reads gate `exec`, and a pool user must not be able to run `sudo` or `su`.
    #[must_use]
    pub fn setuid(&self) -> &'static [&'static str] {
        self.setuid
    }
}

/// `(literal "/")` is a data read, not only a name: the hosted runner showed every
/// sandboxed step, `true` included, refused when it was metadata only (the kernel reads
/// `/` to resolve paths). The root's entry names are therefore listable under this model;
/// nothing below them is.
static MACOS_14: BaseSet = BaseSet {
    names: &["/System", "/etc", "/tmp", "/usr", "/var"],
    reads: &[
        (Match::Literal, "/"),
        (Match::Subpath, "/System/Library"),
        (Match::Subpath, "/System/Cryptexes"),
        (Match::Subpath, "/System/Volumes/Preboot/Cryptexes"),
        (Match::Subpath, "/usr/lib"),
        (Match::Subpath, "/usr/bin"),
        (Match::Subpath, "/usr/sbin"),
        (Match::Subpath, "/usr/libexec"),
        (Match::Subpath, "/usr/share"),
        (Match::Subpath, "/bin"),
        (Match::Subpath, "/sbin"),
        (Match::Subpath, "/private/var/db/dyld"),
        (Match::Subpath, "/private/var/db/timezone"),
        (Match::Literal, "/private/var/select/sh"),
        (Match::Literal, "/dev/null"),
        (Match::Literal, "/dev/zero"),
        (Match::Literal, "/dev/random"),
        (Match::Literal, "/dev/urandom"),
        (Match::Literal, "/dev/tty"),
    ],
    setuid: &[
        "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent",
        "/bin/ps",
        "/sbin/ping",
        "/usr/bin/at",
        "/usr/bin/atq",
        "/usr/bin/atrm",
        "/usr/bin/batch",
        "/usr/bin/crontab",
        "/usr/bin/login",
        "/usr/bin/newgrp",
        "/usr/bin/quota",
        "/usr/bin/su",
        "/usr/bin/sudo",
        "/usr/bin/top",
        "/usr/bin/write",
        "/usr/libexec/authopen",
        "/usr/libexec/security_authtrampoline",
        "/usr/sbin/postdrop",
        "/usr/sbin/postqueue",
        "/usr/sbin/traceroute",
        "/usr/sbin/traceroute6",
    ],
};

fn filter((how, path): (Match, &str)) -> String {
    match how {
        Match::Literal => format!("(literal \"{path}\")"),
        Match::Subpath => format!("(subpath \"{path}\")"),
    }
}

fn filters(rules: &[(Match, &str)]) -> String {
    let each: Vec<String> = rules.iter().copied().map(filter).collect();
    each.join(" ")
}

fn param(kind: &str, name: &str) -> String {
    format!("({kind} (param \"{name}\"))")
}

/// The read section of the allow model, emitted after the write grants: the default
/// deny, names (metadata only) of the ancestors of every granted place, the base set,
/// the run's own places, the pinned trees, the Xcode, the kinds, and the setuid denies.
/// The grants' `-D` parameters are appended after `RUN` and `UENTRY`.
pub(super) fn reads(
    allow: &AllowGrants,
    uid_entry: Option<&AbsPath>,
    lines: &mut Vec<String>,
    params: &mut Vec<(String, String)>,
) {
    let mut named = vec!["RUN".to_owned()];
    let mut own = vec![param("subpath", "RUN")];
    if uid_entry.is_some() {
        named.push("UENTRY".to_owned());
        own.push(param("subpath", "UENTRY"));
    }
    let mut trees = Vec::new();
    for (i, tree) in allow.trees().iter().enumerate() {
        params.push((format!("TREE_{i}"), tree.path().as_str().to_owned()));
        named.push(format!("TREE_{i}"));
        trees.push(param("subpath", &format!("TREE_{i}")));
    }
    if let Some(xcode) = allow.xcode() {
        params.push(("XCODE_APP".to_owned(), xcode.app().as_str().to_owned()));
        named.push("XCODE_APP".to_owned());
    }
    let base = allow.major().base();

    lines.push("(deny file-read*)".to_owned());
    let ancestors: Vec<String> = named.iter().map(|n| param("path-ancestors", n)).collect();
    lines.push(format!(
        "(allow file-read-metadata {})",
        ancestors.join(" ")
    ));
    let names: Vec<String> = base
        .names()
        .iter()
        .map(|path| filter((Match::Literal, *path)))
        .collect();
    lines.push(format!("(allow file-read-metadata {})", names.join(" ")));
    lines.push(format!("(allow file-read* {})", filters(base.reads())));
    lines.push(format!("(allow file-read* {})", own.join(" ")));
    if !trees.is_empty() {
        lines.push(format!("(allow file-read* {})", trees.join(" ")));
    }
    if allow.xcode().is_some() {
        lines.push(format!(
            "(allow file-read* {} {})",
            param("subpath", "XCODE_APP"),
            filters(&XCODE_SUPPORT)
        ));
    }
    for kind in allow.kinds() {
        let rules = kind_rules(*kind);
        if !rules.is_empty() {
            lines.push(format!("(allow file-read* {})", filters(rules)));
        }
    }
    let setuid: Vec<String> = base
        .setuid()
        .iter()
        .map(|path| filter((Match::Literal, *path)))
        .collect();
    lines.push(format!("(deny file-read* {})", setuid.join(" ")));
}

/// The macOS 15 set, from a trace on the hosted macOS 15.8.2 image (build 24H232). The
/// reference workloads (`true`, a read of a granted tree, a bundle entrypoint, a
/// compile and an ad-hoc `codesign` with the selected Xcode, a read through the time
/// zone link) ran under the macOS 14 set with each line removed in turn, and `fs_usage`
/// listed the paths they touch. A line is here only when a workload touched it or
/// failed without it; the lines of the macOS 14 set that neither happened to are not
/// carried over, so this set reads nothing the macOS 14 set does not. Run alone, the
/// macOS 14 set passed every workload and every denied read on this image, and the
/// image's setuid files under the 14 roots are the same twenty.
///
/// - names: `/var` is needed (the compile and the time zone read fail without it);
///   `/etc` and `/usr` are looked at by `codesign`.
/// - `/` is a data read, as in macOS 14: every workload fails without it.
/// - `/System/Library`, `/usr/lib`, `/usr/bin`, `/usr/share`, `/bin`: touched by every
///   dynamically linked workload and by the compile (frameworks, `dyld`, the tools, the
///   ICU data).
/// - `/System/Volumes/Preboot/Cryptexes`: the system frameworks are opened through the
///   `OS` cryptex. `/System/Cryptexes` was not touched and is not carried over.
/// - `/private/var/db/timezone`: needed (the read through `/private/etc/localtime`
///   fails without it).
/// - the five device files: each is opened by a workload.
/// - not carried over, because nothing touched them and no workload failed without them:
///   `/System/Cryptexes`, `/usr/sbin`, `/usr/libexec`, `/sbin`, `/private/var/db/dyld`,
///   `/private/var/select/sh` and the names `/System` and `/tmp`.
/// - setuid: the files of the 20 found under the macOS 14 roots that lie under a
///   directory this set grants; the others are not readable by default.
static MACOS_15: BaseSet = BaseSet {
    names: &["/etc", "/usr", "/var"],
    reads: &[
        (Match::Literal, "/"),
        (Match::Subpath, "/System/Library"),
        (Match::Subpath, "/System/Volumes/Preboot/Cryptexes"),
        (Match::Subpath, "/usr/lib"),
        (Match::Subpath, "/usr/bin"),
        (Match::Subpath, "/usr/share"),
        (Match::Subpath, "/bin"),
        (Match::Subpath, "/private/var/db/timezone"),
        (Match::Literal, "/dev/null"),
        (Match::Literal, "/dev/zero"),
        (Match::Literal, "/dev/random"),
        (Match::Literal, "/dev/urandom"),
        (Match::Literal, "/dev/tty"),
    ],
    setuid: &[
        "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent",
        "/bin/ps",
        "/usr/bin/at",
        "/usr/bin/atq",
        "/usr/bin/atrm",
        "/usr/bin/batch",
        "/usr/bin/crontab",
        "/usr/bin/login",
        "/usr/bin/newgrp",
        "/usr/bin/quota",
        "/usr/bin/su",
        "/usr/bin/sudo",
        "/usr/bin/top",
        "/usr/bin/write",
    ],
};
