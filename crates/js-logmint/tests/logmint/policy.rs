//! The session policy, parsed back.

use js_logmint::{Bucket, LogPrefix, POLICY_VERSION, policy};
use js_logstore::fake::{Action, Policy};
use serde_json::{Value, json};

/// The policy for `bucket` and `prefix`, as text and parsed.
fn built(bucket: &str, prefix: &str) -> (String, Value) {
    let text = policy(
        &Bucket::new(bucket).unwrap(),
        &LogPrefix::new(prefix).unwrap(),
    );
    let parsed = serde_json::from_str(&text).unwrap();
    (text, parsed)
}

/// The one statement of `document`, which must have exactly `Version` and `Statement`.
fn statement(document: &Value) -> &serde_json::Map<String, Value> {
    let mut keys: Vec<&String> = document.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["Statement", "Version"]);
    assert_eq!(document["Version"], POLICY_VERSION);
    let statements = document["Statement"].as_array().unwrap();
    assert_eq!(statements.len(), 1);
    statements[0].as_object().unwrap()
}

/// Asserts the session policy parsed back: exactly `Version` and one statement, the statement
/// exactly `Effect: Allow`, the two actions `s3:PutObject` and `s3:DeleteObject` and the
/// resource `arn:aws:s3:::logs/L*` (`arn:aws:s3:::logs/*` for the empty prefix), whose trailing
/// `*` is its only `*`, `?` or `$`; that the pattern of `a/b` does not cover `a/bc/...`; that the
/// fake store reads the same text as put and delete under exactly `a/b/`; that for a
/// 63-character bucket and a 512-byte prefix the text is under 2,048 characters; and that the
/// nearest legal characters (`.`, `_`, `-`, digits, both cases) come back exactly. It catches a
/// resource wider than the log prefix, another action, a condition or a second statement, and a
/// policy STS would refuse for its length.
#[test]
fn the_session_policy_allows_only_put_and_delete_under_the_log_prefix() {
    for (prefix, resource) in [
        ("a/b", "arn:aws:s3:::logs/a/b/*"),
        ("", "arn:aws:s3:::logs/*"),
        ("js", "arn:aws:s3:::logs/js/*"),
        ("A.z_0-9/y.Z-1_", "arn:aws:s3:::logs/A.z_0-9/y.Z-1_/*"),
    ] {
        let (_, document) = built("logs", prefix);
        let statement = statement(&document);
        let mut keys: Vec<&String> = statement.keys().collect();
        keys.sort();
        assert_eq!(keys, ["Action", "Effect", "Resource"]);
        assert_eq!(statement["Effect"], "Allow");
        assert_eq!(
            statement["Action"],
            json!(["s3:PutObject", "s3:DeleteObject"])
        );
        assert_eq!(statement["Resource"], resource);
        let wildcards: Vec<char> = resource
            .chars()
            .filter(|c| matches!(c, '*' | '?' | '$'))
            .collect();
        assert_eq!(wildcards, ['*']);
        assert!(resource.ends_with('*'));
    }

    let (text, document) = built("logs", "a/b");
    let pattern = statement(&document)["Resource"]
        .as_str()
        .unwrap()
        .strip_suffix('*')
        .unwrap();
    assert!("arn:aws:s3:::logs/a/b/job-1/stdout.log".starts_with(pattern));
    assert!(!"arn:aws:s3:::logs/a/bc/job-1/stdout.log".starts_with(pattern));
    let read = Policy::from_session_policy(&text, "logs").unwrap();
    assert_eq!(read.actions, [Action::Put, Action::Delete]);
    assert_eq!(read.prefix, "a/b/");

    let bucket = "b".repeat(63);
    let prefix = format!("{}/{}", "a".repeat(255), "b".repeat(256));
    assert_eq!(prefix.len(), 512);
    let (text, document) = built(&bucket, &prefix);
    assert!(text.chars().count() < 2048, "{}", text.chars().count());
    assert_eq!(
        statement(&document)["Resource"],
        format!("arn:aws:s3:::{bucket}/{prefix}/*")
    );
}
