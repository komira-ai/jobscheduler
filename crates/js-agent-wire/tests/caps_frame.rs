//! The capabilities frame and the `bundle` capability value. Each test says the defect it
//! catches.

mod common;

use common::{expect, hello, map};
use js_agent_wire::limits::{
    MAX_BUNDLE_CAPABILITY_DIGESTS, MAX_BUNDLE_CAPABILITY_VALUE_LEN, MAX_CAPABILITY_VALUE_LEN,
    MAX_CAPS_BUNDLES, MAX_CAPS_XCODES, MAX_XCODE_BUILD_LEN, capability_value_max,
};
use js_agent_wire::{Caps, ReadModel, Rule, bundle_capability, refusal};
use serde_json::json;

fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

fn caps() -> Caps {
    Caps {
        bundles: vec![digest(1), digest(2)],
        bundle_root: "/Volumes/work/bundles".into(),
        read_model: ReadModel::Allow,
        xcode: vec!["16C5032a".into(), "15F31d".into()],
    }
}

/// The exact JSON of a frame, and the defaults of one that leaves everything out: no
/// bundles, no root, the open read model, no Xcode. It catches a renamed field, a read
/// model spelled another way and a default that makes a sparse sender's frame unreadable.
#[test]
fn a_caps_frame_has_one_wire_form_and_defaults() {
    let value = serde_json::to_value(caps()).unwrap();
    assert_eq!(
        value,
        json!({
            "bundles": [digest(1), digest(2)],
            "bundle_root": "/Volumes/work/bundles",
            "read_model": "allow",
            "xcode": ["16C5032a", "15F31d"]
        })
    );
    let back: Caps = serde_json::from_value(value).unwrap();
    assert_eq!(back, caps());
    let empty: Caps = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, Caps::default());
    assert_eq!(empty.read_model, ReadModel::Open);
    assert_eq!(empty.validate(), Ok(()));
    assert_eq!(caps().validate(), Ok(()));
}

/// Each malformed frame is refused `invalid-caps` naming the field: a short, upper case or
/// other-algorithm digest, digests out of order or repeated, too many of them, a relative or
/// control-character root, an Xcode build with a bad byte, too long, repeated or too many.
/// It catches a frame a consumer would place work by on a digest that no host holds.
#[test]
fn a_malformed_caps_frame_is_refused() {
    let many: Vec<String> = (0..=MAX_CAPS_BUNDLES)
        .map(|n| format!("sha256:{n:064x}"))
        .collect();
    let builds: Vec<String> = (0..=MAX_CAPS_XCODES).map(|n| format!("b{n}")).collect();
    let cases: Vec<(&str, Caps, &str, Rule)> = vec![
        (
            "short",
            Caps {
                bundles: vec!["sha256:ab".into()],
                ..caps()
            },
            "caps.bundles",
            Rule::BadChars,
        ),
        (
            "upper",
            Caps {
                bundles: vec![digest(1).to_uppercase()],
                ..caps()
            },
            "caps.bundles",
            Rule::BadChars,
        ),
        (
            "algorithm",
            Caps {
                bundles: vec![digest(1).replace("sha256", "sha512")],
                ..caps()
            },
            "caps.bundles",
            Rule::BadChars,
        ),
        (
            "descending",
            Caps {
                bundles: vec![digest(2), digest(1)],
                ..caps()
            },
            "caps.bundles",
            Rule::Inconsistent,
        ),
        (
            "repeated",
            Caps {
                bundles: vec![digest(1), digest(1)],
                ..caps()
            },
            "caps.bundles",
            Rule::Inconsistent,
        ),
        (
            "many",
            Caps {
                bundles: many,
                ..caps()
            },
            "caps.bundles",
            Rule::TooMany,
        ),
        (
            "relative",
            Caps {
                bundle_root: "bundles".into(),
                ..caps()
            },
            "caps.bundle_root",
            Rule::Escapes,
        ),
        (
            "control",
            Caps {
                bundle_root: "/a\nb".into(),
                ..caps()
            },
            "caps.bundle_root",
            Rule::BadChars,
        ),
        (
            "bad build",
            Caps {
                xcode: vec!["a b".into()],
                ..caps()
            },
            "caps.xcode",
            Rule::BadChars,
        ),
        (
            "empty build",
            Caps {
                xcode: vec![String::new()],
                ..caps()
            },
            "caps.xcode",
            Rule::Empty,
        ),
        (
            "long build",
            Caps {
                xcode: vec!["a".repeat(MAX_XCODE_BUILD_LEN + 1)],
                ..caps()
            },
            "caps.xcode",
            Rule::TooLong,
        ),
        (
            "repeated build",
            Caps {
                xcode: vec!["a".into(), "a".into()],
                ..caps()
            },
            "caps.xcode",
            Rule::Duplicate,
        ),
        (
            "many builds",
            Caps {
                xcode: builds,
                ..caps()
            },
            "caps.xcode",
            Rule::TooMany,
        ),
    ];
    for (name, frame, field, rule) in cases {
        let err = frame.validate().unwrap_err();
        assert_eq!(err.code(), refusal::INVALID_CAPS, "{name}");
        expect(err, field, rule, refusal::INVALID_CAPS);
    }
}

/// The capability value lists the digests with commas, and a host that holds more than the
/// wire takes reports the first of them. It catches a value the server would refuse as an
/// invalid hello, and a list that drops its first entry.
#[test]
fn the_bundle_capability_lists_digests_and_stops_at_the_wire_cap() {
    assert_eq!(bundle_capability(&[]), "");
    assert_eq!(bundle_capability(&[digest(1)]), digest(1));
    assert_eq!(
        bundle_capability(&[digest(1), digest(2)]),
        format!("{},{}", digest(1), digest(2))
    );
    let all: Vec<String> = (0..40).map(digest).collect();
    let shown = bundle_capability(&all);
    assert_eq!(shown.split(',').count(), MAX_BUNDLE_CAPABILITY_DIGESTS);
    assert_eq!(shown.len(), MAX_BUNDLE_CAPABILITY_VALUE_LEN);
    assert!(shown.starts_with(&digest(0)));
}

/// A hello may carry a `bundle` value up to its own cap and every other capability up to the
/// common one: sixteen digests pass, one byte more fails, and a long `os` fails at 129. It
/// catches a cap raised for every key, and a bundle list that does not fit its own cap.
#[test]
fn only_the_bundle_capability_may_be_long() {
    assert_eq!(
        capability_value_max("bundle"),
        MAX_BUNDLE_CAPABILITY_VALUE_LEN
    );
    assert_eq!(capability_value_max("os"), MAX_CAPABILITY_VALUE_LEN);
    assert_eq!(capability_value_max("bundles"), MAX_CAPABILITY_VALUE_LEN);
    let all: Vec<String> = (0..16).map(digest).collect();
    let mut with = hello();
    with.capabilities = map(&[("bundle", &bundle_capability(&all))]);
    assert_eq!(with.validate(), Ok(()));
    let long = "a".repeat(MAX_BUNDLE_CAPABILITY_VALUE_LEN + 1);
    with.capabilities = map(&[("bundle", &long)]);
    assert_eq!(with.validate().unwrap_err().field(), "capabilities");
    let medium = "a".repeat(MAX_CAPABILITY_VALUE_LEN + 1);
    with.capabilities = map(&[("os", &medium)]);
    assert_eq!(with.validate().unwrap_err().field(), "capabilities");
}
