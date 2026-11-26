//! The retry policy and callback flags of `js job submit`, against the in-memory job server
//! of `fake_jobs`: what the request's spec holds, the signing key byte for byte from its file,
//! and the refusal of a key file that is not base64.

use js_proto::v1::{Callback, RetryPolicy, SubmitJobRequest};

use super::{call, js, only};
use crate::fake_jobs::Fake;
use crate::support::Scratch;

/// The key the file holds: every byte from 0 to 31, then 0xfb, 0xef and 0xff, whose base64
/// holds a `+` and a `/`, so a decoder of another alphabet reads other bytes.
fn key() -> Vec<u8> {
    let mut key: Vec<u8> = (0..32).collect();
    key.extend([0xfb, 0xef, 0xff]);
    key
}

/// Asserts that `--spec.callback.signing-key-file` reads the file's proto3 JSON string as the
/// standard base64 of the key and sends exactly those 35 bytes, beside the URL, the deadline
/// and the three retry flags (the waits in seconds from durations), in one `SubmitJob`.
/// Catches a key dropped, sent as the file's text, decoded with the URL-safe alphabet or cut,
/// and a callback or policy flag put in another field.
#[test]
fn the_signing_key_file_sends_the_keys_bytes() {
    let scratch = Scratch::new("jobs_callback_key");
    let fake = Fake::new();
    let file = scratch.write(
        "key.json",
        r#""AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh/77/8=""#,
        0o600,
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &[
            "job",
            "submit",
            "--id",
            "called-1",
            "--spec.callback.url",
            "https://hooks.example/js-done/ref-1",
            "--spec.callback.signing-key-file",
            file.to_str().unwrap(),
            "--spec.callback.deadline-seconds",
            "2h",
            "--spec.retry.max-attempts",
            "3",
            "--spec.retry.initial-backoff-seconds",
            "30s",
            "--spec.retry.max-backoff-seconds",
            "10m",
        ],
    );
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    let (path, request) = only::<SubmitJobRequest>(&fake);
    assert_eq!(path, call("SubmitJob"));
    let spec = request.spec.expect("a spec");
    assert_eq!(spec.job_id, "called-1");
    assert_eq!(
        spec.callback,
        Some(Callback {
            url: "https://hooks.example/js-done/ref-1".to_owned(),
            signing_key: key(),
            deadline_seconds: Some(7200),
        })
    );
    assert_eq!(
        spec.retry,
        Some(RetryPolicy {
            max_attempts: 3,
            initial_backoff_seconds: Some(30),
            max_backoff_seconds: Some(600),
        })
    );
}

/// Asserts that a key file holding text that is not base64, or a list in place of a string,
/// is refused naming the flag (exit 2, the message starting with the flag's name) and that
/// nothing is sent. Catches a file that is read as something other than the key, or a key
/// sent empty when its file cannot be read as one.
#[test]
fn a_key_file_that_is_not_base64_is_refused_and_sends_nothing() {
    let scratch = Scratch::new("jobs_callback_bad_key");
    for (name, contents) in [("text.json", r#""not base64!""#), ("list.json", "[]")] {
        let fake = Fake::new();
        let file = scratch.write(name, contents, 0o600);
        let got = js(
            &scratch,
            fake.dialer(),
            &[
                "job",
                "submit",
                "--spec.callback.signing-key-file",
                file.to_str().unwrap(),
            ],
        );
        assert_eq!(got.code, 2, "{name}: {}", got.err);
        assert!(
            got.err
                .starts_with("js: --spec.callback.signing-key-file: "),
            "{name}: {}",
            got.err
        );
        assert!(fake.seen().is_empty(), "{name}");
    }
}
