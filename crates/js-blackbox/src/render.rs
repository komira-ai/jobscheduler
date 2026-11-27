//! The farm's shapes (the files of `farm/`) filled in with a test's values.
//!
//! A shape is text with placeholders `{name}`; a brace is never anything else, so a comment
//! in a shape holds none. Every placeholder must have a value and every value a placeholder: a
//! shape that gains or loses one fails the render, so the shape and the test that fills it
//! cannot drift apart unnoticed.

use std::collections::{BTreeMap, BTreeSet};

/// `shape` with each `{name}` replaced by `values[name]`.
///
/// # Errors
///
/// One line naming the first placeholder with no value, a value no placeholder takes, or a
/// brace that opens no placeholder (one with no `}`, an empty or nested one, or a `}` alone).
pub fn render(shape: &str, values: &BTreeMap<&str, String>) -> Result<String, String> {
    let mut out = String::with_capacity(shape.len());
    let mut used = BTreeSet::new();
    let mut rest = shape;
    while let Some(open) = rest.find(['{', '}']) {
        let (before, from) = rest.split_at(open);
        out.push_str(before);
        let name = from
            .strip_prefix('{')
            .and_then(|r| r.split_once('}'))
            .map(|(name, _)| name)
            .filter(|name| !name.is_empty() && !name.contains('{'))
            .ok_or_else(|| {
                format!(
                    "a brace that opens no placeholder at byte {}",
                    shape.len() - from.len()
                )
            })?;
        let value = values
            .get(name)
            .ok_or_else(|| format!("the placeholder {{{name}}} has no value"))?;
        out.push_str(value);
        used.insert(name);
        rest = &from[name.len() + 2..];
    }
    out.push_str(rest);
    match values.keys().find(|k| !used.contains(*k)) {
        Some(unused) => Err(format!("the value {unused} fills no placeholder")),
        None => Ok(out),
    }
}

/// The arguments of a shape of one argument a line: every line that is not empty and does not
/// start with `#`, trimmed.
#[must_use]
pub fn arguments(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}
