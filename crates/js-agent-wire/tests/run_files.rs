//! Run files on a start: the bounds at their edges, the secrecy of the contents, and the
//! mixed-version shape of the field. Each test says the defect it catches.

mod common;

use common::{broken, expect, job, long};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{Command, HeartbeatReply, JobSpec, Rule, RunFile, validate_files};
use serde_json::json;

fn file(name: &str, contents: &str) -> RunFile {
    RunFile {
        name: name.into(),
        contents: contents.into(),
    }
}

fn check(edit: impl FnOnce(&mut JobSpec)) -> js_agent_wire::ValidationError {
    broken(&job(), edit, JobSpec::validate)
}

fn with(files: Vec<RunFile>) -> JobSpec {
    let mut spec = job();
    spec.files = files;
    spec
}

/// The number of files: the cap passes, one over is `too-many` under `invalid-file`.
/// Catches an unbounded list (each file is written to disk by the agent).
#[test]
fn the_number_of_files_is_bounded() {
    let many = |n: usize| (0..n).map(|i| file(&format!("f{i}"), "x")).collect();
    assert_eq!(with(many(MAX_RUN_FILES)).validate(), Ok(()));
    let err = check(|j| j.files = many(MAX_RUN_FILES + 1));
    expect(err, "files", Rule::TooMany, refusal::INVALID_FILE);
}

/// The name follows the grammar of a mount root name: `[A-Za-z0-9._-]`, leading
/// alphanumeric, at most the cap; the cap passes and one over, an empty name, a path
/// separator, a dot-dot, a leading dot or dash and a space are refused. Catches a name
/// that would escape the files directory or hide as a dotfile.
#[test]
fn names_follow_the_root_name_grammar() {
    assert_eq!(MAX_RUN_FILE_NAME_LEN, MAX_ROOT_NAME_LEN);
    assert_eq!(
        with(vec![file(&long(MAX_RUN_FILE_NAME_LEN), "x")]).validate(),
        Ok(())
    );
    assert_eq!(with(vec![file("put-url_1.txt", "x")]).validate(), Ok(()));
    let err = check(|j| j.files = vec![file(&long(MAX_RUN_FILE_NAME_LEN + 1), "x")]);
    expect(err, "files", Rule::TooLong, refusal::INVALID_FILE);
    let err = check(|j| j.files = vec![file("", "x")]);
    expect(err, "files", Rule::Empty, refusal::INVALID_FILE);
    for bad in [
        "a/b",
        "..",
        ".hidden",
        "-x",
        "a b",
        "a\0b",
        "/etc/passwd",
        "a\\b",
    ] {
        let err = check(|j| j.files = vec![file(bad, "x")]);
        expect(err, "files", Rule::BadChars, refusal::INVALID_FILE);
    }
}

/// A name used twice is a duplicate. Catches a second file silently replacing (or, with
/// O_EXCL, failing after the container's other files were written).
#[test]
fn a_name_is_unique() {
    let err = check(|j| j.files = vec![file("a", "1"), file("b", "2"), file("a", "3")]);
    expect(err, "files", Rule::Duplicate, refusal::INVALID_FILE);
}

/// Names that differ only in case are duplicates (`Token` and `token`). Catches two files
/// that are one on a case-folding file system, where the second would overwrite or fail
/// the first.
#[test]
fn names_differing_only_in_case_are_duplicates() {
    let err = check(|j| j.files = vec![file("Token", "1"), file("token", "2")]);
    expect(err, "files", Rule::Duplicate, refusal::INVALID_FILE);
    assert_eq!(
        with(vec![file("token", "1"), file("token2", "2")]).validate(),
        Ok(())
    );
}

/// One file's contents: the cap passes, one over is refused, and a NUL is refused at any
/// position. Catches an unbounded file and a NUL that would truncate the contents where a
/// C reader stops.
#[test]
fn one_file_is_bounded_and_holds_no_nul() {
    assert_eq!(
        with(vec![file("a", &long(MAX_RUN_FILE_LEN))]).validate(),
        Ok(())
    );
    let err = check(|j| j.files = vec![file("a", &long(MAX_RUN_FILE_LEN + 1))]);
    expect(err, "files", Rule::TooLong, refusal::INVALID_FILE);
    for text in ["\0", "ab\0", "\0ab", "a\0b"] {
        let err = check(|j| j.files = vec![file("a", text)]);
        expect(err, "files", Rule::BadChars, refusal::INVALID_FILE);
    }
    // Text that is not printable is allowed: a token may hold a newline or a tab.
    assert_eq!(with(vec![file("a", "line\n\ttab")]).validate(), Ok(()));
}

/// The sum of all contents: exactly the cap passes (four files of the single-file cap),
/// one byte more is refused although every file is within its own cap. Catches a total
/// that is only checked per file.
#[test]
fn the_total_is_bounded() {
    let each = MAX_RUN_FILE_LEN;
    let n = MAX_RUN_FILES_TOTAL_BYTES / each;
    assert_eq!(n * each, MAX_RUN_FILES_TOTAL_BYTES);
    let at_cap: Vec<RunFile> = (0..n)
        .map(|i| file(&format!("f{i}"), &long(each)))
        .collect();
    assert_eq!(with(at_cap.clone()).validate(), Ok(()));
    let mut over = at_cap;
    over.pop();
    over.push(file("last", &long(each - 1)));
    over.push(file("one-more", "xx"));
    let err = check(|j| j.files = over);
    expect(err, "files", Rule::TooLong, refusal::INVALID_FILE);
}

/// The standalone check the run API calls gives the same verdict as the spec's, so a bad
/// file is answered before the job is built. Catches the two drifting apart.
#[test]
fn the_standalone_check_agrees_with_the_spec() {
    let bad = vec![file("a", "1"), file("a", "2")];
    assert_eq!(
        validate_files(&bad).unwrap_err().code(),
        refusal::INVALID_FILE
    );
    assert_eq!(validate_files(&[]), Ok(()));
}

/// `Debug` of a file, of a spec, of a command and of a whole reply never holds the
/// contents, only the name and the length. Catches a derived Debug that would put a
/// presigned URL in a log line or a panic message. Also checks the error text of a bad
/// file repeats neither the name nor the contents.
#[test]
fn debug_never_shows_the_contents() {
    let secret = "https://store.example/put?sig=SECRET-VALUE-123";
    let spec = with(vec![file("put-url", secret)]);
    let one = format!("{:?}", spec.files[0]);
    assert!(
        one.contains("put-url") && one.contains(&secret.len().to_string()),
        "{one}"
    );
    for text in [
        one,
        format!("{spec:?}"),
        format!("{:#?}", spec),
        format!("{:?}", Command::Start(Box::new(spec.clone()))),
        format!(
            "{:?}",
            HeartbeatReply {
                commands: vec![Command::Start(Box::new(spec.clone()))],
                ..HeartbeatReply::default()
            }
        ),
    ] {
        assert!(!text.contains("SECRET-VALUE"), "{text}");
        assert!(!text.contains("store.example"), "{text}");
    }
    let bad = vec![file("secret-name", &format!("{secret}\0"))];
    let err = validate_files(&bad).unwrap_err();
    let shown = format!("{err} {err:?}");
    assert!(
        !shown.contains("SECRET-VALUE") && !shown.contains("secret-name"),
        "{shown}"
    );
}

/// The wire shape: a spec without files writes no `files` key (so an old agent sees the
/// bytes it always saw), a spec with files round-trips, and a start written by an old
/// server (no key) decodes with none. Catches a default that is not additive.
#[test]
fn the_field_is_additive_on_the_wire() {
    let plain = serde_json::to_value(job()).unwrap();
    assert!(plain.get("files").is_none(), "{plain}");
    let old: JobSpec = serde_json::from_value(plain).unwrap();
    assert!(old.files.is_empty());
    let spec = with(vec![file("a", "x")]);
    let value = serde_json::to_value(&spec).unwrap();
    assert_eq!(value["files"], json!([{ "name": "a", "contents": "x" }]));
    assert_eq!(serde_json::from_value::<JobSpec>(value).unwrap(), spec);
}

/// A start carrying a bad file inside a reply is refused by the command's own validation,
/// with the file code. Catches a validator reachable only through `JobSpec::validate` and
/// skipped by the path the agent uses.
#[test]
fn a_command_with_a_bad_file_is_refused() {
    let spec = with(vec![file("../x", "x")]);
    let err = Command::Start(Box::new(spec)).validate().unwrap_err();
    assert_eq!(err.code(), refusal::INVALID_FILE);
}
