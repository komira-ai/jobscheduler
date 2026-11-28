//! The reason a provider gave for refusing a source, as the detail of the start's
//! `failed_to_start` result carries it: `provider-refused: <reason>`, so a client that knows
//! the provider can act on what it said (`refusal::PROVIDER_REFUSED`).
//!
//! The code comes first and is followed by `: `, which is how every reader of a detail
//! already finds the code (the lease server's retry reads the part before the first `:`, and
//! a client reads the leading run of lower-case letters, digits and dashes). The provider's
//! text is untrusted, and the agent makes three changes to it before it leaves the host:
//!
//! - **One line.** Each run of white space and hidden characters (control and format
//!   characters, line and paragraph separators: `js_types::text::hidden`) becomes one space,
//!   and none is left at either end. The wire refuses a detail that holds a hidden character.
//! - **No host paths.** Each path on the host in it becomes [`PATH`]: a `/` at the start of
//!   the text or after a character that is not a letter, a digit, `.`, `_` or `-`, up to the
//!   next space, quote, bracket, `,` or `;`, less any `:` and `.` it ends with. A relative
//!   path or a key (`cas/l/<id>`) is not a path on the host and is kept; the host and port
//!   of a URL go with its path (`http:<path>`).
//! - **At most [`MAX_REASON_LEN`] bytes.** A longer reason is cut at a space, so no word, key
//!   or digest is given in part (or at a character boundary when the part kept holds no
//!   space), and ends in [`CUT`].
//!
//! A reason that is empty once so changed gives the code alone, as every refusal did before
//! the reason was carried. The finding the agent keeps on the host has the provider's text
//! as it was.

/// The most bytes of a provider's reason that a detail carries, [`CUT`] included. The detail
/// is then at most `provider-refused: ` and this, well within the wire's bound on a detail
/// (`js_agent_wire::limits::MAX_DETAIL_LEN`, 1024 bytes).
const MAX_REASON_LEN: usize = 256;

/// What a path on the host is replaced by.
const PATH: &str = "<path>";

/// What ends a reason that was cut: neither a key character nor a hex digit, so a client that
/// reads a key or a digest from the text never reads one that was cut.
const CUT: char = '\u{2026}';

/// The detail of a start that failed with `code` because a provider refused it with `reason`:
/// `<code>: <reason>`, the reason changed as the [module](self) says, or the code alone when
/// nothing of the reason is left.
pub(super) fn detail(code: &str, reason: &str) -> String {
    let reason = bound(strip_paths(&one_line(reason)));
    match reason.is_empty() {
        true => code.to_owned(),
        false => format!("{code}: {reason}"),
    }
}

/// `text` on one line: each run of white space and hidden characters is one space, with none
/// at either end.
fn one_line(text: &str) -> String {
    let mut line = String::with_capacity(text.len());
    let mut gap = false;
    for c in text.chars() {
        if c.is_whitespace() || js_types::text::hidden(c) {
            gap = !line.is_empty();
            continue;
        }
        if gap {
            line.push(' ');
            gap = false;
        }
        line.push(c);
    }
    line
}

/// Whether `c` may stand right before a `/` that is inside a name or a key rather than at
/// the start of a path.
fn names(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The characters but a space that end a path in a text.
const PATH_ENDS: &str = "\"'`()[]{}<>,;";

/// Whether `c` ends a path in a text.
fn ends_path(c: char) -> bool {
    c == ' ' || PATH_ENDS.contains(c)
}

/// `text` with each path on the host in it replaced by [`PATH`].
fn strip_paths(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut before: Option<char> = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let taken = if c == '/' && !before.is_some_and(names) {
            let run = rest.find(ends_path).unwrap_or(rest.len());
            stripped.push_str(PATH);
            rest[..run].trim_end_matches([':', '.']).len()
        } else {
            stripped.push(c);
            c.len_utf8()
        };
        before = rest[..taken].chars().next_back();
        rest = &rest[taken..];
    }
    stripped
}

/// `text` when it is at most [`MAX_REASON_LEN`] bytes; else its start, cut at a space (or at a
/// character boundary when there is none), followed by [`CUT`], in [`MAX_REASON_LEN`] bytes.
fn bound(text: String) -> String {
    if text.len() <= MAX_REASON_LEN {
        return text;
    }
    let mut end = MAX_REASON_LEN - CUT.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &text[..end];
    let kept = match (text[end..].starts_with(' '), kept.rfind(' ')) {
        (false, Some(space)) => &kept[..space],
        _ => kept,
    };
    format!("{kept}{CUT}")
}

#[cfg(test)]
#[path = "reason_tests.rs"]
mod tests;
