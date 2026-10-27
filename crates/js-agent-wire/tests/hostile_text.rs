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
