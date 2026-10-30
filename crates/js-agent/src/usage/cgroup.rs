//! Readers for the text of cgroup version 2 files, and the rule for which directories a
//! lease's usage may be read from. Each reader returns `None` for text it does not fully
//! understand; none guesses a number.

use std::path::Path;

/// How a lease's control group sits below the actions control group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A container engine's own placement: any directory strictly below the actions group
    /// (Podman's `libpod-<id>.scope` and its `container` child).
    Engine,
    /// The direct runner's placement: `<actions>/<lease>/ctr`, the container's group as a
    /// child of its lease's group.
    Crun,
}

/// The layout of `dir` below `actions`, or `None` when `dir` is not a lease's group.
pub fn layout_of(dir: &Path, actions: &Path) -> Option<Layout> {
    let _ = (dir, actions);
    None
}

/// The value of the line `key value` in a flat-keyed file such as `cpu.stat`.
///
/// The key must be the whole first token (a space separates tokens, so a leading space
/// matches nothing), the line must hold exactly two tokens, and the value must be ASCII
/// digits that fit a `u64`. The first line with the key decides: a later duplicate is not
/// consulted, even when the first is malformed.
pub fn parse_stat_key(text: &str, key: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.split(' ').next() == Some(key))?;
    let mut tokens = line.split(' ').skip(1);
    let value = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    parse_digits(value)
}

/// `usage_usec` of a `cpu.stat`: the CPU time used, in microseconds.
pub fn parse_usage_usec(text: &str) -> Option<u64> {
    parse_stat_key(text, "usage_usec")
}

/// `inactive_file` of a `memory.stat`: page cache that is cheap to reclaim.
pub fn parse_inactive_file(text: &str) -> Option<u64> {
    parse_stat_key(text, "inactive_file")
}

/// `active_file` of a `memory.stat`: page cache recently used.
pub fn parse_active_file(text: &str) -> Option<u64> {
    parse_stat_key(text, "active_file")
}

/// A file that holds one number, such as `memory.current`: surrounding white space is
/// trimmed and nothing but digits may remain.
pub fn parse_single_u64(text: &str) -> Option<u64> {
    parse_digits(text.trim())
}

/// The working set: `memory.current` less the inactive file cache, never below 0.
pub fn working_set(current: u64, inactive_file: u64) -> u64 {
    current.saturating_sub(inactive_file)
}

/// Whether a `cpu.max` sets a limit: `Some(false)` when its first token is `max`,
/// `Some(true)` when it is a number, `None` for anything else.
pub fn cpu_limited(cpu_max: &str) -> Option<bool> {
    let first = cpu_max.split_whitespace().next()?;
    if first == "max" {
        return Some(false);
    }
    parse_digits(first).map(|_| true)
}

/// A `u64` written with ASCII digits only (no sign, no space, no empty string).
fn parse_digits(text: &str) -> Option<u64> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
