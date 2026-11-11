//! The characters no text a person or a log may read is allowed to hold: one predicate,
//! [`hidden`], for every crate that checks such text, so the server and the agent refuse
//! the same characters.

/// A character that must not appear in text a person or a log may read: a control
/// character, a format character or a line or paragraph separator.
#[must_use]
pub fn hidden(c: char) -> bool {
    c.is_control() || is_format(c)
}

/// The Unicode format characters (general category Cf) of Unicode 16.0, as listed by
/// UCD 16.0.0 (the data `regex-syntax` 0.8.11 and `unicode-general-category` 1.1.0 are
/// generated from), and U+2028 and U+2029 (categories Zl and Zp). None draws a glyph,
/// but each can reorder or hide the text around it. The tests name a code point at both
/// ends of every range and one just outside each.
const FORMAT_RANGES: [(u32, u32); 21] = [
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

fn is_format(c: char) -> bool {
    let code = u32::from(c);
    FORMAT_RANGES
        .iter()
        .any(|&(low, high)| (low..=high).contains(&code))
}
