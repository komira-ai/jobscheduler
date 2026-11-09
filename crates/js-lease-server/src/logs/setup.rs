//! How the server builds its [`LogGrants`] at start from the `[logs]` table. With
//! `credentials = "sts"` the minter is one STS `AssumeRole` call per mint ([`StsMinter`]) over
//! the blocking HTTP transport ([`UreqTransport`]), signed with the mint key at the server's
//! own clock. With `credentials = "presigned_prefix"` it is a prefix grant per host derived
//! from the signing key at the server's own clock ([`PresignedPrefixMinter`], no call). Either
//! way a host's credential covers its own prefix, where each of its jobs is placed.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_logmint::{Minter, PresignedPrefixMinter, StsMinter};
use js_logstore::{Credential, UreqTransport};
use js_types::Clock as _;

use super::{LogGrants, Scope, SinkTemplate};
use crate::adapt::SharedClock;
use crate::file::{AccessKey, LogsConfig, Minting, ObjectStoreLogs};

/// The longest one `AssumeRole` call may take, connect to answer. A call that takes longer is
/// a failed mint ([`js_logmint::MintError::Unavailable`]), retried with the backoff.
pub(crate) const MINT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a long-lived key's expiry is set to: the mint and signing keys are the operator's and
/// have none, and the minters sign with them without reading one.
const NEVER: Duration = Duration::from_secs(1 << 40);

/// The server's clock as the object-store crates read time: the same instant, as a
/// [`SystemTime`]. The minter signs at it and the fake store of the tests issues at it, so a
/// credential's expiry and the server's renewal rule read one clock.
#[derive(Clone)]
pub(crate) struct StoreClock(pub(crate) SharedClock);

impl js_logstore::Clock for StoreClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0.now().millis())
    }
}

/// The host-credential side of `logs`: `None` for `sink = "none"`, which mints nothing and
/// sends no sink.
pub(crate) fn from_config(logs: &LogsConfig, clock: &SharedClock) -> Option<Arc<LogGrants>> {
    match logs {
        LogsConfig::None => None,
        LogsConfig::ObjectStore(sink) => Some(Arc::new(object_store(sink, clock))),
    }
}

/// The grants of an object-store sink, minting with its `[logs]` keys.
fn object_store(sink: &ObjectStoreLogs, clock: &SharedClock) -> LogGrants {
    let store_clock = StoreClock(clock.clone());
    let minter: Arc<dyn Minter> = match &sink.minting {
        Minting::Sts {
            endpoint,
            role_arn,
            key,
        } => {
            let minter = StsMinter::new(
                UreqTransport::new(MINT_TIMEOUT),
                store_clock,
                endpoint.clone(),
                role_arn.clone(),
                long_lived(key),
            );
            Arc::new(minter)
        }
        Minting::PresignedPrefix { key } => {
            let minter = PresignedPrefixMinter::new(store_clock, long_lived(key));
            Arc::new(minter)
        }
    };
    let template = SinkTemplate {
        endpoint: sink.endpoint.clone(),
        prefix: sink.prefix.clone(),
    };
    let scope = Scope {
        bucket: sink.bucket.clone(),
        prefix: sink.prefix.clone(),
        ttl: sink.credential_ttl,
    };
    LogGrants::new(template, scope, minter, clock.clone())
}

/// The operator's long-lived key `key` as a credential the minters sign with.
fn long_lived(key: &AccessKey) -> Credential {
    Credential {
        access_key_id: key.access_key_id().to_owned(),
        secret_access_key: key.secret_access_key().to_owned(),
        session_token: None,
        expires_at: UNIX_EPOCH + NEVER,
    }
}
