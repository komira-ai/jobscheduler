//! What one probe found: an [`Outcome`] word, the error number when the system refused,
//! and a short detail, printed as one line `name: outcome (ERRNO); detail`.
//!
//! A line never carries the contents of a file the probe opened, only whether it opened and
//! its size, so a run prints no secret it reached.

use std::fmt;
use std::io;

/// The answer of one probe. The word is what the pinned answers name (`expected.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    /// The job could do it.
    Allowed,
    /// The system refused it (a permission error, or a tool that would not do it).
    Denied,
    /// Nothing to try here: the place does not exist on this host, or the probe cannot look.
    NotApplicable,
    /// A marker an earlier job left is still there.
    Found,
    /// A marker an earlier job left is not there.
    Gone,
    /// A resource limit has a finite soft value.
    Limited,
    /// A resource limit is infinite.
    Unlimited,
    /// A fact that is printed, never judged (free space).
    Info,
    /// The probe failed in a way that answers nothing (a timeout, an unexpected error).
    Error,
}

/// Every outcome, in the order of [`Outcome::word`]'s table.
const WORDS: [(Outcome, &str); 9] = [
    (Outcome::Allowed, "allowed"),
    (Outcome::Denied, "denied"),
    (Outcome::NotApplicable, "n/a"),
    (Outcome::Found, "found"),
    (Outcome::Gone, "gone"),
    (Outcome::Limited, "limited"),
    (Outcome::Unlimited, "unlimited"),
    (Outcome::Info, "info"),
    (Outcome::Error, "error"),
];

impl Outcome {
    /// The word printed and pinned for this outcome.
    #[must_use]
    pub fn word(self) -> &'static str {
        WORDS
            .iter()
            .find(|(o, _)| *o == self)
            .map_or("error", |(_, w)| w)
    }

    /// The outcome a word names, if any.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        WORDS.iter().find(|(_, w)| *w == word).map(|(o, _)| *o)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

/// One probe's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// `area:what`, the name the pinned answers use.
    pub name: String,
    /// The answer.
    pub outcome: Outcome,
    /// The error number the system answered with, when it refused.
    pub errno: Option<i32>,
    /// What else is worth saying, short; empty for nothing.
    pub detail: String,
}

impl Line {
    /// A line with no error number and no detail.
    #[must_use]
    pub fn new(name: impl Into<String>, outcome: Outcome) -> Self {
        Self {
            name: name.into(),
            outcome,
            errno: None,
            detail: String::new(),
        }
    }

    /// The same line with `detail`.
    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// The same line with the error number of `err`, if it has one.
    #[must_use]
    pub fn errno_of(mut self, err: &io::Error) -> Self {
        self.errno = err.raw_os_error();
        self
    }

    /// A line for a call that failed with `err`: [`Outcome::Denied`] for a permission error,
    /// [`Outcome::Error`] for anything else, with the error number and `detail`.
    #[must_use]
    pub fn refused(name: impl Into<String>, err: &io::Error, detail: impl Into<String>) -> Self {
        Self::new(name, refusal(err)).errno_of(err).detail(detail)
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.outcome)?;
        if let Some(errno) = self.errno {
            write!(f, " ({})", errno_name(errno))?;
        }
        if !self.detail.is_empty() {
            write!(f, "; {}", self.detail)?;
        }
        Ok(())
    }
}

/// The outcome a failed call means: [`Outcome::Denied`] when the system refused for
/// permission (`EPERM`, which the sandbox answers, `EACCES`, which the file modes answer, or
/// `EROFS`), [`Outcome::Error`] otherwise.
#[must_use]
pub fn refusal(err: &io::Error) -> Outcome {
    match err.raw_os_error() {
        Some(libc::EPERM | libc::EACCES | libc::EROFS) => Outcome::Denied,
        _ => Outcome::Error,
    }
}

/// The names of the error numbers a probe meets, by this platform's values.
const ERRNOS: [(i32, &str); 22] = [
    (libc::EPERM, "EPERM"),
    (libc::ENOENT, "ENOENT"),
    (libc::ESRCH, "ESRCH"),
    (libc::EACCES, "EACCES"),
    (libc::EEXIST, "EEXIST"),
    (libc::ENOTDIR, "ENOTDIR"),
    (libc::EISDIR, "EISDIR"),
    (libc::EINVAL, "EINVAL"),
    (libc::ENFILE, "ENFILE"),
    (libc::EMFILE, "EMFILE"),
    (libc::ENOSPC, "ENOSPC"),
    (libc::EROFS, "EROFS"),
    (libc::EAGAIN, "EAGAIN"),
    (libc::ENOSYS, "ENOSYS"),
    (libc::EDQUOT, "EDQUOT"),
    (libc::ECONNREFUSED, "ECONNREFUSED"),
    (libc::ETIMEDOUT, "ETIMEDOUT"),
    (libc::EHOSTUNREACH, "EHOSTUNREACH"),
    (libc::ENETUNREACH, "ENETUNREACH"),
    (libc::EADDRNOTAVAIL, "EADDRNOTAVAIL"),
    (libc::ENOTSUP, "ENOTSUP"),
    (libc::ELOOP, "ELOOP"),
];

/// The symbolic name of `errno` (`EPERM`), or `errno N` for one the table does not hold.
#[must_use]
pub fn errno_name(errno: i32) -> String {
    ERRNOS
        .iter()
        .find(|(n, _)| *n == errno)
        .map_or_else(|| format!("errno {errno}"), |(_, name)| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_parses_back_to_its_outcome() {
        for (outcome, word) in WORDS {
            assert_eq!(outcome.word(), word);
            assert_eq!(Outcome::parse(word), Some(outcome));
            assert_eq!(outcome.to_string(), word);
        }
        assert_eq!(Outcome::parse("maybe"), None);
    }

    #[test]
    fn a_line_prints_name_outcome_errno_and_detail() {
        let err = io::Error::from_raw_os_error(libc::EPERM);
        let line = Line::refused("write:etc", &err, "/private/etc");
        assert_eq!(line.to_string(), "write:etc: denied (EPERM); /private/etc");
        assert_eq!(
            Line::new("write:run-dir", Outcome::Allowed).to_string(),
            "write:run-dir: allowed"
        );
        assert_eq!(
            Line::new("x:y", Outcome::Info).detail("d").to_string(),
            "x:y: info; d"
        );
    }

    #[test]
    fn only_permission_errors_are_denials() {
        for errno in [libc::EPERM, libc::EACCES, libc::EROFS] {
            assert_eq!(
                refusal(&io::Error::from_raw_os_error(errno)),
                Outcome::Denied
            );
        }
        for errno in [libc::ENOENT, libc::EINVAL, libc::ENOSPC] {
            assert_eq!(
                refusal(&io::Error::from_raw_os_error(errno)),
                Outcome::Error
            );
        }
        assert_eq!(refusal(&io::Error::other("no number")), Outcome::Error);
    }

    #[test]
    fn errno_names_come_from_the_table_or_the_number() {
        for (errno, name) in ERRNOS {
            assert_eq!(errno_name(errno), name);
        }
        assert_eq!(errno_name(100_000), "errno 100000");
    }

    #[test]
    fn a_line_without_an_os_error_carries_no_errno() {
        let line = Line::refused("a:b", &io::Error::other("x"), "");
        assert_eq!(line.errno, None);
        assert_eq!(line.to_string(), "a:b: error");
    }
}
