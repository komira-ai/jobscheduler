//! Text that comes from a person or from a layout someone else wrote.

/// Whether `c` may not appear in an annotation or be printed as it is: a control character
/// (which includes a newline and the escape that starts a terminal sequence), a line or
/// paragraph separator, an invisible character (zero-width, soft hyphen, byte-order mark, tag characters), or a character that changes the direction text is drawn in.
pub(crate) fn forbidden(c: char) -> bool {
    let bidi = matches!(
        c,
        '\u{ad}' | '\u{61c}' | '\u{200b}'..='\u{200f}' | '\u{feff}' | '\u{2060}' | '\u{e0000}'..='\u{e007f}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    );
    [c.is_control(), bidi].into_iter().any(|found| found)
}

/// `text` for one line of output: forbidden characters and backslashes are written as
/// escapes (`\u{001b}`, `\\`), so what is printed is what is stored and cannot start a
/// line or a terminal sequence of its own.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match (forbidden(c), c) {
            (true, _) => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
            (false, '\\') => out.push_str("\\\\"),
            (false, _) => out.push(c),
        }
    }
    out
}
