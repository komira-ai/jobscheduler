//! `js-bundle push` against a registry in this process, over plain http on `127.0.0.1` with a
//! port the system picked (the `--plain-http` entry; the publish's https rules and its
//! credentials are js-oci's tests): what it prints, what the registry then holds, that the
//! same bundle again changes nothing, that a held tag is refused, and every flag it refuses
//! before anything is sent.

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{Run, blob_path, manifest_digest, packed, run, strings, text, work};
use js_testkit::oci_registry::Fixture;

/// `js-bundle push <layout> --to 127.0.0.1:<port>/team/bundle:<tag> --plain-http` and `extra`.
fn push_args(layout: &Path, fx: &Fixture, tag: &str, extra: &[&str]) -> Vec<String> {
    let mut args = strings(&[
        "push",
        &text(layout),
        "--to",
        &format!("127.0.0.1:{}/team/bundle:{tag}", fx.port),
        "--plain-http",
    ]);
    args.extend(strings(extra));
    args
}

/// Runs the program off the runtime's threads, so the registry keeps answering.
async fn run_off(args: Vec<String>) -> Run {
    tokio::task::spawn_blocking(move || run(&args))
        .await
        .expect("the run")
}

/// Proves `push` puts the bundle under its tag and says so with the reference by digest:
/// the registry then serves the layout's manifest bytes under the tag and its digest, and
/// every blob the manifest names; a second push of the same bundle says `unchanged` and
/// sends no upload. Catches a push that changes the bytes, prints another digest, or
/// uploads again.
#[tokio::test(flavor = "multi_thread")]
async fn push_puts_the_bundle_under_its_tag_and_a_second_push_changes_nothing() {
    let work = work("js-bundle-push");
    let (layout, digest) = packed(&work, "layout", "one");
    let fx = Fixture::start_plain().await;
    let first = run_off(push_args(&layout, &fx, "1", &[])).await;
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let reference = format!("127.0.0.1:{}/team/bundle:1@{digest}", fx.port);
    assert_eq!(first.stdout, format!("pushed {reference}\n"));
    assert_eq!(first.stderr, "");
    let manifest = std::fs::read(blob_path(&layout, &digest)).expect("manifest");
    for reference in ["1", digest.as_str()] {
        assert_eq!(
            fx.manifest(reference).map(|(_, b)| b).as_ref(),
            Some(&manifest)
        );
    }
    let names: serde_json::Value = serde_json::from_slice(&manifest).expect("json");
    let config = names["config"]["digest"].as_str().expect("config");
    let layer = names["layers"][0]["digest"].as_str().expect("layer");
    for blob in [config, layer] {
        let held = fx.blob(&js_oci::Digest::parse(blob).expect("digest"));
        assert_eq!(
            held,
            Some(std::fs::read(blob_path(&layout, blob)).expect("blob"))
        );
    }

    let before = fx.requests().len();
    let again = run_off(push_args(&layout, &fx, "1", &[])).await;
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    assert_eq!(again.stdout, format!("unchanged {reference}\n"));
    let sent = fx.requests().split_off(before);
    assert!(
        sent.iter()
            .all(|r| !r.contains(" PUT ") && !r.contains(" POST ")),
        "{sent:?}"
    );
}

/// Proves a different bundle under a tag that names one already fails with both digests
/// named and leaves the tag as it was. Catches a push that moves a tag.
#[tokio::test(flavor = "multi_thread")]
async fn push_of_another_bundle_under_a_held_tag_is_refused() {
    let work = work("js-bundle-push-held");
    let (first, first_digest) = packed(&work, "first", "one");
    let (second, second_digest) = packed(&work, "second", "two");
    let fx = Fixture::start_plain().await;
    let pushed = run_off(push_args(&first, &fx, "1", &[])).await;
    assert_eq!(pushed.code, Some(0), "{}", pushed.stderr);
    let refused = run_off(push_args(&second, &fx, "1", &[])).await;
    assert_eq!(refused.code, Some(1));
    assert_eq!(refused.stdout, "");
    assert_eq!(
        refused.stderr,
        format!(
            "js-bundle: the tag 1 names {first_digest}, not {second_digest}: a tag is never moved to another manifest; publish under a new tag\n"
        )
    );
    let held = std::fs::read(blob_path(&first, &first_digest)).expect("manifest");
    assert_eq!(fx.manifest("1").map(|(_, b)| b), Some(held));
    assert_eq!(manifest_digest(&second), second_digest);
}

/// Proves the password is read from the file `--password-file` names, standard input
/// included, with its line end dropped: a push with credentials from a file and from
/// `/dev/stdin` succeeds. Catches a password file that cannot be standard input, and a
/// trailing newline kept in the password (which `Credentials` refuses as a control
/// character).
#[tokio::test(flavor = "multi_thread")]
async fn the_password_comes_from_a_file_or_standard_input_without_its_line_end() {
    let work = work("js-bundle-push-password");
    let (layout, _) = packed(&work, "layout", "one");
    let file = work.join("password");
    std::fs::write(&file, "s3cret\r\n").expect("password");
    let fx = Fixture::start_plain().await;
    let with_file = &["--user", "pusher", "--password-file", &text(&file)];
    let from_file = run_off(push_args(&layout, &fx, "1", with_file)).await;
    assert_eq!(from_file.code, Some(0), "{}", from_file.stderr);

    let args = push_args(
        &layout,
        &fx,
        "2",
        &["--user", "pusher", "--password-file", "/dev/stdin"],
    );
    let from_stdin = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_js-bundle"))
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start js-bundle");
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(b"s3cret\n").expect("write the password");
        drop(stdin);
        child.wait_with_output().expect("js-bundle")
    })
    .await
    .expect("the run");
    let stderr = String::from_utf8_lossy(&from_stdin.stderr);
    assert_eq!(from_stdin.status.code(), Some(0), "{stderr}");
    assert!(String::from_utf8_lossy(&from_stdin.stdout).starts_with("pushed "));
}

/// Proves every flag `push` refuses is refused before a request goes anywhere, with the
/// reason on standard error: a `--to` with no tag, a tag the distribution spec does not
/// allow, a name with no registry, a password file that is missing, not text, or holds a
/// control character, and `--user` without `--password-file`. Catches a bad flag that is
/// sent to the registry, or a password taken in pieces.
#[tokio::test(flavor = "multi_thread")]
async fn push_refuses_bad_flags_before_it_sends_anything() {
    let work = work("js-bundle-push-flags");
    let (layout, _) = packed(&work, "layout", "one");
    let fx = Fixture::start_plain().await;
    let port = fx.port;
    let not_text = work.join("not-text");
    std::fs::write(&not_text, [0xff, 0xfe]).expect("file");
    let control = work.join("control");
    std::fs::write(&control, "a\u{7}b\n").expect("file");
    let missing = work.join("missing");
    let to = |to: &str| strings(&["push", &text(&layout), "--to", to, "--plain-http"]);
    let with = |file: &Path| {
        let mut args = to(&format!("127.0.0.1:{port}/team/bundle:1"));
        args.extend(strings(&["--user", "u", "--password-file", &text(file)]));
        args
    };
    for (args, reason) in [
        (
            to(&format!("127.0.0.1:{port}/team/bundle")),
            format!("--to \"127.0.0.1:{port}/team/bundle\" is not REGISTRY/REPOSITORY:TAG"),
        ),
        (
            to("bundle"),
            "--to \"bundle\" is not REGISTRY/REPOSITORY:TAG".to_owned(),
        ),
        (
            to(&format!("127.0.0.1:{port}/team/bundle:-1")),
            r#""-1" is not a tag: ASCII letters, digits, `_`, `.` and `-`, not starting with `.` or `-`, at most 128 characters"#.to_owned(),
        ),
        (
            to("bundle:1"),
            r#""bundle" is not a well formed registry reference"#.to_owned(),
        ),
        (
            with(&missing),
            format!("{}: No such file or directory (os error 2)", missing.display()),
        ),
        (
            with(&not_text),
            format!("{}: the password file is not UTF-8 text", not_text.display()),
        ),
        (
            with(&control),
            "registry credentials: the password is empty or holds a control character"
                .to_owned(),
        ),
    ] {
        let refused = run_off(args).await;
        assert_eq!(refused.code, Some(1), "{reason}");
        assert_eq!(refused.stderr, format!("js-bundle: {reason}\n"));
    }
    let usage = run_off(strings(&[
        "push",
        &text(&layout),
        "--to",
        "127.0.0.1:1/team/bundle:1",
        "--user",
        "u",
    ]))
    .await;
    assert_eq!(usage.code, Some(2), "{}", usage.stderr);
    assert!(usage.stderr.contains("--password-file"), "{}", usage.stderr);
    assert_eq!(fx.requests(), Vec::<String>::new());
}

/// Proves a push without `--plain-http` speaks https only, so a registry on this machine is
/// not reached at all: the client refuses the loopback address before it connects. Catches
/// a push that falls back to plain http, or reaches loopback without being told to.
#[tokio::test(flavor = "multi_thread")]
async fn push_without_plain_http_does_not_reach_a_registry_on_loopback() {
    let work = work("js-bundle-push-https");
    let (layout, _) = packed(&work, "layout", "one");
    let fx = Fixture::start_plain().await;
    let args = strings(&[
        "push",
        &text(&layout),
        "--to",
        &format!("127.0.0.1:{}/team/bundle:1", fx.port),
    ]);
    let refused = run_off(args).await;
    assert_eq!(refused.code, Some(1));
    assert_eq!(
        refused.stderr,
        "js-bundle: 127.0.0.1 resolves to 127.0.0.1, a loopback address\n"
    );
    assert_eq!(fx.requests(), Vec::<String>::new());
}
