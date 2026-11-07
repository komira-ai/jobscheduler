//! [`TestStore`]: the S3-compatible object store, with an STS endpoint, that the live-log tests
//! of js-logstore, js-agent and js-lease-server write to and read from (CI's `test` job runs one
//! in a service container, pinned by digest).
//!
//! The store is the one [`URL_VAR`] names, `http://ACCESS_KEY_ID:SECRET@HOST:PORT/BUCKET`, in
//! the region [`REGION_VAR`] names: settings of the test harness, not of the product. The key is
//! the store's root key, which may create objects anywhere in the bucket and call `AssumeRole`;
//! the bucket exists before the tests run. A test that needs the store is `#[ignore]`d with a
//! reason, so the default `cargo test` never reaches for it; one that is run without the
//! variables fails, and does not skip: [`TestStore::from_env`] panics with a message that says
//! how to set them.
//!
//! Nothing here talks to the store: it says where the store is, and [`unique`] names what a test
//! puts in it, so that two tests, in one process or in two, never share a key, a host or a job.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

/// The variable that names the store, its root key and its bucket:
/// `http://ACCESS_KEY_ID:SECRET@HOST:PORT/BUCKET`.
pub const URL_VAR: &str = "JS_TEST_S3_URL";

/// The variable that names the region the store checks every signature against.
pub const REGION_VAR: &str = "JS_TEST_S3_REGION";

/// Where the test store is, and the root key that may do anything in its bucket.
#[derive(Clone, PartialEq, Eq)]
pub struct TestStore {
    /// The store's URL, `http://HOST:PORT`, with no key, path or trailing `/`. It is the STS
    /// endpoint too.
    pub base_url: String,
    /// The region every request is signed for.
    pub region: String,
    /// The bucket the tests use.
    pub bucket: String,
    /// The root key's id.
    pub access_key_id: String,
    /// The root key's secret. Secret: no `Debug` output holds it.
    pub secret_access_key: String,
}

impl fmt::Debug for TestStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestStore")
            .field("base_url", &self.base_url)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// Why the variables do not name a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoStore {
    /// [`URL_VAR`] or [`REGION_VAR`] is not set.
    Unset,
    /// [`URL_VAR`] is not `http://ACCESS_KEY_ID:SECRET@HOST:PORT/BUCKET`; the reason names the
    /// part, never the value.
    Malformed(&'static str),
}

impl fmt::Display for NoStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self {
            Self::Unset => format!("{URL_VAR} or {REGION_VAR} is not set"),
            Self::Malformed(part) => format!("{URL_VAR} is malformed: {part}"),
        };
        write!(
            f,
            "{why}: this test needs an S3-compatible store with STS. Start one, create a bucket, \
             and set {URL_VAR}=http://ACCESS_KEY_ID:SECRET@HOST:PORT/BUCKET (the root key) and \
             {REGION_VAR}=REGION (CI's service container sets both)"
        )
    }
}

impl TestStore {
    /// The store `url` (the value of [`URL_VAR`]) and `region` (that of [`REGION_VAR`]) name.
    ///
    /// # Errors
    ///
    /// [`NoStore::Unset`] when either is `None` or empty; [`NoStore::Malformed`] when `url` is not
    /// `http://` or `https://`, then `ACCESS_KEY_ID:SECRET@` (the last `@` ends the key, and the
    /// first `:` in it ends the id), then the authority, then `/` and the bucket with no other `/`.
    pub fn parse(url: Option<&str>, region: Option<&str>) -> Result<Self, NoStore> {
        let (Some(url), Some(region)) = (url, region) else {
            return Err(NoStore::Unset);
        };
        if url.is_empty() || region.is_empty() {
            return Err(NoStore::Unset);
        }
        let (scheme, rest) = url
            .split_once("://")
            .filter(|(scheme, _)| matches!(*scheme, "http" | "https"))
            .ok_or(NoStore::Malformed("the scheme is not http:// or https://"))?;
        let (key, place) = rest
            .rsplit_once('@')
            .ok_or(NoStore::Malformed("there is no ACCESS_KEY_ID:SECRET@"))?;
        let (access_key_id, secret_access_key) = key
            .split_once(':')
            .filter(|(id, secret)| !id.is_empty() && !secret.is_empty())
            .ok_or(NoStore::Malformed("the key is not ACCESS_KEY_ID:SECRET"))?;
        let (authority, bucket) = place
            .split_once('/')
            .filter(|(authority, bucket)| {
                !authority.is_empty() && !bucket.is_empty() && !bucket.contains('/')
            })
            .ok_or(NoStore::Malformed("the place is not HOST:PORT/BUCKET"))?;
        Ok(Self {
            base_url: format!("{scheme}://{authority}"),
            region: region.to_owned(),
            bucket: bucket.to_owned(),
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
        })
    }

    /// The store [`TestStore::parse`] reads from `url` and `region`.
    ///
    /// # Panics
    ///
    /// When they do not name one: the message names both variables and says how to set them.
    #[must_use]
    pub fn or_fail(url: Option<&str>, region: Option<&str>) -> Self {
        match Self::parse(url, region) {
            Ok(store) => store,
            Err(why) => panic!("{why}"),
        }
    }

    /// The store [`URL_VAR`] and [`REGION_VAR`] name.
    ///
    /// # Panics
    ///
    /// When they do not name one (see [`TestStore::or_fail`]).
    #[must_use]
    pub fn from_env() -> Self {
        let url = std::env::var(URL_VAR).ok();
        let region = std::env::var(REGION_VAR).ok();
        Self::or_fail(url.as_deref(), region.as_deref())
    }
}

/// A name no other call makes, in this process or another: `label`, the process id, the time in
/// nanoseconds and a counter, joined by `-`. It is lower-case ASCII letters, digits and `-` when
/// `label` is, so it may be a job's id, an agent's id or a segment of a key.
#[must_use]
pub fn unique(label: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = UNIX_EPOCH.elapsed().map_or(0, |since| since.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{label}-{}-{nanos}-{count}", std::process::id())
}
