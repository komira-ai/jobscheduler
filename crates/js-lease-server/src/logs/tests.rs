//! The grants on their own: the backoff and its cap, a credential the wire cannot carry, the
//! bound on mints at once, a mint whose host left meanwhile, a queue outside a runtime, and the
//! grants a `[logs]` table builds. The heartbeat and the start are tested through the route
//! (`agent_api_log_tests.rs`).

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_lease::{ManualClock, Timestamp};
use js_logmint::fake::FakeMinter;
use js_logmint::{RoleArn, StsEndpoint};
use js_logstore::fake::FakeStore;
use js_logstore::{Clock as _, Endpoint};

use super::*;
use crate::agent_api::job_rig::{
    LOG_BUCKET, LOG_ENDPOINT, LOG_PREFIX, LOG_REGION, LOG_TTL, START_MS, grants_over,
};
use crate::file::{AccessKey, ObjectStoreLogs};
use crate::util::blocking;

/// A minter that answers every mint with one credential.
struct Answering(Mutex<Credential>);

impl Minter for Answering {
    fn mint(&self, _: &MintRequest) -> Result<Credential, MintError> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// A good credential that expires at `expires_at`.
fn credential(expires_at: SystemTime) -> Credential {
    Credential {
        access_key_id: "AKIDHOST".to_owned(),
        secret_access_key: "host-secret".to_owned(),
        session_token: Some("host-token".to_owned()),
        expires_at,
    }
}

/// The server's clock, at [`START_MS`].
fn clock() -> SharedClock {
    SharedClock::new(ManualClock::new(Timestamp::from_millis(START_MS)))
}

/// The sink's store.
fn endpoint() -> Endpoint {
    Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap()
}

/// Grants over js-logmint's fake minter, on [`clock`]; the minter too.
fn faked() -> (Arc<LogGrants>, Arc<FakeMinter>) {
    let clock = clock();
    let store = FakeStore::new(endpoint(), Arc::new(StoreClock(clock.clone())));
    let minter = Arc::new(FakeMinter::new(store));
    let grants = grants_over(endpoint(), Arc::clone(&minter) as Arc<dyn Minter>, &clock);
    (grants, minter)
}

/// Waits on the pool for blocking calls until no mint of `grants` runs.
async fn settle(grants: &Arc<LogGrants>) {
    let grants = Arc::clone(grants);
    blocking(move || grants.settle()).await;
}

/// The first retry waits five seconds and each later one twice the one before, up to five
/// minutes, however many failures there were. It catches a backoff that does not double, one
/// that is not capped, and one that overflows on a long outage.
#[test]
fn the_backoff_doubles_from_five_seconds_to_its_cap_of_five_minutes() {
    let waits: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
    assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300]);
    assert_eq!(backoff(u32::MAX), MINT_RETRY_CAP);
    assert_eq!(backoff(1), MINT_RETRY_FIRST);
    assert_eq!(millis(Duration::MAX), u64::MAX);
}

/// A minted credential is held only when the wire can carry it to an agent: one that expires
/// before the epoch or at it, or whose key has a byte a request header cannot hold, is a
/// refused mint and nothing is held. It catches a grant that the agent would drop as
/// malformed being held and resent forever.
#[tokio::test]
async fn a_credential_the_wire_cannot_carry_is_a_failed_mint() {
    let later = UNIX_EPOCH + Duration::from_millis(START_MS) + LOG_TTL;
    let refused = MintError::Refused(Refusal::Answer(UNUSABLE));
    let early = UNIX_EPOCH - Duration::from_secs(1);
    let spaced = Credential {
        access_key_id: "AKID HOST".to_owned(),
        ..credential(later)
    };
    for bad in [credential(early), credential(UNIX_EPOCH), spaced] {
        assert_eq!(grant_of(bad), Err(refused.clone()));
    }
    let good = grant_of(credential(later)).unwrap();
    let expected = START_MS + LOG_TTL.as_millis() as u64;
    assert_eq!(good.expires_at_ms, expected);

    let minter = Arc::new(Answering(Mutex::new(credential(early))));
    let grants = grants_over(endpoint(), Arc::clone(&minter) as Arc<dyn Minter>, &clock());
    grants.prepare("host-a");
    settle(&grants).await;
    assert_eq!(grants.credential_for("host-a"), None);
    assert_eq!(grants.counters().failed, 1);
    *minter.0.lock().unwrap() = credential(later);
    grants.prepare("host-a");
    settle(&grants).await;
    assert_eq!(
        grants.counters().minted,
        0,
        "the backoff was not waited out"
    );
}

/// At most eight mints run at once: with the minter holding every mint, nine hosts asking
/// get eight mints started and the ninth waits; once they are let go the ninth is minted by a
/// worker that is already running. It catches a bound that is not kept and a host left
/// waiting once a worker is free.
#[tokio::test]
async fn no_more_than_eight_mints_run_at_once() {
    let (grants, minter) = faked();
    minter.block();
    let hosts: Vec<String> = (0..9).map(|n| format!("host-{n}")).collect();
    for host in &hosts {
        grants.prepare(host);
    }
    let held = Arc::clone(&minter);
    blocking(move || held.wait_for_held(MAX_MINTS_IN_FLIGHT)).await;
    assert_eq!(minter.requests().len(), MAX_MINTS_IN_FLIGHT);
    minter.release();
    settle(&grants).await;
    assert_eq!(minter.requests().len(), 9);
    for host in &hosts {
        assert!(grants.credential_for(host).is_some(), "{host}");
        assert_eq!(minter.calls(host), 1, "{host}");
    }
    assert_eq!(grants.counters().hosts_with_credential, 9);
}

/// A host forgotten while its mint runs is not given the credential when the mint ends: it
/// is dropped, and no renewal or delivery follows. It catches a departed host brought back by
/// its own late mint.
#[tokio::test]
async fn a_mint_that_ends_after_its_host_left_is_dropped() {
    let (grants, minter) = faked();
    minter.block();
    grants.prepare("host-a");
    let held = Arc::clone(&minter);
    blocking(move || held.wait_for_held(1)).await;
    grants.forget("host-a");
    minter.release();
    settle(&grants).await;
    assert_eq!(grants.credential_for("host-a"), None);
    assert_eq!(grants.counters(), LogCounters::default());
}

/// Outside a runtime a mint is queued and not started (no thread is spawned from a caller
/// that has no pool); the next call from inside one starts it. It catches a queue that is
/// lost, and a mint run on the caller's thread.
#[test]
fn a_mint_asked_outside_a_runtime_waits_for_the_next_one() {
    let (grants, minter) = faked();
    grants.prepare("host-a");
    assert_eq!(minter.calls("host-a"), 0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        grants.tick(&BTreeSet::from(["host-a".to_owned()]));
        settle(&grants).await;
    });
    assert_eq!(minter.calls("host-a"), 1);
    assert!(grants.credential_for("host-a").is_some());
}

/// `sink = "none"` builds no grants; an object-store table builds grants whose sinks name the
/// table's store, region, bucket, style and prefix, minting with the table's role and key
/// (no call is made here). It catches a sink built from anything but the table.
#[test]
fn a_logs_table_builds_the_grants_it_names() {
    let clock = clock();
    assert!(from_config(&crate::file::LogsConfig::None, &clock).is_none());
    let key = br#"{"access_key_id": "MINTKEY", "secret_access_key": "mint-secret"}"#;
    let key = AccessKey::parse(key).unwrap();
    let table = ObjectStoreLogs {
        bucket: Bucket::new(LOG_BUCKET).unwrap(),
        prefix: LogPrefix::new(LOG_PREFIX).unwrap(),
        endpoint: Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, false).unwrap(),
        read_key: key.clone(),
        minting: crate::file::Minting::Sts {
            endpoint: StsEndpoint::parse(LOG_ENDPOINT, LOG_REGION).unwrap(),
            role_arn: RoleArn::new("arn:aws:iam::example:role/js-logs").unwrap(),
            key,
        },
        credential_ttl: LOG_TTL,
        max_follow: 256,
    };
    let logs = crate::file::LogsConfig::ObjectStore(Box::new(table));
    let grants = from_config(&logs, &clock).unwrap();
    let sink = grants.sink_for("host-a", "build-42").unwrap();
    assert_eq!(
        sink,
        LogSink {
            endpoint: LOG_ENDPOINT.to_owned(),
            region: LOG_REGION.to_owned(),
            bucket: LOG_BUCKET.to_owned(),
            prefix: format!("a/b/{}/build-42/", js_logmint::host_segment("host-a")),
            path_style: false,
        }
    );
    assert_eq!(grants.scope.ttl, LOG_TTL);
    assert_eq!(grants.counters(), LogCounters::default());
    let now = StoreClock(clock).now();
    assert_eq!(now, UNIX_EPOCH + Duration::from_millis(START_MS));
}

/// A `presigned_prefix` table builds grants whose minter derives a prefix grant from the
/// table's signing key with no call: the credential minted for `host-a` is a grant of exactly
/// the prefix its jobs' sinks are under, in the table's bucket, expiring at the server's clock
/// plus `credential_ttl`; a job of `host-b` is under another prefix. It catches a server that
/// grants one prefix and places jobs under another.
#[test]
fn a_presigned_prefix_table_grants_each_host_the_prefix_of_its_jobs() {
    let clock = clock();
    let key = br#"{"access_key_id": "SIGNKEY", "secret_access_key": "signing-secret"}"#;
    let key = AccessKey::parse(key).unwrap();
    let table = ObjectStoreLogs {
        bucket: Bucket::new(LOG_BUCKET).unwrap(),
        prefix: LogPrefix::new(LOG_PREFIX).unwrap(),
        endpoint: Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap(),
        read_key: key.clone(),
        minting: crate::file::Minting::PresignedPrefix { key },
        credential_ttl: LOG_TTL,
        max_follow: 256,
    };
    let logs = crate::file::LogsConfig::ObjectStore(Box::new(table));
    let grants = from_config(&logs, &clock).unwrap();
    let request = MintRequest {
        bucket: grants.scope.bucket.clone(),
        prefix: grants.scope.prefix.clone(),
        host: "host-a".to_owned(),
        ttl: grants.scope.ttl,
    };
    let credential = grants.minter.mint(&request).unwrap();
    let grant =
        js_logstore::PrefixGrant::parse(credential.session_token.as_deref().unwrap()).unwrap();
    assert_eq!(grant.signing_key_id(), "SIGNKEY");
    assert_eq!(grant.bucket(), LOG_BUCKET);
    let expires = UNIX_EPOCH + Duration::from_millis(START_MS) + LOG_TTL;
    assert_eq!(grant.expires_at(), expires);
    let sink = grants.sink_for("host-a", "build-42").unwrap();
    assert_eq!(sink.prefix, format!("{}build-42/", grant.prefix()));
    let other = grants.sink_for("host-b", "build-43").unwrap();
    assert!(
        !other.prefix.starts_with(grant.prefix()),
        "{}",
        other.prefix
    );
}
