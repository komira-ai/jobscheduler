//! The fake store's prefix grants ([`crate::presigned`]): the signing keys grants are derived
//! from, the credential a grant signs with, and the check of a presigned (query-form) request.

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use super::{
    Action, FakeStore, Issued, Kind, MINTING_KEY_TTL, Policy, State, Verified, error, parse_stamp,
    split_url,
};
use crate::credential::Credential;
use crate::http::{HttpRequest, HttpResponse};
use crate::presigned::{PrefixGrant, grant_key_id};
use crate::sigv4::{MAX_PRESIGN_SECS, Scope, Unsigned, presign};

/// The query parameter that makes a request presigned.
const SIGNATURE: &str = "X-Amz-Signature";

/// What the name of every signature parameter of a presigned request begins with.
const AUTH_PARAM: &str = "X-Amz-";

/// The path and the decoded query of `request` when it is presigned: its query has an
/// `X-Amz-Signature`.
pub(super) fn presigned_url(request: &HttpRequest) -> Option<(&str, Vec<(String, String)>)> {
    split_url(&request.url).filter(|(_, query)| query.iter().any(|(name, _)| name == SIGNATURE))
}

/// The signature parameters of a presigned request that the fake reads before it checks the
/// signature.
struct Auth<'q> {
    key_id: &'q str,
    token: Option<&'q str>,
    at: SystemTime,
    expires: u64,
    signed_names: Vec<&'q str>,
}

/// The parameters of `params`, or `None` when one is missing or malformed, or `X-Amz-Expires`
/// is outside 1 to [`MAX_PRESIGN_SECS`] (S3 refuses a longer one).
fn auth(params: &[(String, String)]) -> Option<Auth<'_>> {
    let get = |name: &str| {
        params
            .iter()
            .find(|(have, _)| have == name)
            .map(|(_, value)| value.as_str())
    };
    let (key_id, _) = get("X-Amz-Credential")?.split_once('/')?;
    Some(Auth {
        key_id,
        token: get("X-Amz-Security-Token"),
        at: parse_stamp(get("X-Amz-Date")?)?,
        expires: get("X-Amz-Expires")?
            .parse()
            .ok()
            .filter(|secs| (1..=MAX_PRESIGN_SECS).contains(secs))?,
        signed_names: get("X-Amz-SignedHeaders")?.split(';').collect(),
    })
}

impl FakeStore {
    /// A new long-lived key (no session token) that prefix grants are derived from, until
    /// [`MINTING_KEY_TTL`] after the fake's clock now. It signs no request itself (it may do
    /// nothing); a grant derived from it may put and delete under its prefix until its expiry.
    pub fn prefix_signing_key(&self) -> Credential {
        let none = Policy {
            actions: Vec::new(),
            prefix: String::new(),
        };
        self.issue_in(&mut self.lock(), none, MINTING_KEY_TTL, Kind::Signing)
    }

    /// The credential that signs as `key_id` with the session token `token`: one the fake
    /// issued, or the credential of a prefix grant whose token is `token`, derived from a
    /// signing key the fake issued, for this bucket, with `key_id` the id the token gives. A
    /// grant may put and delete under its prefix and nothing else.
    pub(super) fn issued<'s>(
        &self,
        state: &'s State,
        key_id: &str,
        token: Option<&str>,
    ) -> Option<Cow<'s, Issued>> {
        if let Some(issued) = state.keys.get(key_id) {
            return Some(Cow::Borrowed(issued));
        }
        let token = token?;
        let grant = PrefixGrant::parse(token).ok()?;
        let signing = state
            .keys
            .get(grant.signing_key_id())
            .filter(|signing| signing.signs_grants)?;
        let fits = grant_key_id(token) == key_id && grant.bucket() == self.shared.endpoint.bucket();
        fits.then(|| {
            Cow::Owned(Issued {
                credential: grant.credential(&signing.credential.secret_access_key),
                policy: Policy {
                    actions: vec![Action::Put, Action::Delete],
                    prefix: grant.prefix().to_owned(),
                },
                may_assume: false,
                signs_grants: false,
            })
        })
    }

    /// The credential that presigned `request` (whose URL has `path` and the decoded `query`)
    /// for `service`, when it is one [`FakeStore::issued`] knows, unexpired, the request is
    /// within its own `X-Amz-Expires`, and the signature is the one the fake computes;
    /// otherwise the refusal. What it verifies carries the query without the signature's
    /// parameters.
    pub(super) fn verify_presigned<'s, 'r>(
        &self,
        state: &'s State,
        request: &'r HttpRequest,
        service: &str,
        path: &'r str,
        query: Vec<(String, String)>,
    ) -> Result<Verified<'s, 'r>, HttpResponse> {
        let mismatch = || error(403, "SignatureDoesNotMatch");
        let (params, own): (Vec<_>, Vec<_>) = query
            .into_iter()
            .partition(|(name, _)| name.starts_with(AUTH_PARAM));
        let auth = auth(&params).ok_or_else(mismatch)?;
        let issued = self
            .issued(state, auth.key_id, auth.token)
            .ok_or_else(|| error(403, "InvalidAccessKeyId"))?;
        let now = self.shared.clock.now();
        if issued.credential.is_expired(now) {
            return Err(error(403, "ExpiredToken"));
        }
        if now >= auth.at + Duration::from_secs(auth.expires) {
            return Err(error(403, "AccessDenied"));
        }
        let headers: Vec<(String, String)> = request
            .headers
            .iter()
            .filter(|(name, _)| name != "host")
            .cloned()
            .collect();
        let sign_also: Vec<&str> = auth
            .signed_names
            .iter()
            .copied()
            .filter(|name| !name.starts_with("x-amz-") && *name != "host")
            .collect();
        let unsigned = Unsigned {
            method: request.method.as_str(),
            host: request.header("host").unwrap_or_default(),
            path,
            query: &own,
            headers: &headers,
            sign_also: &sign_also,
            body: &[],
        };
        let scope = Scope {
            region: self.shared.endpoint.region(),
            service,
        };
        let expected = presign(&unsigned, &issued.credential, &scope, auth.at, auth.expires);
        let genuine = expected.len() == params.len() && expected.iter().all(|p| params.contains(p));
        match genuine {
            true => Ok(Verified {
                issued,
                path,
                query: own,
            }),
            false => Err(mismatch()),
        }
    }
}
