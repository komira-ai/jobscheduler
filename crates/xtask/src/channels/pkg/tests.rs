//! `channels pkg upload`: what is put and in which order, what a `412` answers, and what is
//! refused before anything is put, against the in-memory S3 of the apt tests.

use super::*;
use crate::channels::apt::testing::{BUCKET, DENIED, FakeAws, flag};
use crate::channels::sums::{self, SUMS};
use crate::fetch::Sha256;
use js_testkit::dir::TestDir;

const PREFIX_1_2_3: &str = "pkg/jobscheduler/1.2.3/";
const PKG: &str = "js-agent-1.2.3-macos-arm64.pkg";
const PKG_BYTES: &[u8] = b"signed package bytes";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-pkg-{name}"))
}

fn line(name: &str, bytes: &[u8]) -> sums::Line {
    sums::Line {
        sha256: Sha256::of(bytes),
        name: name.to_owned(),
    }
}

/// The text of a sums file, as `sha256sum` writes it.
fn render(lines: &[sums::Line]) -> String {
    lines
        .iter()
        .map(|l| format!("{}  {}\n", l.sha256, l.name))
        .collect()
}

/// A release directory of 1.2.3 holding the package and a `SHA256SUMS` listing it, and a work
/// directory beside it.
struct Fixture {
    scratch: TestDir,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let scratch = scratch(name);
        std::fs::create_dir(scratch.join("release")).expect("release");
        std::fs::write(scratch.join("release").join(PKG), PKG_BYTES).expect("pkg");
        let fixture = Fixture { scratch };
        fixture.sums(&[line(PKG, PKG_BYTES)]);
        fixture
    }

    fn sums(&self, lines: &[sums::Line]) {
        std::fs::write(self.scratch.join("release").join(SUMS), render(lines)).expect("sums");
    }

    fn with<T>(&self, version: &str, bucket: &str, f: impl FnOnce(&Request) -> T) -> T {
        let release = self.scratch.join("release");
        let work = self.scratch.join("work");
        f(&Request {
            dir: &release,
            version,
            bucket,
            work: &work,
        })
    }

    fn upload(&self, aws: &FakeAws) -> Result<Vec<String>, Error> {
        self.with("1.2.3", BUCKET, |r| upload::upload(aws, r))
    }
}

fn keys() -> [String; 2] {
    [
        format!("{PREFIX_1_2_3}{PKG}"),
        format!("{PREFIX_1_2_3}SHA256SUMS"),
    ]
}

#[test]
fn the_package_is_put_before_its_sums_each_conditionally_and_nothing_else() {
    // Behaviour: a reader that finds the sums finds the package they list; no write replaces.
    let f = Fixture::new("first");
    let aws = FakeAws::new();
    let said = f.upload(&aws).expect("uploaded");
    assert_eq!(aws.keys_put(), keys());
    assert_eq!(said, keys().map(|k| format!("{k}: written")).to_vec());
    assert_eq!(aws.store.borrow().len(), 2);
    assert_eq!(aws.get(&keys()[0]).expect("pkg"), PKG_BYTES);
    assert_eq!(
        aws.get(&keys()[1]).expect("sums"),
        render(&[line(PKG, PKG_BYTES)]).into_bytes()
    );
    let calls = aws.calls_of("put-object");
    for call in &calls {
        assert_eq!(flag(call, "--if-none-match").as_deref(), Some("*"));
        assert_eq!(flag(call, "--if-match"), None);
        assert_eq!(flag(call, "--bucket").as_deref(), Some(BUCKET));
        assert_eq!(
            flag(call, "--cache-control").as_deref(),
            Some("public, max-age=31536000, immutable")
        );
    }
    let types: Vec<String> = calls
        .iter()
        .map(|c| flag(c, "--content-type").expect("type"))
        .collect();
    assert_eq!(
        types,
        ["application/octet-stream", "text/plain; charset=utf-8"]
    );
}

#[test]
fn a_412_with_the_same_bytes_passes_and_writes_nothing_new() {
    // Behaviour: a re-run of the job after a partial upload finishes it.
    let f = Fixture::new("rerun");
    let aws = FakeAws::new();
    f.upload(&aws).expect("first");
    let before = aws.store.borrow().clone();
    let said = f.upload(&aws).expect("second");
    assert_eq!(*aws.store.borrow(), before);
    assert_eq!(
        said,
        keys()
            .map(|k| format!("{k}: already there with the same bytes"))
            .to_vec()
    );
}

#[test]
fn a_package_already_stored_is_skipped_and_the_sums_still_written() {
    // Behaviour: the job that died between the two puts is completed by its re-run.
    let f = Fixture::new("resume");
    let aws = FakeAws::new();
    aws.put(&keys()[0], PKG_BYTES);
    let said = f.upload(&aws).expect("uploaded");
    assert_eq!(
        said[0],
        format!("{}: already there with the same bytes", keys()[0])
    );
    assert_eq!(said[1], format!("{}: written", keys()[1]));
}

#[test]
fn a_412_with_a_different_package_fails_and_the_sums_are_not_put() {
    // Behaviour: a different file at a version's key is never written over and never listed.
    let f = Fixture::new("different");
    let aws = FakeAws::new();
    aws.put(&keys()[0], b"other bytes");
    let Error::Failed(why) = f.upload(&aws).unwrap_err() else {
        panic!("not a failure");
    };
    assert!(
        why.starts_with(&format!("{} exists with other bytes (sha256 ", keys()[0])),
        "{why}"
    );
    assert!(
        why.ends_with("a file of a version is never replaced"),
        "{why}"
    );
    assert_eq!(aws.get(&keys()[0]).expect("kept"), b"other bytes");
    assert_eq!(aws.get(&keys()[1]), None);
}

#[test]
fn a_412_with_different_sums_fails() {
    // Behaviour: the sums of a version are not replaced either.
    let f = Fixture::new("different-sums");
    let aws = FakeAws::new();
    aws.put(&keys()[1], b"other sums");
    let Error::Failed(why) = f.upload(&aws).unwrap_err() else {
        panic!("not a failure");
    };
    assert!(
        why.starts_with(&format!("{} exists with other bytes", keys()[1])),
        "{why}"
    );
    assert_eq!(aws.get(&keys()[1]).expect("kept"), b"other sums");
}

#[test]
fn a_412_whose_file_cannot_be_read_back_fails() {
    // Behaviour: an object that cannot be compared is not taken to be ours.
    let f = Fixture::new("unreadable");
    let aws = FakeAws::new();
    aws.put(&keys()[0], b"x");
    aws.refuse_when(
        &format!("get-object --bucket {BUCKET} --key {}", keys()[0]),
        DENIED,
    );
    assert_eq!(
        f.upload(&aws).unwrap_err(),
        Error::Failed(format!(
            "{} exists, and reading it back failed: {DENIED}",
            keys()[0]
        ))
    );
}

#[test]
fn a_refusal_that_is_not_a_412_fails_with_the_answer_and_stops() {
    // Behaviour: an AWS refusal ends the upload; the sums are not tried after a failed package.
    let f = Fixture::new("denied");
    let aws = FakeAws::new();
    aws.refuse_when("macos-arm64.pkg --body", "Access Denied\n");
    assert_eq!(
        f.upload(&aws).unwrap_err(),
        Error::Failed(format!("put {}: Access Denied", keys()[0]))
    );
    assert_eq!(aws.keys_put(), [keys()[0].clone()]);
}

#[test]
fn sums_that_list_another_file_are_refused_before_anything_is_put() {
    // Behaviour: the directory holds exactly the package and the sums that list it.
    let f = Fixture::new("other-file");
    f.sums(&[line("other-file", PKG_BYTES)]);
    let aws = FakeAws::new();
    assert_eq!(
        f.upload(&aws).unwrap_err(),
        Error::Refused(vec![format!("SHA256SUMS lists other-file, not {PKG}")])
    );
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn sums_that_list_the_package_and_another_file_are_refused_before_anything_is_put() {
    let f = Fixture::new("two-files");
    f.sums(&[line(PKG, PKG_BYTES), line("other-file", b"x")]);
    let aws = FakeAws::new();
    assert_eq!(
        f.upload(&aws).unwrap_err(),
        Error::Refused(vec![format!(
            "SHA256SUMS must list exactly {PKG}; it lists {PKG}, other-file"
        )])
    );
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn sums_that_list_the_package_at_another_digest_are_refused_before_anything_is_put() {
    let f = Fixture::new("other-digest");
    f.sums(&[line(PKG, b"not the package")]);
    let aws = FakeAws::new();
    assert_eq!(
        f.upload(&aws).unwrap_err(),
        Error::Refused(vec![format!(
            "{PKG} is {}, but SHA256SUMS lists {}",
            Sha256::of(PKG_BYTES),
            Sha256::of(b"not the package")
        )])
    );
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn sums_that_are_malformed_or_empty_are_refused_before_anything_is_put() {
    let f = Fixture::new("malformed");
    let sums_path = f.scratch.join("release").join(SUMS);
    std::fs::write(&sums_path, "not a sum\n").expect("sums");
    let aws = FakeAws::new();
    let Error::Refused(problems) = f.upload(&aws).unwrap_err() else {
        panic!("not a refusal");
    };
    assert_eq!(
        problems,
        [format!(
            "{}: line 1: \"not a sum\" is not `<sha256>  <file name>` and a newline",
            sums_path.display()
        )]
    );
    std::fs::write(&sums_path, "").expect("empty");
    assert_eq!(
        f.upload(&aws).unwrap_err(),
        Error::Refused(vec![format!(
            "SHA256SUMS must list exactly {PKG}; it lists "
        )])
    );
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn a_missing_package_or_sums_file_is_named() {
    let f = Fixture::new("missing");
    let aws = FakeAws::new();
    let release = f.scratch.join("release");
    std::fs::remove_file(release.join(SUMS)).expect("remove");
    let Error::Spawn { what, .. } = f.upload(&aws).unwrap_err() else {
        panic!("not a read error");
    };
    assert_eq!(what, format!("read {}", release.join(SUMS).display()));
    std::fs::remove_file(release.join(PKG)).expect("remove");
    let Error::Spawn { what, .. } = f.upload(&aws).unwrap_err() else {
        panic!("not a read error");
    };
    assert_eq!(what, format!("read {}", release.join(PKG).display()));
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn flags_out_of_their_form_are_usage_errors_before_anything_is_read() {
    let f = Fixture::new("usage");
    let aws = FakeAws::new();
    for (version, bucket, flag) in [
        ("1.2", BUCKET, "--version"),
        ("v1.2.3", BUCKET, "--version"),
        ("1.2.3", "Not A Bucket", "--bucket"),
        ("1.2.3", "", "--bucket"),
    ] {
        let Err(Error::Usage(why)) = f.with(version, bucket, |r| upload::upload(&aws, r)) else {
            panic!("{version} {bucket}: not a usage error");
        };
        assert!(why.starts_with(flag), "{why}");
    }
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn a_work_directory_that_cannot_be_made_is_named() {
    let f = Fixture::new("work");
    std::fs::write(f.scratch.join("work"), "a file").expect("file");
    let aws = FakeAws::new();
    let Error::Spawn { what, .. } = f.upload(&aws).unwrap_err() else {
        panic!("not a spawn error");
    };
    assert_eq!(what, format!("create {}", f.scratch.join("work").display()));
    assert!(aws.calls.borrow().is_empty());
}

#[test]
fn the_names_and_keys_are_those_of_the_version() {
    assert_eq!(file_name("10.20.30"), "js-agent-10.20.30-macos-arm64.pkg");
    assert_eq!(version_prefix("10.20.30"), "pkg/jobscheduler/10.20.30/");
}

#[test]
fn the_verb_runs_against_the_program_it_is_given() {
    // Behaviour: the flags reach the upload and its `aws` is the one named.
    let f = Fixture::new("run");
    let missing = f.scratch.join("no-aws");
    let Err(Error::Spawn { what, .. }) = run(Verb::Upload {
        dir: f.scratch.join("release"),
        version: "1.2.3".to_owned(),
        bucket: BUCKET.to_owned(),
        work: f.scratch.join("work"),
        aws: missing.clone(),
    }) else {
        panic!("not a spawn error");
    };
    assert_eq!(what, missing.display().to_string());
}
