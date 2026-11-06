//! The prefix-grant minter: the host's prefix it grants (the one the STS minter's session
//! policy names), the absolute expiry it sets from the injected clock, its refusals, and a
//! job's log written with its grant through the signer and read back from the fake store.

use std::time::Duration;

use js_logmint::fake::FakeMinter;
use js_logmint::{LogPrefix, MintError, MintRequest, Minter, PresignedPrefixMinter, Refusal};
use js_logstore::fake::{Action, Policy};
use js_logstore::{
    Credential, Endpoint, GrantError, JobPrefix, ManualClock, Metadata, PrefixGrant, PrefixSigner,
    Stream, Transport,
};

use crate::support::{BUCKET, REGION, Rig, START, URL, at, form, request};

/// The grant a credential's token writes.
fn grant_of(credential: &Credential) -> PrefixGrant {
    PrefixGrant::parse(credential.session_token.as_deref().unwrap()).unwrap()
}

/// Asserts that the prefix-grant minter, selected for `credentials = "presigned_prefix"`,
/// grants host A exactly the prefix the STS minter's session policy names for host A
/// (`a/b/js-<hash>/`, the resource of its `Policy` field being that prefix and `*`), derived
/// from the minter's signing key, in the request's bucket, expiring at the injected now plus
/// the lifetime asked: an hour, and 15 minutes after the clock has moved by a second and a
/// half (cut down to the whole second). The credential is the grant's. It catches a minter
/// whose prefix differs from the STS one and an expiry read from anything but the clock.
#[test]
fn a_grant_covers_the_prefix_the_sts_minter_names_until_now_plus_the_lifetime() {
    let rig = Rig::new();
    let minter = rig.presigned_minter();
    let ask = request("a/b", "host-a");
    let credential = minter.mint(&ask).unwrap();
    let grant = grant_of(&credential);
    assert_eq!(grant.prefix(), ask.host_prefix().unwrap().as_str());
    assert!(grant.prefix().starts_with("a/b/js-"), "{}", grant.prefix());
    assert_eq!(grant.bucket(), BUCKET);
    assert!(grant.signing_key_id().starts_with("FAKEKEY"));
    assert_eq!(grant.expires_at(), at(START + 3600));
    assert_eq!(credential.expires_at, at(START + 3600));

    rig.minter().mint(&ask).unwrap();
    let fields = form(&rig.sent()[0]);
    let policy = &fields.iter().find(|(name, _)| name == "Policy").unwrap().1;
    let resource = format!("\"Resource\":\"arn:aws:s3:::{BUCKET}/{}*\"", grant.prefix());
    assert!(policy.contains(&resource), "{policy}");

    rig.clock.advance(Duration::from_millis(1500));
    let short = MintRequest {
        ttl: Duration::from_secs(15 * 60),
        ..ask
    };
    assert_eq!(
        grant_of(&minter.mint(&short).unwrap()).expires_at(),
        at(START + 1 + 900)
    );
}

/// Asserts that hosts A and B get grants of different prefixes, neither a prefix of the other,
/// under the same log prefix, and that the same host gets the same prefix again. It catches a
/// grant scoped to the log prefix, which would let one host write the other's jobs.
#[test]
fn two_hosts_get_disjoint_prefixes() {
    let rig = Rig::new();
    let minter = rig.presigned_minter();
    let a = grant_of(&minter.mint(&request("a/b", "host-a")).unwrap());
    let b = grant_of(&minter.mint(&request("a/b", "host-b")).unwrap());
    let again = grant_of(&minter.mint(&request("a/b", "host-a")).unwrap());
    assert_ne!(a.prefix(), b.prefix());
    assert!(!a.prefix().starts_with(b.prefix()) && !b.prefix().starts_with(a.prefix()));
    assert_eq!(again.prefix(), a.prefix());
}

/// Asserts the refusals: a log prefix with no room for a host's segment is `Refused` by every
/// minter (the prefix-grant, the STS and the fake one) before any call; a lifetime past what
/// the clock holds, and a signing key whose id a grant cannot carry, are `Refused` by the
/// prefix-grant minter. Its `Debug` names the key id and never the secret. It catches a grant
/// built over a prefix the layout refuses, and a secret printed.
#[test]
fn a_mint_no_grant_can_be_built_for_is_refused() {
    let rig = Rig::new();
    let crowded = MintRequest {
        prefix: LogPrefix::new(&"p".repeat(500)).unwrap(),
        ..request("a/b", "host-a")
    };
    let too_long = Err(MintError::Refused(Refusal::Answer(
        "the host's prefix is longer than the object layout allows",
    )));
    let fake = FakeMinter::new(rig.fake.clone());
    assert_eq!(rig.presigned_minter().mint(&crowded), too_long);
    assert_eq!(rig.minter().mint(&crowded), too_long);
    assert_eq!(fake.mint(&crowded), too_long);
    assert!(rig.sent().is_empty());

    let no_grant = Err(MintError::Refused(Refusal::Answer(
        "no grant can be built for the expiry or the signing key",
    )));
    let forever = MintRequest {
        ttl: Duration::MAX,
        ..request("a/b", "host-a")
    };
    assert_eq!(rig.presigned_minter().mint(&forever), no_grant);
    let odd_key = Credential {
        access_key_id: "odd key".to_owned(),
        secret_access_key: "odd-secret".to_owned(),
        session_token: None,
        expires_at: at(START),
    };
    let odd = PresignedPrefixMinter::new(rig.clock.clone(), odd_key);
    assert_eq!(odd.mint(&request("a/b", "host-a")), no_grant);
    let shown = format!("{odd:?}");
    assert!(
        shown.contains("odd key") && !shown.contains("odd-secret"),
        "{shown}"
    );
}

/// Asserts the whole path with the fake store standing for a store with no STS: a credential
/// minted by the prefix-grant minter for host A, given to the signer, presigns the puts of a
/// job's chunk and final under the job's prefix below host A's prefix; the store accepts them;
/// and a reader with a read key under the log prefix (as `GetLogs` reads) lists exactly those
/// two objects under the job's prefix and reads back their bytes. A put under host B's prefix
/// is refused by the signer before any request. It catches a grant, a signer and a layout that
/// disagree on where a job's log goes.
#[test]
fn a_jobs_log_written_with_a_grant_lands_under_its_hosts_prefix_and_reads_back() {
    let rig = Rig::new();
    let ask = request("a/b", "host-a");
    let credential = rig.presigned_minter().mint(&ask).unwrap();
    let endpoint = Endpoint::parse(URL, REGION, BUCKET, true).unwrap();
    let clock = ManualClock::new(at(START));
    let signer = PrefixSigner::new(endpoint, credential, clock).unwrap();
    let host = ask.host_prefix().unwrap();
    let job = JobPrefix::new(host.as_str().trim_end_matches('/'), "job-1").unwrap();
    let chunk = job.chunk_key(Stream::Stdout, 0);
    let last = job.final_key(Stream::Stdout);
    let none = Metadata::new();
    for (key, bytes) in [(&chunk, &b"hello "[..]), (&last, &b"hello world"[..])] {
        let put = signer.put(key, bytes.to_vec(), &none).unwrap();
        assert_eq!(rig.fake.send(put).unwrap().status, 200);
    }
    let sibling = request("a/b", "host-b").host_prefix().unwrap();
    let theirs = format!("{}job-1/stdout.log", sibling.as_str());
    assert_eq!(
        signer.put(&theirs, b"x".to_vec(), &none),
        Err(GrantError::OutsidePrefix)
    );

    let reader = rig.client(rig.fake.issue(Policy {
        actions: vec![Action::Get, Action::List],
        prefix: "a/b/".to_owned(),
    }));
    let page = reader.list(job.as_str(), None, 10).unwrap();
    let keys: Vec<&str> = page
        .entries
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    assert_eq!(keys, [last.as_str(), chunk.as_str()]);
    assert!(chunk.starts_with(host.as_str()) && last.starts_with(host.as_str()));
    let read = reader.get_range(&last, 0, 64).unwrap().unwrap();
    assert_eq!(read.bytes, b"hello world");
}
