//! `js_testkit::s3`: where the test store is, read from the two variables, and the unique
//! names the tests put in it. Nothing here needs the store.

use js_testkit::s3::{NoStore, REGION_VAR, TestStore, URL_VAR, unique};

const URL: &str = "http://root-id:root/se:cret@127.0.0.1:9000/js-live-logs";

/// Asserts that a good URL and region give each part, the secret with a `:` and a `/` in it
/// kept whole, and that `Debug` names everything but the secret.
#[test]
fn a_good_url_gives_every_part_and_debug_hides_the_secret() {
    let store = TestStore::parse(Some(URL), Some("js-test-1")).expect("a store");
    assert_eq!(
        store,
        TestStore {
            base_url: "http://127.0.0.1:9000".to_owned(),
            region: "js-test-1".to_owned(),
            bucket: "js-live-logs".to_owned(),
            access_key_id: "root-id".to_owned(),
            secret_access_key: "root/se:cret".to_owned(),
        }
    );
    assert_eq!(
        format!("{store:?}"),
        "TestStore { base_url: \"http://127.0.0.1:9000\", region: \"js-test-1\", \
         bucket: \"js-live-logs\", access_key_id: \"root-id\", .. }"
    );
    let https = TestStore::parse(Some("https://a:b@store.example/bkt"), Some("r"));
    assert_eq!(https.expect("a store").base_url, "https://store.example");
}

/// Asserts that a missing or empty variable is `Unset`, and that each malformed part is
/// named by the part.
#[test]
fn each_missing_or_malformed_part_is_named() {
    let unset = [
        (None, Some("r")),
        (Some(URL), None),
        (Some(""), Some("r")),
        (Some(URL), Some("")),
    ];
    for (url, region) in unset {
        assert_eq!(
            TestStore::parse(url, region),
            Err(NoStore::Unset),
            "{url:?} {region:?}"
        );
    }
    let malformed = [
        ("ftp://a:b@h:1/b", "the scheme is not http:// or https://"),
        ("127.0.0.1:9000/b", "the scheme is not http:// or https://"),
        ("http://h:1/b", "there is no ACCESS_KEY_ID:SECRET@"),
        ("http://ab@h:1/b", "the key is not ACCESS_KEY_ID:SECRET"),
        ("http://:b@h:1/b", "the key is not ACCESS_KEY_ID:SECRET"),
        ("http://a:@h:1/b", "the key is not ACCESS_KEY_ID:SECRET"),
        ("http://a:b@h:1", "the place is not HOST:PORT/BUCKET"),
        ("http://a:b@/b", "the place is not HOST:PORT/BUCKET"),
        ("http://a:b@h:1/", "the place is not HOST:PORT/BUCKET"),
        ("http://a:b@h:1/b/c", "the place is not HOST:PORT/BUCKET"),
    ];
    for (url, part) in malformed {
        assert_eq!(
            TestStore::parse(Some(url), Some("r")),
            Err(NoStore::Malformed(part)),
            "{url}"
        );
    }
}

/// Asserts that a test without a store fails, and does not skip, with the whole message:
/// what is wrong, and both variables and the form to set them to; the value of a malformed
/// URL is not in it.
#[test]
fn a_missing_store_fails_the_test_with_how_to_set_it() {
    let how = "this test needs an S3-compatible store with STS. Start one, create a bucket, \
               and set JS_TEST_S3_URL=http://ACCESS_KEY_ID:SECRET@HOST:PORT/BUCKET (the root \
               key) and JS_TEST_S3_REGION=REGION (CI's service container sets both)";
    let cases = [
        (
            None,
            format!("JS_TEST_S3_URL or JS_TEST_S3_REGION is not set: {how}"),
        ),
        (
            Some("http://id:hunter2@h:1"),
            format!("JS_TEST_S3_URL is malformed: the place is not HOST:PORT/BUCKET: {how}"),
        ),
    ];
    for (url, want) in cases {
        let failure = std::panic::catch_unwind(|| TestStore::or_fail(url, Some("r")))
            .expect_err("a missing store fails the test");
        let said = failure
            .downcast_ref::<String>()
            .expect("a formatted message");
        assert_eq!(said, &want);
    }
    let good = TestStore::or_fail(Some(URL), Some("js-test-1"));
    assert_eq!(good.bucket, "js-live-logs");
}

/// Asserts that the variables are read where the test runs: what `from_env` gives is what
/// `parse` makes of the two variables as the process has them, a failure included.
#[test]
fn from_env_reads_the_two_variables() {
    let url = std::env::var(URL_VAR).ok();
    let region = std::env::var(REGION_VAR).ok();
    let want = TestStore::parse(url.as_deref(), region.as_deref()).ok();
    assert_eq!(std::panic::catch_unwind(TestStore::from_env).ok(), want);
}

/// Asserts that two calls never give one name, and that a name keeps its label and holds only
/// what a job id or a key segment may.
#[test]
fn unique_names_differ_and_keep_their_label() {
    let (a, b) = (unique("job"), unique("job"));
    assert_ne!(a, b);
    assert!(a.starts_with("job-"), "{a}");
    assert!(
        a.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
        "{a}"
    );
}
