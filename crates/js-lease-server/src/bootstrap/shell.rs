//! Quoting and the text of one boot step. Every value that comes from a plan or a
//! secret is quoted here, so no step can add shell syntax.

use std::fmt::Write;

/// `text` as one shell word: as it is when it holds only characters that mean nothing to
/// the shell, otherwise in single quotes.
pub(super) fn word(text: &str) -> String {
    let plain = !text.is_empty()
        && text.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
                )
        });
    match plain {
        true => text.to_owned(),
        false => single_quoted(text),
    }
}

/// `text` in single quotes, always. A quote inside it closes the string, is written
/// escaped, and opens it again.
pub(super) fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// `text` as a double-quoted value of a systemd unit file: `\`, `"`, `%` and `$` are
/// escaped, so the value is one argument and nothing in it is expanded.
pub(super) fn unit_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// A SHA-256 as lowercase hex.
pub(super) fn hex(digest: &[u8; 32]) -> String {
    digest
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a word with only plain characters stays bare, that an empty word and
    /// one with a space, a quote, a glob, a `$`, a newline or a `;` is quoted, and that
    /// a single quote inside is closed, escaped and reopened.
    #[test]
    fn a_word_is_quoted_unless_it_is_plain() {
        assert_eq!(
            word("--state=/var/lib/x_y@1.2,3:4+5%6-7"),
            "--state=/var/lib/x_y@1.2,3:4+5%6-7"
        );
        assert_eq!(word(""), "''");
        for odd in ["a b", "a*b", "$HOME", "a\nb", "a;b", "it's", "`x`", "~"] {
            assert!(word(odd).starts_with('\''), "{odd}");
        }
        assert_eq!(word("it's"), r"'it'\''s'");
        assert_eq!(single_quoted("plain"), "'plain'");
    }

    /// Asserts that a unit value escapes the four characters systemd expands and keeps
    /// the rest.
    #[test]
    fn a_unit_value_escapes_what_systemd_expands() {
        assert_eq!(unit_quoted(r#"a\b"c%d$e f"#), r#""a\\b\"c%%d$$e f""#);
        assert_eq!(unit_quoted(""), r#""""#);
    }

    /// Asserts that a digest is 64 lowercase hex digits, two per byte, zero-padded.
    #[test]
    fn a_digest_is_lowercase_hex() {
        let mut digest = [0xab; 32];
        digest[0] = 0x01;
        digest[31] = 0xf0;
        let text = hex(&digest);
        assert_eq!(text.len(), 64);
        assert!(text.starts_with("01abab"));
        assert!(text.ends_with("abf0"));
    }
}
