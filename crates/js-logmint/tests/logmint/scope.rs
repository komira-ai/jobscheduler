//! What a minted credential may do in the fake store, whichever minter minted it.

use std::sync::Arc;

use js_logmint::fake::FakeMinter;
use js_logmint::{MintRequest, Minter};
use js_logstore::fake::{Action, Policy};
use js_logstore::{Credential, JobPrefix, Metadata, Stream};

use crate::support::{Rig, denied, request};

/// A credential for `host-1` under `prefix`, from the rig's STS minter and from a fake minter
/// over the same store.
fn minted(rig: &Rig, prefix: &str) -> Vec<Credential> {
    let ask: MintRequest = request(prefix, "host-1");
    let fake = FakeMinter::new(rig.fake.clone());
    let minters: [Arc<dyn Minter>; 2] = [Arc::new(rig.minter()), Arc::new(fake)];
    minters
        .iter()
        .map(|minter| minter.mint(&ask).unwrap())
        .collect()
}

/// The keys of a chunk and a final of `job` under the log prefix `prefix`.
fn keys(prefix: &str, job: &str) -> [String; 2] {
    let job = JobPrefix::new(prefix, job).unwrap();
    [
        job.chunk_key(Stream::Stdout, 1_048_576),
        job.final_key(Stream::Stderr),
    ]
}

/// Asserts, through the fake store, that the credential minted for `host-1` with the prefix
/// `a/b` (by the STS minter and by the fake minter) puts and deletes the chunks and finals of
/// `job-1` and of `job-2` under `a/b/`; that a put or a delete under `a/bc/`, under `a/` or at
/// the bucket root, and any get or list, even under `a/b/`, are `AccessDenied` and change
/// nothing (an object the test put outside the prefix with another credential stays); and that
/// with an empty prefix the credential covers the whole bucket for put and delete and still
/// reads nothing. It catches a scope that is not in the credential that was minted, a resource
/// without the prefix's trailing `/`, and a credential that can read.
#[test]
fn a_minted_credential_works_under_the_log_prefix_and_nowhere_else() {
    let rig = Rig::new();
    let admin = rig.client(rig.fake.issue(Policy {
        actions: vec![Action::Put, Action::Get],
        prefix: String::new(),
    }));
    let none = Metadata::new();
    let outside = [
        "a/bc/job-1/stdout.log",
        "a/job-1/stdout.log",
        "job-1/stdout.log",
    ];
    for key in outside {
        admin.put(key, b"kept".to_vec(), &none).unwrap();
    }
    let before = rig.fake.objects();

    for credential in minted(&rig, "a/b") {
        let client = rig.client(credential);
        for key in keys("a/b", "job-1").iter().chain(&keys("a/b", "job-2")) {
            client.put(key, b"bytes".to_vec(), &none).unwrap();
            assert_eq!(rig.fake.objects()[key], b"bytes");
            client.delete(key).unwrap();
        }
        for key in outside.iter().chain(&["a/b", "a/b.log", "other"]) {
            assert!(denied(client.put(key, b"x".to_vec(), &none)), "{key}");
            assert!(denied(client.delete(key)), "{key}");
        }
        assert!(denied(client.get_range(&keys("a/b", "job-1")[1], 0, 10)));
        assert!(denied(client.list("a/b/", None, 10)));
        assert_eq!(rig.fake.objects(), before);
    }

    for credential in minted(&rig, "") {
        let client = rig.client(credential);
        for key in keys("", "job-1").iter().chain(&keys("a/b", "job-2")) {
            client.put(key, b"bytes".to_vec(), &none).unwrap();
            client.delete(key).unwrap();
        }
        assert!(denied(client.get_range(outside[2], 0, 10)));
        assert!(denied(client.list("", None, 10)));
        assert_eq!(rig.fake.objects(), before);
    }
}
