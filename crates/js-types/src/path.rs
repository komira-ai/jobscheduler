//! The rules for the paths a job names: one implementation, so that the server, when it
//! takes a job, and the agent, when it takes a start, check the same text with the same
//! code (`docs/design/job-rootfs.md`, section 2.3).
//!
//! Every rule reads the text alone: it splits on `/` (never through `std::path`, so the
//! answer is the same on every OS it runs on) and touches no file system. Whether a path
//! exists, or where a link in it leads, is the agent's to find out when it opens the path.
//!
//! | Rule | Used for |
//! |---|---|
//! | [`relative`] | the run directory's paths: a working directory, an input's `dest`, an output |
//! | [`absolute`] | a program, and a host path (of a layer or a mount) |
//! | [`target`] | where a mount appears in a job's file system |
//! | [`disjoint`] | relative paths that may neither repeat nor lie below one another |
//! | [`distinct`] | mount targets, which may lie below one another but not repeat |
//!
//! A fault is a [`PathFault`]: which rule broke, never a byte of the path.

use crate::text::hidden;

/// The longest path any rule here accepts, in bytes: a relative path, an absolute one and
/// a mount target, its leading `/` included.
pub const MAX_PATH_LEN: usize = 4096;

/// The places of a job's file system the runtime keeps for itself; a mount may name none
/// of them, nor anything below one. Compared in lower case.
pub const RUNTIME_PLACES: [&str; 9] = [
    "/proc",
    "/dev",
    "/sys",
    "/run/js-files",
    "/scratch",
    "/root",
    "/tmp",
    "/var/tmp",
    "/run",
];

/// Which rule a path broke. The text is fixed: it never holds a byte of the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathFault {
    /// The path is empty, or has an empty or a `.` part (a `/` at its end, or two in a
    /// row), or is a target of `/` alone.
    #[error("is empty or has an empty or `.` part")]
    Empty,
    /// The path is longer than [`MAX_PATH_LEN`].
    #[error("is longer than the cap")]
    TooLong,
    /// The path holds a character its rule does not allow.
    #[error("holds a character that is not allowed")]
    BadChars,
    /// The path leaves where it is meant to be: a `..` part, a relative path that starts
    /// with `/`, or an absolute one that does not.
    #[error("leaves its root")]
    Escapes,
    /// Two paths of one list are equal, compared in lower case.
    #[error("repeats another path")]
    Duplicate,
    /// One path of a list lies below another, compared in lower case.
    #[error("lies below another path")]
    Nested,
    /// A mount target is one of the [`RUNTIME_PLACES`] or lies below one.
    #[error("names a place the runtime keeps for itself")]
    RuntimePlace,
}

/// A relative path, or the empty path when `allow_empty`: at most [`MAX_PATH_LEN`] bytes,
/// ASCII only, with no `:`, `,` or `\` and no [`hidden`] character, not starting with
/// `/`, and every part a name (not empty, not `.`, not `..`).
///
/// ASCII keeps the comparisons of [`disjoint`] and [`distinct`] exact with a lower-case
/// fold: a Mac agent runs on case-insensitive APFS, where `A` and `a`, or `\u{e9}` composed
/// and decomposed, are one name, and ASCII needs no Unicode tables. No `:` or `,`, so the
/// path cannot add a field to a mount option string.
///
/// # Errors
///
/// [`PathFault::Empty`], [`PathFault::TooLong`], [`PathFault::BadChars`] or
/// [`PathFault::Escapes`], checked in that order.
pub fn relative(value: &str, allow_empty: bool) -> Result<(), PathFault> {
    if value.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(PathFault::Empty)
        };
    }
    if value.len() > MAX_PATH_LEN {
        return Err(PathFault::TooLong);
    }
    if value
        .chars()
        .any(|c| hidden(c) || !c.is_ascii() || matches!(c, ':' | ',' | '\\'))
    {
        return Err(PathFault::BadChars);
    }
    if value.starts_with('/') || value.split('/').any(|part| part == "..") {
        return Err(PathFault::Escapes);
    }
    if value.split('/').any(|part| part.is_empty() || part == ".") {
        return Err(PathFault::Empty);
    }
    Ok(())
}

/// An absolute path in normal form: not empty, at most [`MAX_PATH_LEN`] bytes, no
/// [`hidden`] character (NUL and every control character among them), starting with `/`,
/// with no `..` part and no empty or `.` part after it (so no `/` at its end, and not `/`
/// alone). Any other character is allowed, `:` and `,` included: such a path is opened,
/// never written into a mount option.
///
/// # Errors
///
/// [`PathFault::Empty`], [`PathFault::TooLong`], [`PathFault::BadChars`] or
/// [`PathFault::Escapes`], checked in that order, except that a `..` part
/// ([`PathFault::Escapes`]) is found before an empty or `.` one ([`PathFault::Empty`]).
pub fn absolute(value: &str) -> Result<(), PathFault> {
    if value.is_empty() {
        return Err(PathFault::Empty);
    }
    if value.len() > MAX_PATH_LEN {
        return Err(PathFault::TooLong);
    }
    if value.chars().any(hidden) {
        return Err(PathFault::BadChars);
    }
    let Some(below) = value.strip_prefix('/') else {
        return Err(PathFault::Escapes);
    };
    if below.split('/').any(|part| part == "..") {
        return Err(PathFault::Escapes);
    }
    if below.split('/').any(|part| part.is_empty() || part == ".") {
        return Err(PathFault::Empty);
    }
    Ok(())
}

/// Where a mount appears in a job's file system: at most [`MAX_PATH_LEN`] bytes, its
/// leading `/` included; `/` and then a [`relative`] path (so ASCII, no `:`, `,` or `\`,
/// normal form, and not `/` alone); and none of the [`RUNTIME_PLACES`] nor below one,
/// compared in lower case.
///
/// # Errors
///
/// [`PathFault::Empty`], [`PathFault::TooLong`], [`PathFault::Escapes`] (it does not start
/// with `/`, or has a `..` part), [`PathFault::BadChars`] or [`PathFault::RuntimePlace`].
pub fn target(value: &str) -> Result<(), PathFault> {
    if value.is_empty() {
        return Err(PathFault::Empty);
    }
    if value.len() > MAX_PATH_LEN {
        return Err(PathFault::TooLong);
    }
    let Some(below) = value.strip_prefix('/') else {
        return Err(PathFault::Escapes);
    };
    relative(below, false)?;
    let folded = value.to_ascii_lowercase();
    let kept = RUNTIME_PLACES.iter().any(|place| {
        folded
            .strip_prefix(place)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    });
    match kept {
        true => Err(PathFault::RuntimePlace),
        false => Ok(()),
    }
}

/// No two of `paths` are equal, and none lies below another, once folded to lower case.
/// An empty path is equal to another empty one and lies below none.
///
/// # Errors
///
/// [`PathFault::Duplicate`] for two equal paths, [`PathFault::Nested`] for one below
/// another; the first pair in list order decides.
pub fn disjoint(paths: &[&str]) -> Result<(), PathFault> {
    compare(paths, true)
}

/// No two of `paths` are equal once folded to lower case; one may lie below another.
///
/// # Errors
///
/// [`PathFault::Duplicate`] for two equal paths.
pub fn distinct(paths: &[&str]) -> Result<(), PathFault> {
    compare(paths, false)
}

/// Every pair of `paths`, folded to lower case: equal is [`PathFault::Duplicate`], and one
/// below the other is [`PathFault::Nested`] when `refuse_nesting`.
fn compare(paths: &[&str], refuse_nesting: bool) -> Result<(), PathFault> {
    let folded: Vec<String> = paths.iter().map(|p| p.to_ascii_lowercase()).collect();
    let below = |inner: &str, outer: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    for (index, one) in folded.iter().enumerate() {
        for two in &folded[index + 1..] {
            if one == two {
                return Err(PathFault::Duplicate);
            }
            if refuse_nesting && [below(one, two), below(two, one)].contains(&true) {
                return Err(PathFault::Nested);
            }
        }
    }
    Ok(())
}
