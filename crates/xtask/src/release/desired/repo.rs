//! The two text formats `release desired` reads: the release's `SHA256SUMS` and an apt
//! repository's `Packages` index.

use super::Error;

/// The digest `sha256sum` wrote for `name` in `text`.
///
/// A line is 64 lowercase hex digits, a space, then a space or `*` (binary mode) and the name.
///
/// # Errors
/// [`Error::Sums`] for a line that is not of that form.
pub fn sums_digest(text: &str, name: &str) -> Result<Option<String>, Error> {
    let mut found = None;
    for (at, line) in text.lines().enumerate() {
        let parsed = line
            .split_once(' ')
            .filter(|(digest, _)| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .and_then(|(digest, rest)| Some((digest, rest.strip_prefix([' ', '*'])?)));
        let Some((digest, listed)) = parsed else {
            return Err(Error::Sums {
                line: at + 1,
                text: line.to_owned(),
            });
        };
        if listed == name {
            found = Some(digest.to_owned());
        }
    }
    Ok(found)
}

/// What a `Packages` index says of the package whose `Filename:` is `filename`.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed {
    /// No stanza has that `Filename`.
    No,
    /// A stanza has it, with this `SHA256` (`None`: the stanza has no such field).
    Yes(Option<String>),
}

/// The `SHA256` of the stanza of `text` whose `Filename` is `filename`.
#[must_use]
pub fn listed(text: &str, filename: &str) -> Listed {
    let field = |stanza: &str, key: &str| {
        stanza
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
            .map(str::to_owned)
    };
    text.split("\n\n")
        .find(|stanza| field(stanza, "Filename").as_deref() == Some(filename))
        .map_or(Listed::No, |stanza| Listed::Yes(field(stanza, "SHA256")))
}
