//! Credentials the store's STS mints (`AssumeRole` with the session policy js-logmint builds),
//! used by this crate's client: what they may do and what the store refuses them.

use std::time::Duration;

use js_logmint::{MintError, Minter, Refusal};
use js_logstore::{Metadata, StoreError, Stream};
use js_testkit::s3::unique;

use super::support::{Rig, SHORTEST_TTL};

/// The store's answer to a request the session policy does not allow.
fn denied() -> StoreError {
    StoreError::Denied {
        status: 403,
        code: "AccessDenied".to_owned(),
    }
}

/// Proves a minted credential, signed with its session token, may put and delete under its
/// host's prefix, for any job of the host, and that the store refuses it everywhere else: a
/// put and a delete under another host's prefix (whose object stays), a put under the log
/// prefix outside every host, and a put under a sibling whose name begins with the host's
/// segment but is not the host's prefix; and a read or a listing even under its own prefix.
/// Catches a session token the client does not sign or send, a session policy whose resource
/// is wider than the host's prefix, and one that grants reads.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_minted_credential_writes_its_hosts_prefix_and_nothing_else() {
    let rig = Rig::new();
    let root = rig.client(rig.root());
    let (credential, own) = rig.mint(&unique("host-a"));
    assert!(credential.session_token.is_some(), "a temporary credential");
    let host = rig.client(credential);

    let first = rig.job(&own, "job-1").chunk_key(Stream::Stdout, 0);
    let second = rig.job(&own, "job-2").final_key(Stream::Stderr);
    host.put(&first, b"one".to_vec(), &Metadata::new())
        .expect("its own job");
    host.put(&second, b"two".to_vec(), &Metadata::new())
        .expect("another job of its host");
    let read = |key: &str| root.get_range(key, 0, 16).expect("read").map(|o| o.bytes);
    assert_eq!(read(&first), Some(b"one".to_vec()));
    assert_eq!(read(&second), Some(b"two".to_vec()));
    assert_eq!(host.get_range(&first, 0, 3), Err(denied()));
    assert_eq!(host.list(own.as_str(), None, 10), Err(denied()));
    host.delete(&first).expect("delete under its own prefix");
    assert_eq!(read(&first), None);

    let other_host = rig.request(&unique("host-b"), SHORTEST_TTL);
    let other = other_host.host_prefix().expect("a host prefix");
    let theirs = rig.job(&other, "job-1").chunk_key(Stream::Stdout, 0);
    root.put(&theirs, b"theirs".to_vec(), &Metadata::new())
        .expect("put");
    assert_eq!(
        host.put(&theirs, b"mine".to_vec(), &Metadata::new()),
        Err(denied())
    );
    assert_eq!(host.delete(&theirs), Err(denied()));
    assert_eq!(read(&theirs), Some(b"theirs".to_vec()));

    let outside = rig.job(&rig.prefix, "job-1").chunk_key(Stream::Stdout, 0);
    let sibling = format!("{}x/job-1/stdout.log", own.as_str().trim_end_matches('/'));
    for key in [outside, sibling] {
        assert_eq!(
            host.put(&key, b"x".to_vec(), &Metadata::new()),
            Err(denied()),
            "{key}"
        );
        assert_eq!(read(&key), None, "{key}");
    }
}

/// Proves the boundary of the lifetime the store's STS issues: the shortest it takes (900 s,
/// what the live-log tests of js-agent and js-lease-server are configured with) is minted and
/// works; a second less is refused with the store's code, as a `Refused` the server does not
/// retry into success. Catches a `DurationSeconds` sent other than as asked, and a refusal read
/// as unavailable.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn the_shortest_lifetime_the_store_issues_is_minted_and_one_second_less_is_refused() {
    let rig = Rig::new();
    let minter = rig.minter_in(&rig.test.region);
    let host = unique("host-a");
    let short = rig.request(&host, SHORTEST_TTL - Duration::from_secs(1));
    assert_eq!(
        minter.mint(&short),
        Err(MintError::Refused(Refusal::Status {
            status: 400,
            code: "InvalidParameterValue".to_owned(),
        }))
    );
    let (credential, own) = rig.mint(&host);
    let key = rig.job(&own, "job-1").chunk_key(Stream::Stdout, 0);
    rig.client(credential)
        .put(&key, b"x".to_vec(), &Metadata::new())
        .expect("the shortest-lived credential works");
}
