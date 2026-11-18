//! `channels descriptor write`: what the merge adds and keeps, what it refuses, and what is put,
//! in which condition, against the in-memory S3 of the apt tests.

use super::*;
use crate::channels::apt::testing::{BUCKET, DENIED, FakeAws, etag, flag};
use js_testkit::dir::TestDir;

const DISTRIBUTION: &str = "E1ABC23";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-descriptor-{name}"))
}

/// What hostkeeper's release leaves: its own key, channels and formats, and its own packages.
fn hostkeeper_descriptor() -> Value {
    json!({
        "version": 1,
        "name": "komira",
        "unknown-key": { "kept": [1, 2, 3] },
        "channels": ["testing", "stable"],
        "key": { "keyring": "apt/keyring.gpg", "fingerprints": ["AA", "BB"] },
        "formats": {
            "apt": { "path": "apt", "architectures": ["amd64"] },
            "rpm": { "path": "rpm", "architectures": ["x86_64"] },
            "macos-pkg": {
                "path": "pkg",
                "platforms": ["macos-arm64"],
                "packages": {
                    "hostkeeper": { "dir": "hostkeeper" },
                    "hello": { "dir": "hostkeeper", "extra": true }
                }
            }
        }
    })
}

fn merged(existing: Value) -> Value {
    merge(existing).expect("merged")
}

fn failed(existing: Value) -> String {
    let Err(Error::Failed(why)) = merge(existing) else {
        panic!("not a failure");
    };
    why
}

fn stored(aws: &FakeAws) -> Value {
    serde_json::from_slice(&aws.get(FILE).expect("descriptor")).expect("json")
}

fn put_descriptor(aws: &FakeAws, value: &Value) {
    aws.put(FILE, format!("{value:#}\n").as_bytes());
}

fn request<'a>(work: &'a Path, distribution_id: &'a str, attempts: u32) -> Request<'a> {
    Request {
        bucket: BUCKET,
        distribution_id,
        work,
        attempts,
    }
}

fn write_with(
    name: &str,
    aws: &FakeAws,
    distribution_id: &str,
    attempts: u32,
) -> Result<Vec<String>, Error> {
    let dir = scratch(name);
    let work = dir.join("work");
    write(aws, &request(&work, distribution_id, attempts))
}

#[test]
fn the_merge_adds_js_agent_and_keeps_every_other_key_as_it_was() {
    // Behaviour: another product's facts and keys this build does not know survive the merge.
    let mut expected = hostkeeper_descriptor();
    expected["formats"]["macos-pkg"]["packages"]["js-agent"] = json!({ "dir": "jobscheduler" });
    assert_eq!(merged(hostkeeper_descriptor()), expected);
}

#[test]
fn the_merge_makes_the_macos_pkg_format_when_there_is_none() {
    // Behaviour: a descriptor with no formats, or no macos-pkg, gets exactly this release's part.
    let want = json!({
        "path": "pkg",
        "platforms": ["macos-arm64"],
        "packages": { "js-agent": { "dir": "jobscheduler" } }
    });
    assert_eq!(merged(json!({}))["formats"]["macos-pkg"], want);
    let other = json!({ "version": 1, "formats": { "apt": { "path": "apt" } } });
    let got = merged(other);
    assert_eq!(got["formats"]["macos-pkg"], want);
    assert_eq!(got["formats"]["apt"], json!({ "path": "apt" }));
}

#[test]
fn the_merge_is_a_no_op_when_the_mapping_is_there() {
    // Behaviour: merging twice changes nothing the second time.
    let once = merged(hostkeeper_descriptor());
    assert_eq!(merged(once.clone()), once);
}

#[test]
fn the_merge_unites_platforms_without_duplicates() {
    // Behaviour: platforms of other products stay, in order, and ours is added once.
    let mut theirs = hostkeeper_descriptor();
    theirs["formats"]["macos-pkg"]["platforms"] = json!(["macos-x86_64"]);
    let once = merged(theirs);
    assert_eq!(
        once["formats"]["macos-pkg"]["platforms"],
        json!(["macos-x86_64", "macos-arm64"])
    );
    assert_eq!(
        merged(once.clone())["formats"]["macos-pkg"]["platforms"],
        once["formats"]["macos-pkg"]["platforms"]
    );
}

#[test]
fn the_merge_refuses_another_directory_for_js_agent_naming_both() {
    let mut theirs = hostkeeper_descriptor();
    theirs["formats"]["macos-pkg"]["packages"]["js-agent"] = json!({ "dir": "elsewhere" });
    assert_eq!(
        failed(theirs),
        "the descriptor maps js-agent to the directory \"elsewhere\", and this release maps it to jobscheduler"
    );
    let mut none = hostkeeper_descriptor();
    none["formats"]["macos-pkg"]["packages"]["js-agent"] = json!({});
    assert!(failed(none).contains("to the directory null,"));
}

#[test]
fn the_merge_refuses_another_path() {
    let mut theirs = hostkeeper_descriptor();
    theirs["formats"]["macos-pkg"]["path"] = json!("mac");
    assert_eq!(
        failed(theirs),
        "the descriptor's formats.macos-pkg.path is \"mac\", and the release writes its package to pkg"
    );
    let mut number = hostkeeper_descriptor();
    number["formats"]["macos-pkg"]["path"] = json!(3);
    assert!(failed(number).contains("path is 3,"));
}

#[test]
fn the_merge_refuses_a_newer_version_or_one_that_is_no_number() {
    let mut newer = hostkeeper_descriptor();
    newer["version"] = json!(2);
    assert_eq!(
        failed(newer),
        "the descriptor in the bucket is version 2, newer than this build reads (1)"
    );
    let mut text = hostkeeper_descriptor();
    text["version"] = json!("1");
    assert_eq!(
        failed(text),
        "the descriptor's version is \"1\", not a number"
    );
    let mut older = hostkeeper_descriptor();
    older["version"] = json!(0);
    merged(older);
}

#[test]
fn the_merge_refuses_a_part_that_is_not_of_its_form() {
    assert_eq!(
        failed(json!([])),
        "the descriptor in the bucket is not a JSON object"
    );
    assert_eq!(
        failed(json!({ "formats": [] })),
        "the descriptor's formats is not an object"
    );
    assert_eq!(
        failed(json!({ "formats": { "macos-pkg": "x" } })),
        "the descriptor's formats.macos-pkg is not an object"
    );
    assert_eq!(
        failed(json!({ "formats": { "macos-pkg": { "packages": [] } } })),
        "the descriptor's formats.macos-pkg.packages is not an object"
    );
    assert_eq!(
        failed(json!({ "formats": { "macos-pkg": { "platforms": "macos-arm64" } } })),
        "the descriptor's formats.macos-pkg.platforms is not a list"
    );
}

#[test]
fn a_write_puts_the_merge_if_match_the_etag_read_and_changes_nothing_else() {
    // Behaviour: one conditional single-part put of the merged descriptor, no invalidation
    // without a distribution.
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    let read_etag = etag(&aws.get(FILE).expect("descriptor"));
    let said = write_with("put", &aws, "", 3).expect("written");
    assert_eq!(said, [format!("{FILE}: written")]);
    assert_eq!(stored(&aws), merged(hostkeeper_descriptor()));
    let puts = aws.calls_of("put-object");
    assert_eq!(puts.len(), 1);
    assert_eq!(flag(&puts[0], "--if-match"), Some(read_etag));
    assert_eq!(flag(&puts[0], "--if-none-match"), None);
    assert_eq!(flag(&puts[0], "--bucket").as_deref(), Some(BUCKET));
    assert_eq!(flag(&puts[0], "--key").as_deref(), Some(FILE));
    assert_eq!(
        flag(&puts[0], "--content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        flag(&puts[0], "--cache-control").as_deref(),
        Some("public, max-age=60")
    );
    assert!(aws.calls_of("create-invalidation").is_empty());
}

#[test]
fn a_write_with_a_distribution_invalidates_the_descriptor_after_the_put() {
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    let said = write_with("invalidate", &aws, DISTRIBUTION, 3).expect("written");
    assert_eq!(
        said,
        [format!("{FILE}: written"), format!("/{FILE}: invalidated")]
    );
    let calls = aws.calls.borrow();
    let last = calls.last().expect("a call");
    assert_eq!(
        last,
        &strings(&[
            "cloudfront",
            "create-invalidation",
            "--distribution-id",
            DISTRIBUTION,
            "--paths",
            "/hostkeeper-repository.json"
        ])
    );
}

#[test]
fn a_failed_invalidation_is_an_error_with_the_answer() {
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    aws.refuse_when("create-invalidation", "Access Denied\n");
    assert_eq!(
        write_with("invalidate-denied", &aws, DISTRIBUTION, 3).unwrap_err(),
        Error::Failed("invalidate /hostkeeper-repository.json: Access Denied".to_owned())
    );
}

#[test]
fn a_descriptor_that_already_maps_js_agent_is_not_written_and_not_invalidated() {
    // Behaviour: a re-run of the release writes nothing and says so.
    let aws = FakeAws::new();
    put_descriptor(&aws, &merged(hostkeeper_descriptor()));
    let said = write_with("same", &aws, DISTRIBUTION, 3).expect("no change");
    assert_eq!(
        said,
        ["hostkeeper-repository.json: already maps js-agent to pkg/jobscheduler; nothing written"]
    );
    assert!(aws.calls_of("put-object").is_empty());
    assert!(aws.calls_of("create-invalidation").is_empty());
}

#[test]
fn a_lost_race_is_read_again_and_decided_again() {
    // Behaviour: another release's write between the read and the put is kept, and ours added.
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    let mut changed = hostkeeper_descriptor();
    changed["formats"]["macos-pkg"]["packages"]["other"] = json!({ "dir": "other" });
    let racer = changed.clone();
    aws.before_put_of(
        FILE,
        Box::new(move |store| {
            store.insert(FILE.to_owned(), format!("{racer:#}\n").into_bytes());
        }),
    );
    let said = write_with("race", &aws, "", 3).expect("written");
    assert_eq!(said, [format!("{FILE}: written")]);
    assert_eq!(aws.calls_of("get-object").len(), 2);
    assert_eq!(aws.calls_of("put-object").len(), 2);
    assert_eq!(stored(&aws), merged(changed));
}

#[test]
fn attempts_that_all_lose_are_an_error() {
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    aws.refuse_when("put-object", "An error occurred (PreconditionFailed)\n");
    assert_eq!(
        write_with("exhausted", &aws, "", 2).unwrap_err(),
        Error::Failed(
            "hostkeeper-repository.json was written by another release 2 times in a row; run the job again"
                .to_owned()
        )
    );
    assert_eq!(aws.calls_of("get-object").len(), 2);
    assert_eq!(aws.calls_of("put-object").len(), 2);
}

#[test]
fn an_absent_descriptor_is_refused_with_no_put() {
    // Behaviour: hostkeeper's release creates the descriptor; this release only adds to it.
    let want = Error::Failed(
        "hostkeeper-repository.json is not in the bucket (or the role cannot read it): hostkeeper's release creates it, and this release only adds to it"
            .to_owned(),
    );
    let aws = FakeAws::new();
    assert_eq!(write_with("absent", &aws, "", 3).unwrap_err(), want);
    assert!(aws.calls_of("put-object").is_empty());
    let aws = FakeAws::new();
    *aws.missing.borrow_mut() = DENIED.to_owned();
    assert_eq!(write_with("hidden", &aws, "", 3).unwrap_err(), want);
    assert!(aws.calls_of("put-object").is_empty());
}

#[test]
fn a_descriptor_that_cannot_be_merged_is_refused_with_no_put() {
    let aws = FakeAws::new();
    let mut newer = hostkeeper_descriptor();
    newer["version"] = json!(2);
    put_descriptor(&aws, &newer);
    assert!(matches!(
        write_with("newer", &aws, "", 3).unwrap_err(),
        Error::Failed(why) if why.contains("version 2")
    ));
    assert!(aws.calls_of("put-object").is_empty());

    let aws = FakeAws::new();
    aws.put(FILE, b"not json");
    let Error::Failed(why) = write_with("not-json", &aws, "", 3).unwrap_err() else {
        panic!("not a failure");
    };
    assert!(
        why.starts_with("hostkeeper-repository.json in the bucket is not JSON: "),
        "{why}"
    );
    assert!(aws.calls_of("put-object").is_empty());
}

#[test]
fn an_unreadable_descriptor_and_an_answer_without_etag_are_errors() {
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    aws.refuse_when("get-object", "Internal Error\n");
    assert_eq!(
        write_with("read-fails", &aws, "", 3).unwrap_err(),
        Error::Failed("read hostkeeper-repository.json: Internal Error".to_owned())
    );
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    *aws.stdout.borrow_mut() = Some("{}".to_owned());
    assert_eq!(
        write_with("no-etag", &aws, "", 3).unwrap_err(),
        Error::Failed("read hostkeeper-repository.json: aws printed no ETag".to_owned())
    );
    assert!(aws.calls_of("put-object").is_empty());
}

#[test]
fn a_put_refused_for_another_reason_stops_with_the_answer() {
    let aws = FakeAws::new();
    put_descriptor(&aws, &hostkeeper_descriptor());
    aws.refuse_when("put-object", "Access Denied\n");
    assert_eq!(
        write_with("put-denied", &aws, "", 3).unwrap_err(),
        Error::Failed("put hostkeeper-repository.json: Access Denied".to_owned())
    );
    assert_eq!(aws.calls_of("put-object").len(), 1);
}

#[test]
fn flags_out_of_their_form_are_usage_errors_before_anything_is_called() {
    let aws = FakeAws::new();
    let dir = scratch("usage");
    let work = dir.join("work");
    for (bucket, distribution_id, attempts, flag) in [
        ("Not A Bucket", "", 3, "--bucket"),
        ("", "", 3, "--bucket"),
        (BUCKET, "e1", 3, "--distribution-id"),
        (BUCKET, "", 0, "--attempts"),
    ] {
        let r = Request {
            bucket,
            distribution_id,
            work: &work,
            attempts,
        };
        let Err(Error::Usage(why)) = write(&aws, &r) else {
            panic!("{bucket} {distribution_id} {attempts}: not a usage error");
        };
        assert!(why.starts_with(flag), "{why}");
    }
    assert!(aws.calls.borrow().is_empty());
    assert!(!work.exists());
}

#[test]
fn a_work_directory_or_next_file_that_cannot_be_made_is_named() {
    let dir = scratch("work");
    std::fs::write(dir.join("work"), "a file").expect("file");
    let aws = FakeAws::new();
    let Error::Spawn { what, .. } = write(&aws, &request(&dir.join("work"), "", 3)).unwrap_err()
    else {
        panic!("not a spawn error");
    };
    assert_eq!(what, format!("create {}", dir.join("work").display()));
    assert!(aws.calls.borrow().is_empty());

    let work = dir.join("blocked");
    std::fs::create_dir_all(work.join("descriptor-next.json")).expect("dir");
    put_descriptor(&aws, &hostkeeper_descriptor());
    let Error::Spawn { what, .. } = write(&aws, &request(&work, "", 3)).unwrap_err() else {
        panic!("not a spawn error");
    };
    assert_eq!(
        what,
        format!("write {}", work.join("descriptor-next.json").display())
    );
    assert!(aws.calls_of("put-object").is_empty());
}

#[test]
fn the_verb_runs_against_the_program_it_is_given() {
    // Behaviour: the flags reach the write, and its `aws` is the one named.
    let dir = scratch("run");
    let missing = dir.join("no-aws");
    let Err(Error::Spawn { what, .. }) = run(Verb::Write {
        bucket: BUCKET.to_owned(),
        distribution_id: String::new(),
        work: dir.join("work"),
        attempts: 3,
        aws: missing.clone(),
    }) else {
        panic!("not a spawn error");
    };
    assert_eq!(what, missing.display().to_string());
}
