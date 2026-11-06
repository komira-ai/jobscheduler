//! What a minted credential may do in the fake store, whichever minter minted it: the STS
//! minter, the fake minter and the prefix-grant minter all scope a host's credential to the
//! host's own prefix.

use std::sync::Arc;

use js_logmint::fake::FakeMinter;
use js_logmint::{MintRequest, Minter, host_segment};
use js_logstore::fake::{Action, Policy};
use js_logstore::{Credential, JobPrefix, Metadata, Stream};

use crate::support::{Rig, denied, request};

/// A credential for `host` under `prefix`, from the rig's STS minter, from a fake minter over
/// the same store and from a prefix-grant minter whose signing key the store holds.
fn minted(rig: &Rig, prefix: &str, host: &str) -> Vec<Credential> {
    let ask: MintRequest = request(prefix, host);
    let fake = FakeMinter::new(rig.fake.clone());
    let minters: [Arc<dyn Minter>; 3] = [
        Arc::new(rig.minter()),
        Arc::new(fake),
        Arc::new(rig.presigned_minter()),
    ];
    minters
        .iter()
        .map(|minter| minter.mint(&ask).unwrap())
        .collect()
}

/// The base of the jobs of `host` under the log prefix `prefix`: `prefix/js-<hash>`.
fn base(prefix: &str, host: &str) -> String {
    match prefix.is_empty() {
        true => host_segment(host),
        false => format!("{prefix}/{}", host_segment(host)),
    }
}

/// The keys of a chunk and a final of `job` under `base`.
fn keys(base: &str, job: &str) -> [String; 2] {
    let job = JobPrefix::new(base, job).unwrap();
    [
        job.chunk_key(Stream::Stdout, 1_048_576),
        job.final_key(Stream::Stderr),
    ]
}

/// Asserts, through the fake store and for each of the three minters, that the credential
/// minted for `host-1` with the log prefix `a/b` puts and deletes the chunks and finals of
/// `job-1` and `job-2` under `a/b/js-<hash of host-1>/`; that a put or a delete under the
/// sibling host's prefix `a/b/js-<hash of host-2>/`, directly under `a/b/` (where no job is
/// any more), under `a/bc/`, under `a/` or at the bucket root, and any get or list, even under
/// the host's own prefix, are `AccessDenied` and change nothing (objects the test put there
/// with another credential stay); and that with an empty log prefix the credential covers the
/// host's prefix at the bucket root and still not the sibling's. It catches a credential scoped
/// to the whole log prefix (which would write another host's jobs), a resource without the
/// trailing `/`, a minter that disagrees with the others on the host's prefix, and a
/// credential that can read.
#[test]
fn a_minted_credential_works_under_its_hosts_prefix_and_nowhere_else() {
    let rig = Rig::new();
    let admin = rig.client(rig.fake.issue(Policy {
        actions: vec![Action::Put, Action::Get],
        prefix: String::new(),
    }));
    let none = Metadata::new();
    let own = base("a/b", "host-1");
    let sibling = keys(&base("a/b", "host-2"), "job-1");
    let outside: Vec<String> = sibling
        .iter()
        .cloned()
        .chain([
            "a/b/job-1/stdout.log".to_owned(),
            "a/bc/job-1/stdout.log".to_owned(),
            "a/job-1/stdout.log".to_owned(),
            "job-1/stdout.log".to_owned(),
            format!("{own}x/job-1/stdout.log"),
        ])
        .collect();
    for key in &outside {
        admin.put(key, b"kept".to_vec(), &none).unwrap();
    }
    let before = rig.fake.objects();

    for credential in minted(&rig, "a/b", "host-1") {
        let client = rig.client(credential);
        for key in keys(&own, "job-1").iter().chain(&keys(&own, "job-2")) {
            client.put(key, b"bytes".to_vec(), &none).unwrap();
            assert_eq!(rig.fake.objects()[key], b"bytes");
            client.delete(key).unwrap();
        }
        let near = [own.clone(), "a/b/x".to_owned(), "other".to_owned()];
        for key in outside.iter().chain(&near) {
            assert!(denied(client.put(key, b"x".to_vec(), &none)), "{key}");
            assert!(denied(client.delete(key)), "{key}");
        }
        assert!(denied(client.get_range(&keys(&own, "job-1")[1], 0, 10)));
        assert!(denied(client.list(&format!("{own}/"), None, 10)));
        assert_eq!(rig.fake.objects(), before);
    }

    for credential in minted(&rig, "", "host-1") {
        let client = rig.client(credential);
        for key in &keys(&base("", "host-1"), "job-1") {
            client.put(key, b"bytes".to_vec(), &none).unwrap();
            client.delete(key).unwrap();
        }
        let sibling = &keys(&base("", "host-2"), "job-1")[0];
        assert!(denied(client.put(sibling, b"x".to_vec(), &none)));
        assert!(denied(client.get_range("job-1/stdout.log", 0, 10)));
        assert!(denied(client.list("", None, 10)));
        assert_eq!(rig.fake.objects(), before);
    }
}
