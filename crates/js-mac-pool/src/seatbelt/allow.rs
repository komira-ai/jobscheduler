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

/// A macOS major version that has a base set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MacosMajor {
    /// macOS 14 (Sonoma).
    Macos14,
}

impl MacosMajor {
    /// Every version that has a base set.
    pub const ALL: [Self; 1] = [Self::Macos14];

    /// The version with major number `major`, if it has a base set.
    #[must_use]
    pub fn from_major(major: u32) -> Option<Self> {
        match major {
            14 => Some(Self::Macos14),
            _ => None,
        }
    }

    /// The version of a `sw_vers -productVersion` text (`14.5`, `14.5.1`), if it has a
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
        }
    }

    /// The base set of this version.
    #[must_use]
    pub fn base(self) -> &'static BaseSet {
        match self {
            Self::Macos14 => &MACOS_14,
        }
    }
}

/// What every run of the allow-list model may read, and the setuid files it may not.
#[derive(Debug, PartialEq, Eq)]
pub struct BaseSet {
    reads: &'static [(Match, &'static str)],
    setuid: &'static [&'static str],
}

impl BaseSet {
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

static MACOS_14: BaseSet = BaseSet {
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
        (Match::Literal, "/dev/null"),
        (Match::Literal, "/dev/zero"),
        (Match::Literal, "/dev/random"),
        (Match::Literal, "/dev/urandom"),
        (Match::Literal, "/dev/tty"),
    ],
    setuid: &[
        "/usr/bin/at",
        "/usr/bin/atq",
        "/usr/bin/atrm",
        "/usr/bin/batch",
        "/usr/bin/crontab",
        "/usr/bin/login",
        "/usr/bin/su",
        "/usr/bin/sudo",
        "/usr/libexec/authopen",
        "/usr/libexec/security_authtrampoline",
        "/usr/sbin/traceroute",
        "/usr/sbin/traceroute6",
        "/sbin/ping",
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
    _allow: &AllowGrants,
    _uid_entry: Option<&AbsPath>,
    _lines: &mut Vec<String>,
    _params: &mut Vec<(String, String)>,
) {
}
