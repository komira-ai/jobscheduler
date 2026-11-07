//! What the tests share: the test store, clients of it signed with its root key or with a
//! credential the store's STS minted, and a log prefix of the test's own.

use std::time::{Duration, SystemTime};

use js_logmint::{Bucket, LogPrefix, MintRequest, Minter, RoleArn, StsEndpoint, StsMinter};
use js_logstore::{Clock, Credential, Endpoint, JobPrefix, Store, SystemClock, UreqTransport};
use js_testkit::s3::{TestStore, unique};

/// How long one request may take: the store is on this machine.
const CALL_LIMIT: Duration = Duration::from_secs(30);

/// The role every mint asks for. The test store takes any: what narrows the credential is
/// the session policy, which is what the tests check.
pub const ROLE: &str = "arn:aws:iam::000000000000:role/js-live-logs";

/// The shortest lifetime the test store's STS issues.
pub const SHORTEST_TTL: Duration = Duration::from_secs(900);

/// A long-lived key's expiry: the root key has none.
const NEVER: Duration = Duration::from_secs(1 << 40);

/// A client of the test store, signing at the system clock.
pub type Client = Store<UreqTransport, SystemClock>;

/// The test store and a log prefix no other test uses.
pub struct Rig {
    pub test: TestStore,
    pub endpoint: Endpoint,
    /// The log prefix `L` every key of the test is under (with its trailing `/`).
    pub prefix: LogPrefix,
}

impl Rig {
    /// The store the variables name, and a new log prefix.
    pub fn new() -> Self {
        let test = TestStore::from_env();
        let endpoint = Self::endpoint_in(&test, &test.region);
        let prefix = LogPrefix::new(&unique("logstore")).expect("a log prefix");
        Self {
            test,
            endpoint,
            prefix,
        }
    }

    /// The store's endpoint, path style, signed for `region`.
    pub fn endpoint_in(test: &TestStore, region: &str) -> Endpoint {
        Endpoint::parse(&test.base_url, region, &test.bucket, true).expect("an endpoint")
    }

    /// The store's root key.
    pub fn root(&self) -> Credential {
        Credential {
            access_key_id: self.test.access_key_id.clone(),
            secret_access_key: self.test.secret_access_key.clone(),
            session_token: None,
            expires_at: SystemTime::UNIX_EPOCH + NEVER,
        }
    }

    /// A client signing with `credential`.
    pub fn client(&self, credential: Credential) -> Client {
        self.client_at(credential, SystemClock)
    }

    /// A client signing with `credential` at the times `clock` reads.
    pub fn client_at<C: Clock>(&self, credential: Credential, clock: C) -> Store<UreqTransport, C> {
        Store::new(
            UreqTransport::new(CALL_LIMIT),
            clock,
            self.endpoint.clone(),
            credential,
        )
    }

    /// A minter that calls the store's STS signed for `region` with the root key.
    pub fn minter_in(&self, region: &str) -> StsMinter<UreqTransport, SystemClock> {
        let sts = StsEndpoint::parse(&self.test.base_url, region).expect("an STS endpoint");
        StsMinter::new(
            UreqTransport::new(CALL_LIMIT),
            SystemClock,
            sts,
            RoleArn::new(ROLE).expect("a role"),
            self.root(),
        )
    }

    /// What a mint for `host` asks for: the test's bucket and log prefix, for `ttl`.
    pub fn request(&self, host: &str, ttl: Duration) -> MintRequest {
        MintRequest {
            bucket: Bucket::new(&self.test.bucket).expect("a bucket"),
            prefix: self.prefix.clone(),
            host: host.to_owned(),
            ttl,
        }
    }

    /// A credential the store's STS mints for `host`, with the shortest lifetime it issues,
    /// and the host's prefix it covers.
    pub fn mint(&self, host: &str) -> (Credential, LogPrefix) {
        let request = self.request(host, SHORTEST_TTL);
        let credential = self
            .minter_in(&self.test.region)
            .mint(&request)
            .expect("the store's STS mints");
        (credential, request.host_prefix().expect("a host prefix"))
    }

    /// The prefix of the job `job_id` under the log prefix `under` (a host's, or the test's).
    pub fn job(&self, under: &LogPrefix, job_id: &str) -> JobPrefix {
        JobPrefix::new(under.as_str().trim_end_matches('/'), job_id).expect("a job prefix")
    }
}

/// The bytes `0, 1, ..., 255, 0, 1, ...`, `len` of them: every byte value, so a body the
/// signer hashed other than as sent shows.
pub fn every_byte(len: usize) -> Vec<u8> {
    (0..=255u8).cycle().take(len).collect()
}
