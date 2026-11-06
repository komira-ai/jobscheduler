//! How the server builds its [`LogGrants`] at start from the `[logs]` table: the production
//! minter is one STS `AssumeRole` call per mint ([`StsMinter`]) over the blocking HTTP
//! transport ([`UreqTransport`]), signed with the mint key at the server's own clock.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_lease::Clock as _;
use js_logmint::StsMinter;
use js_logstore::{Credential, UreqTransport};

use super::{LogGrants, Scope, SinkTemplate};
use crate::adapt::SharedClock;
use crate::file::{LogsConfig, Minting, ObjectStoreLogs};

/// The longest one `AssumeRole` call may take, connect to answer. A call that takes longer is
/// a failed mint ([`js_logmint::MintError::Unavailable`]), retried with the backoff.
pub(crate) const MINT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a long-lived key's expiry is set to: the mint key is the operator's and has none, and
/// the minter signs with it without reading one.
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
    let Minting::Sts {
        endpoint,
        role_arn,
        key,
    } = &sink.minting;
    let key = Credential {
        access_key_id: key.access_key_id().to_owned(),
        secret_access_key: key.secret_access_key().to_owned(),
        session_token: None,
        expires_at: UNIX_EPOCH + NEVER,
    };
    let minter = StsMinter::new(
        UreqTransport::new(MINT_TIMEOUT),
        StoreClock(clock.clone()),
        endpoint.clone(),
        role_arn.clone(),
        key,
    );
    let template = SinkTemplate {
        endpoint: sink.endpoint.clone(),
        prefix: sink.prefix.clone(),
    };
    let scope = Scope {
        bucket: sink.bucket.clone(),
        prefix: sink.prefix.clone(),
        ttl: sink.credential_ttl,
    };
    LogGrants::new(template, scope, Arc::new(minter), clock.clone())
}
