//! Free text that draws nothing or reorders what is around it is refused.

mod common;

use common::{broken, expect, hello, result};
use js_agent_wire::refusal;
use js_agent_wire::{Hello, Rule};

fn check(edit: impl FnOnce(&mut Hello)) -> js_agent_wire::ValidationError {
    broken(&hello(), edit, Hello::validate)
}

/// One character from each Unicode format range and the line and paragraph separators, in
/// the agent version, a capability value, and a GPU model, a result detail: all refused;
/// ordinary non-ASCII text passes. It catches a right-to-left override or a zero-width
/// character that makes a log line read as another message, or a separator that splits it.
#[test]
fn format_and_separator_characters_are_refused() {
    let hostile = [
        '\u{00AD}',
        '\u{0600}',
        '\u{061C}',
        '\u{06DD}',
        '\u{070F}',
        '\u{180E}',
        '\u{200B}',
        '\u{200F}',
        '\u{2028}',
        '\u{2029}',
        '\u{202E}',
        '\u{2060}',
        '\u{2066}',
        '\u{2069}',
        '\u{206F}',
        '\u{FEFF}',
        '\u{FFF9}',
        '\u{FFFB}',
        '\u{E0001}',
        '\u{E0020}',
        '\u{E007F}',
    ];
    for c in hostile {
        let text = format!("a{c}b");
        let err = check(|h| h.agent_version = text.clone());
        expect(err, "agent_version", Rule::BadChars, refusal::INVALID_HELLO);
        let err = check(|h| {
            h.capabilities.insert("os".into(), text.clone());
        });
        expect(err, "capabilities", Rule::BadChars, refusal::INVALID_HELLO);
        let err = check(|h| h.gpu_devices[0].model = text.clone());
        expect(
            err,
            "gpu_devices.model",
            Rule::BadChars,
            refusal::INVALID_HELLO,
        );
        let mut bad = result();
        bad.detail = Some(text.clone());
        let err = bad.validate().unwrap_err();
        expect(
            err,
            "results.detail",
            Rule::BadChars,
            refusal::INVALID_HEARTBEAT,
        );
    }
    let mut ok = hello();
    ok.agent_version = "v1 \u{e9}\u{4e2d}\u{1f600} \u{2010} \u{200A}".into();
    assert_eq!(ok.validate(), Ok(()));
}

/// Both ends of every range of the Unicode 16.0 format-character listing (general
/// category Cf, plus U+2028 and U+2029) are refused, and the code point just below and
/// just above each range is not. It catches a range that is missing (U+0890, U+0891,
/// U+08E2, U+110BD, U+110CD, U+13430 to U+1343F, U+1BCA0 to U+1BCA3 and U+1D173 to
/// U+1D17A were once absent), one cut short, and one that spills over a neighbour.
#[test]
fn every_format_range_is_refused_edge_to_edge() {
    let ranges = [
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
    let with = |code: u32| {
        let text = format!("a{}b", char::from_u32(code).unwrap());
        let mut copy = hello();
        copy.agent_version = text;
        copy.validate()
    };
    for (low, high) in ranges {
        for code in [low, (low + high) / 2, high] {
            let err = with(code).unwrap_err();
            expect(err, "agent_version", Rule::BadChars, refusal::INVALID_HELLO);
        }
        for outside in [low - 1, high + 1] {
            let inside = ranges.iter().any(|&(a, b)| (a..=b).contains(&outside));
            assert_eq!(with(outside).is_ok(), !inside, "U+{outside:04X}");
        }
    }
}

/// Characters that draw nothing but are not format characters (the Hangul and halfwidth
/// fillers, the braille blank, the combining grapheme joiner, the Khmer inherent vowels,
/// variation selectors and the wide and no-break spaces) pass in free text: the documented
/// limit of the rule, which is the Unicode format category and not every default-ignorable
/// character, because variation selectors and spaces occur in real text. These fields
/// reach a log or a screen, never an id, an image or a path. A change to the rule shows
/// here.
#[test]
fn invisible_characters_outside_the_format_category_are_a_documented_limit() {
    let mut ok = hello();
    for code in [
        0x115F, 0x1160, 0x3164, 0xFFA0, 0x2800, 0x034F, 0x17B4, 0x17B5, 0x180B, 0x180C, 0x180D,
        0xFE00, 0xFE0F, 0xE0100, 0xE01EF, 0x2000, 0x200A, 0x3000, 0x00A0,
    ] {
        ok.agent_version = format!("a{}b", char::from_u32(code).unwrap());
        assert_eq!(ok.validate(), Ok(()), "U+{code:04X}");
    }
}
