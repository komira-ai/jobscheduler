//! The hostile-input table for `BundleConfig::validate` / `parse`: each row edits the
//! golden document and names the field and the defect that must be reported.

mod common;

use common::{TWO_TREE, edited, golden, hex, minimal, set};
use js_bundle::{
    BundleConfig, ConfigError, Defect, EnvNameError, MAX_CONFIG_BYTES, MAX_ENTRIES, MAX_ENV_RULES,
    MAX_JSON_ERROR_BYTES, MAX_LAYERS, MAX_TOTAL_BYTES, MAX_VALUES_PER_RULE,
};
use serde_json::{Value, json};

type Edit = Box<dyn Fn(&mut Value)>;

fn at(pointer: &'static str, new: Value) -> Edit {
    Box::new(move |v: &mut Value| set(v, pointer, new.clone()))
}

fn layers(count: usize) -> Value {
    let list: Vec<Value> = (0..count)
        .map(|i| {
            json!({
                "tree": format!("t{i}"),
                "diff_id": format!("sha256:{i:064x}"),
                "uncompressed_bytes": 1, "entries": 1,
                "license": "MIT", "source_url": "https://example.invalid/s"
            })
        })
        .collect();
    Value::Array(list)
}

fn rule(name: &str, op: &str, values: Value) -> Value {
    json!({"name": name, "op": op, "values": values, "locked": false})
}

fn invalid(result: Result<BundleConfig, ConfigError>) -> (String, Defect) {
    match result {
        Err(ConfigError::Invalid { field, defect }) => (field, defect),
        other => panic!("expected an Invalid refusal, got {other:?}"),
    }
}

fn json_error(result: Result<BundleConfig, ConfigError>) -> String {
    match result {
        Err(ConfigError::Json(text)) => text,
        other => panic!("expected a Json refusal, got {other:?}"),
    }
}

/// Every row is refused with its field and defect. Together they cover each rule of
/// format section 1.3 that `validate` owns.
#[test]
fn hostile_table_is_refused_with_field_and_defect() {
    let long = |n: usize| "a".repeat(n);
    let rows: Vec<(&str, Edit, &str, Defect)> = vec![
        (
            "schema 2",
            at("/schema", json!(2)),
            "schema",
            Defect::Unsupported { want: "1" },
        ),
        (
            "os linux",
            at("/os", json!("linux")),
            "os",
            Defect::Unsupported { want: "darwin" },
        ),
        (
            "arch x86",
            at("/architecture", json!("x86_64")),
            "architecture",
            Defect::Unsupported { want: "arm64" },
        ),
        ("empty name", at("/name", json!("")), "name", Defect::Empty),
        (
            "long name",
            at("/name", json!(long(65))),
            "name",
            Defect::TooLong { max: 64 },
        ),
        (
            "upper name",
            at("/name", json!("Tool")),
            "name",
            Defect::Malformed {
                want: "lowercase letters, digits and . _ -",
            },
        ),
        (
            "slash name",
            at("/name", json!("a/b")),
            "name",
            Defect::Malformed {
                want: "lowercase letters, digits and . _ -",
            },
        ),
        (
            "dash first name",
            at("/name", json!("-a")),
            "name",
            Defect::Malformed {
                want: "lowercase letters, digits and . _ -",
            },
        ),
        (
            "space version",
            at("/version", json!("1 2")),
            "version",
            Defect::Malformed {
                want: "letters, digits and . _ - +",
            },
        ),
        (
            "plus first version",
            at("/version", json!("+1")),
            "version",
            Defect::Malformed {
                want: "letters, digits and . _ - +",
            },
        ),
        (
            "no layers",
            at("/layers", json!([])),
            "layers",
            Defect::Empty,
        ),
        (
            "33 layers",
            at("/layers", layers(MAX_LAYERS + 1)),
            "layers",
            Defect::TooMany { max: 32 },
        ),
        (
            "duplicate tree",
            Box::new(|v: &mut Value| set(v, "/layers/1/tree", json!("job"))),
            "layers[1].tree",
            Defect::Duplicate,
        ),
        (
            "duplicate diff id",
            Box::new(|v: &mut Value| set(v, "/layers/1/diff_id", json!(hex('a')))),
            "layers[1].diff_id",
            Defect::Duplicate,
        ),
        (
            "zero bytes",
            at("/layers/0/uncompressed_bytes", json!(0)),
            "layers[0].uncompressed_bytes",
            Defect::Zero,
        ),
        (
            "zero entries",
            at("/layers/0/entries", json!(0)),
            "layers[0].entries",
            Defect::Zero,
        ),
        (
            "empty licence",
            at("/layers/0/license", json!("")),
            "layers[0].license",
            Defect::Empty,
        ),
        (
            "long licence",
            at("/layers/0/license", json!(long(257))),
            "layers[0].license",
            Defect::TooLong { max: 256 },
        ),
        (
            "licence shell",
            at("/layers/0/license", json!("MIT; rm")),
            "layers[0].license",
            Defect::Malformed {
                want: "an SPDX expression",
            },
        ),
        (
            "empty url",
            at("/layers/0/source_url", json!("")),
            "layers[0].source_url",
            Defect::Empty,
        ),
        (
            "long url",
            at(
                "/layers/0/source_url",
                json!(format!("https://{}", long(2048))),
            ),
            "layers[0].source_url",
            Defect::TooLong { max: 2048 },
        ),
        (
            "http url",
            at("/layers/0/source_url", json!("http://example.invalid/s")),
            "layers[0].source_url",
            Defect::Malformed {
                want: "an https URL without spaces or control characters",
            },
        ),
        (
            "url with space",
            at("/layers/0/source_url", json!("https://example.invalid/a b")),
            "layers[0].source_url",
            Defect::Malformed {
                want: "an https URL without spaces or control characters",
            },
        ),
        (
            "url with newline",
            at(
                "/layers/0/source_url",
                json!("https://example.invalid/a\nb"),
            ),
            "layers[0].source_url",
            Defect::Malformed {
                want: "an https URL without spaces or control characters",
            },
        ),
        (
            "env dyld",
            at("/env/0/name", json!("DYLD_INSERT_LIBRARIES")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Forbidden),
        ),
        (
            "env ld",
            at("/env/0/name", json!("LD_LIBRARY_PATH")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Forbidden),
        ),
        (
            "env sandbox",
            at("/env/0/name", json!("SANDBOX_X")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Forbidden),
        ),
        (
            "env reserved prefix",
            at("/env/0/name", json!("JS_TOKEN")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Forbidden),
        ),
        (
            "env home",
            at("/env/0/name", json!("HOME")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Forbidden),
        ),
        (
            "env developer dir",
            at("/env/0/name", json!("DEVELOPER_DIR")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Forbidden),
        ),
        (
            "env empty name",
            at("/env/0/name", json!("")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Malformed),
        ),
        (
            "env digit first",
            at("/env/0/name", json!("1X")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Malformed),
        ),
        (
            "env dash",
            at("/env/0/name", json!("A-B")),
            "env[0].name",
            Defect::EnvName(EnvNameError::Malformed),
        ),
        (
            "env duplicate name",
            at("/env/1/name", json!("PATH")),
            "env[1].name",
            Defect::Duplicate,
        ),
        (
            "set with no value",
            at("/env/1/values", json!([])),
            "env[1].values",
            Defect::Arity,
        ),
        (
            "set with two values",
            at("/env/1/values", json!(["/a", "/b"])),
            "env[1].values",
            Defect::Arity,
        ),
        (
            "prepend with no value",
            at("/env/0/values", json!([])),
            "env[0].values",
            Defect::Arity,
        ),
        (
            "prepend with 65 values",
            at("/env/0/values", json!(vec!["/a"; MAX_VALUES_PER_RULE + 1])),
            "env[0].values",
            Defect::Arity,
        ),
        (
            "undeclared tree",
            at("/env/1/values/0", json!("${tree:nope}/lib")),
            "env[1].values[0]",
            Defect::Undeclared("nope".into()),
        ),
        (
            "colon in a joined value",
            at("/env/0/values/1", json!("${tree:zig}/a:b")),
            "env[0].values[1]",
            Defect::Malformed {
                want: "free of ':' (the values are joined with it)",
            },
        ),
        (
            "entry name empty",
            Box::new(|v: &mut Value| {
                set(v, "/entrypoints", json!({"": {"path": "${tree:zig}/zig"}}))
            }),
            "entrypoints.",
            Defect::Empty,
        ),
        (
            "entry name slash",
            Box::new(|v: &mut Value| {
                set(
                    v,
                    "/entrypoints",
                    json!({"a/b": {"path": "${tree:zig}/zig"}}),
                )
            }),
            "entrypoints.a/b",
            Defect::Malformed {
                want: "a name of letters, digits and . _ - +",
            },
        ),
        (
            "entry name dot first",
            Box::new(|v: &mut Value| {
                set(
                    v,
                    "/entrypoints",
                    json!({".x": {"path": "${tree:zig}/zig"}}),
                )
            }),
            "entrypoints..x",
            Defect::Malformed {
                want: "a name of letters, digits and . _ - +",
            },
        ),
        (
            "entry absolute path",
            at("/entrypoints/zig/path", json!("/usr/bin/zig")),
            "entrypoints.zig.path",
            Defect::Malformed {
                want: "${tree:NAME}/path below a tree",
            },
        ),
        (
            "entry the tree itself",
            at("/entrypoints/zig/path", json!("${tree:zig}")),
            "entrypoints.zig.path",
            Defect::Malformed {
                want: "${tree:NAME}/path below a tree",
            },
        ),
        (
            "entry developer dir",
            at("/entrypoints/zig/path", json!("${developer_dir}/x")),
            "entrypoints.zig.path",
            Defect::Malformed {
                want: "${tree:NAME}/path below a tree",
            },
        ),
        (
            "entry undeclared tree",
            at("/entrypoints/zig/path", json!("${tree:nope}/x")),
            "entrypoints.zig.path",
            Defect::Undeclared("nope".into()),
        ),
        (
            "writable undeclared",
            at("/writable_trees", json!(["nope"])),
            "writable_trees[0]",
            Defect::Undeclared("nope".into()),
        ),
        (
            "writable twice",
            at("/writable_trees", json!(["zig", "zig"])),
            "writable_trees",
            Defect::Duplicate,
        ),
        (
            "reads twice",
            at("/seatbelt/reads", json!(["timezone", "timezone"])),
            "seatbelt.reads",
            Defect::Duplicate,
        ),
        (
            "components twice",
            at("/xcode/components", json!(["clang", "clang"])),
            "xcode.components",
            Defect::Duplicate,
        ),
        (
            "empty tool",
            at("/built/tool", json!("")),
            "built.tool",
            Defect::Empty,
        ),
        (
            "long tool",
            at("/built/tool", json!(long(129))),
            "built.tool",
            Defect::TooLong { max: 128 },
        ),
        (
            "non-ascii tool",
            at("/built/tool", json!("t\u{e9}")),
            "built.tool",
            Defect::Malformed {
                want: "printable ASCII",
            },
        ),
    ];
    for (label, edit, field, defect) in rows {
        let (got_field, got_defect) = invalid(edited(edit));
        assert_eq!(
            (got_field.as_str(), &got_defect),
            (field, &defect),
            "{label}"
        );
    }
}

/// More than 64 env rules, entrypoints are refused by count.
#[test]
fn counts_are_bounded() {
    let many: Vec<Value> = (0..=MAX_ENV_RULES)
        .map(|i| rule(&format!("N{i}"), "set", json!(["x"])))
        .collect();
    let got = invalid(edited(|v| set(v, "/env", Value::Array(many))));
    assert_eq!(got, ("env".to_string(), Defect::TooMany { max: 64 }));

    let entries: serde_json::Map<String, Value> = (0..65)
        .map(|i| (format!("e{i}"), json!({"path": "${tree:zig}/zig"})))
        .collect();
    let got = invalid(edited(|v| set(v, "/entrypoints", Value::Object(entries))));
    assert_eq!(
        got,
        ("entrypoints".to_string(), Defect::TooMany { max: 64 })
    );
}

/// An entrypoint name over 64 bytes is refused.
#[test]
fn long_entrypoint_name_is_refused() {
    let name = "a".repeat(65);
    let got = invalid(edited(|v| {
        set(
            v,
            "/entrypoints",
            json!({ (name.clone()): {"path": "${tree:zig}/zig"} }),
        );
    }));
    assert_eq!(
        got,
        (format!("entrypoints.{name}"), Defect::TooLong { max: 64 })
    );
}

/// A layer count at the limit, and rules at the limit, are accepted.
#[test]
fn counts_at_the_limit_are_accepted() {
    let config = edited(|v| {
        set(v, "/layers", layers(MAX_LAYERS));
        set(
            v,
            "/env",
            json!([rule("P", "prepend", json!(vec!["/a"; MAX_VALUES_PER_RULE]))]),
        );
        set(v, "/entrypoints", json!({}));
    })
    .unwrap();
    assert_eq!(config.layers.len(), MAX_LAYERS);
}

/// Values that fail the placeholder rules fail while reading, with the reason in the
/// message: nothing but `${tree:NAME}` and `${developer_dir}` expands, and a result
/// outside its tree, empty-component or NUL/newline value never reads.
#[test]
fn placeholder_violations_are_refused_at_read() {
    let rows = [
        ("${HOME}/x", "unknown placeholder ${HOME}"),
        ("${tree:a/../b}", "placeholder names no valid tree"),
        ("${tree:zig}/../x", "'..'"),
        ("${tree:zig}//x", "empty component"),
        ("${tree:zig}x", "attached"),
        ("${tree:zig", "not closed"),
        ("a\nb", "newline"),
        ("a\u{0}b", "NUL"),
    ];
    for (text, reason) in rows {
        let message = json_error(edited(|v| set(v, "/env/1/values/0", json!(text))));
        assert!(message.contains(reason), "{text:?}: {message}");
    }
}

/// Unknown fields, bad enum values, bad names and versions are refused while reading.
#[test]
fn schema_violations_are_refused_at_read() {
    let rows: Vec<(&str, Edit, &str)> = vec![
        (
            "unknown top field",
            Box::new(|v: &mut Value| set(v, "/extra", json!(1))),
            "unknown field",
        ),
        (
            "unknown nested field",
            Box::new(|v: &mut Value| set(v, "/layers/0/extra", json!(1))),
            "unknown field",
        ),
        (
            "unknown read",
            at("/seatbelt/reads/0", json!("everything")),
            "unknown variant",
        ),
        (
            "unknown network level",
            at("/seatbelt/network_max", json!("all")),
            "unknown variant",
        ),
        (
            "unknown component",
            at("/xcode/components/0", json!("ld")),
            "unknown variant",
        ),
        (
            "unknown op",
            at("/env/0/op", json!("remove")),
            "unknown variant",
        ),
        (
            "bad tree name",
            at("/layers/0/tree", json!("Job")),
            "invalid tree name",
        ),
        (
            "long tree name",
            at("/layers/0/tree", json!("a".repeat(33))),
            "invalid tree name",
        ),
        (
            "bad diff id",
            at("/layers/0/diff_id", json!("sha256:ABC")),
            "invalid diff id",
        ),
        (
            "bad os version",
            at("/os.version", json!("14")),
            "invalid version",
        ),
        (
            "bad xcode version",
            at("/xcode/min", json!("27.01")),
            "invalid version",
        ),
        (
            "negative bytes",
            at("/layers/0/entries", json!(-1)),
            "invalid value",
        ),
    ];
    for (label, edit, reason) in rows {
        let message = json_error(edited(edit));
        assert!(message.contains(reason), "{label}: {message}");
    }
    assert!(json_error(BundleConfig::parse(b"{")).contains("EOF"));
    assert!(json_error(BundleConfig::parse(&[0xff, 0xfe])).contains("expected"));
}

/// A key repeated in a struct is refused rather than last-wins.
#[test]
fn repeated_key_is_refused() {
    let text = TWO_TREE.replacen(
        "\"os\":\"darwin\"",
        "\"os\":\"darwin\",\"os\":\"darwin\"",
        1,
    );
    assert!(json_error(BundleConfig::parse(text.as_bytes())).contains("duplicate field"));
}

/// The config document is bounded before it is read: one byte over is refused unread,
/// exactly at the limit reads.
#[test]
fn over_long_config_is_refused_and_the_limit_reads() {
    let mut bytes = TWO_TREE.as_bytes().to_vec();
    bytes.resize(MAX_CONFIG_BYTES, b' ');
    assert!(BundleConfig::parse(&bytes).is_ok());
    bytes.push(b' ');
    assert_eq!(
        BundleConfig::parse(&bytes),
        Err(ConfigError::TooLarge {
            len: MAX_CONFIG_BYTES + 1,
            max: MAX_CONFIG_BYTES
        })
    );
}

/// `validate` also guards a config built in memory.
#[test]
fn validate_checks_a_config_built_in_memory() {
    let mut config = golden();
    assert_eq!(config.validate(), Ok(()));
    config.os = "ios".into();
    assert!(config.validate().is_err());
    assert_eq!(minimal().validate(), Ok(()));
}

/// Errors read as `field: reason`.
#[test]
fn errors_name_the_field() {
    let err = edited(|v| set(v, "/layers/0/entries", json!(0))).unwrap_err();
    assert_eq!(err.to_string(), "layers[0].entries: must be at least 1");
}

/// An entrypoint name that appears twice is refused rather than last-wins: the digest is
/// over the raw bytes, so a reader that takes the first would see another program.
#[test]
fn repeated_entrypoint_name_is_refused() {
    let text = TWO_TREE.replacen(
        "\"zig\":{\"path\":\"${tree:zig}/zig\"}",
        "\"zig\":{\"path\":\"${tree:zig}/zig\"},\"zig\":{\"path\":\"${tree:job}/bin/other\"}",
        1,
    );
    assert_ne!(text, TWO_TREE);
    let message = json_error(BundleConfig::parse(text.as_bytes()));
    assert!(message.contains("duplicate entrypoint"), "{message}");
    assert!(message.contains("zig"), "{message}");
}

/// Layer sizes and entry counts have ceilings: one layer, and all layers together, may
/// not declare more than 16 GiB, and a layer has at most 2^24 entries. The ceilings
/// themselves are accepted.
#[test]
fn layer_sizes_and_entry_counts_are_capped() {
    let layer = |field: &'static str, value: u64| {
        invalid(edited(|v| {
            set(v, &format!("/layers/0/{field}"), json!(value))
        }))
    };
    assert_eq!(
        layer("uncompressed_bytes", MAX_TOTAL_BYTES + 1),
        (
            "layers[0].uncompressed_bytes".to_string(),
            Defect::OverLimit {
                max: MAX_TOTAL_BYTES
            }
        )
    );
    assert_eq!(
        layer("uncompressed_bytes", u64::MAX),
        (
            "layers[0].uncompressed_bytes".to_string(),
            Defect::OverLimit {
                max: MAX_TOTAL_BYTES
            }
        )
    );
    assert_eq!(
        layer("entries", MAX_ENTRIES + 1),
        (
            "layers[0].entries".to_string(),
            Defect::OverLimit { max: MAX_ENTRIES }
        )
    );
    let sized = |first: u64, second: u64, entries: u64| {
        edited(|v| {
            set(v, "/layers/0/uncompressed_bytes", json!(first));
            set(v, "/layers/1/uncompressed_bytes", json!(second));
            set(v, "/layers/0/entries", json!(entries));
        })
    };
    assert!(sized(MAX_TOTAL_BYTES - 1, 1, MAX_ENTRIES).is_ok());
    assert!(sized(MAX_TOTAL_BYTES, 0, 1).is_err());
    assert_eq!(
        invalid(sized(MAX_TOTAL_BYTES - 1, 2, 1)),
        (
            "layers".to_string(),
            Defect::OverLimit {
                max: MAX_TOTAL_BYTES
            }
        )
    );
    assert_eq!(
        invalid(sized(MAX_TOTAL_BYTES, MAX_TOTAL_BYTES, 1)),
        (
            "layers".to_string(),
            Defect::OverLimit {
                max: MAX_TOTAL_BYTES
            }
        )
    );
}

/// What the parser echoes of the document is escaped and bounded: a key with a newline
/// and an escape character gives one line without control characters, and a 900 kB key
/// gives a message of at most `MAX_JSON_ERROR_BYTES`, also for multi-byte text.
#[test]
fn json_errors_escape_control_characters_and_stay_bounded() {
    let message = json_error(edited(|v| set(v, "/x\ny\u{1b}[31m\u{202e}z", json!(1))));
    assert!(message.contains("unknown field"), "{message}");
    assert!(!message.chars().any(char::is_control), "{message:?}");
    assert!(!message.contains('\u{202e}'), "{message:?}");
    assert!(message.contains("x\\ny\\u{1b}"), "{message}");

    for unit in ["k", "\u{e9}", "\u{1f600}"] {
        let key = unit.repeat(900_000 / unit.len());
        let message = json_error(edited(|v| set(v, &format!("/{key}"), json!(1))));
        assert!(message.len() <= MAX_JSON_ERROR_BYTES, "{}", message.len());
        assert!(message.starts_with("unknown field"), "{unit:?}");
        assert!(message.ends_with("..."), "{unit:?}");
    }

    let short = json_error(BundleConfig::parse(b"{"));
    assert!(short.len() < MAX_JSON_ERROR_BYTES);
    assert!(!short.ends_with("..."));
}

/// The entrypoints must be a map: another JSON type is refused saying so.
#[test]
fn entrypoints_must_be_a_map() {
    let message = json_error(edited(|v| set(v, "/entrypoints", json!([]))));
    assert!(message.contains("a map of entrypoints"), "{message}");
}
