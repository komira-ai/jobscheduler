//! The fake's STS: `AssumeRole`, and the session policies it reads.
//!
//! A call is a `POST` to the path `/` whose body is a form (`application/x-www-form-urlencoded`),
//! signed for the service `sts` in the fake's region. The fake checks, in this order, after the
//! scripted failures of [`FakeStore::fail_next`]:
//!
//! 1. the signature, as for any request (403 `InvalidAccessKeyId`, `ExpiredToken`,
//!    `SignatureDoesNotMatch`; 400 `InvalidURI`);
//! 2. the key is one of [`FakeStore::minting_key`] (403 `AccessDenied`);
//! 3. the body is a form (400 `MalformedQueryString`) whose `Action` is `AssumeRole` (400
//!    `InvalidAction`);
//! 4. `Version` is given, `RoleArn` begins with `arn:`, `RoleSessionName` is 2 to 64 bytes of
//!    `[A-Za-z0-9+=,.@_-]` and `DurationSeconds` is from [`MIN_DURATION_SECS`] to
//!    [`MAX_DURATION_SECS`] (400 `ValidationError`);
//! 5. `Policy` is a session policy [`Policy::from_session_policy`] reads (400
//!    `MalformedPolicyDocument`).
//!
//! The answer is a temporary credential whose policy is the one read, valid for
//! `DurationSeconds` from the fake's clock (or for what [`FakeStore::answer_sts_with_lifetime`]
//! says), in the XML of an `AssumeRole` answer.

use std::time::Duration;

use serde_json::Value;

use super::{Action, FakeStore, Policy, State, answer, percent_decode, rfc3339, split_url, xml};
use crate::credential::Credential;
use crate::http::{HttpRequest, HttpResponse, Method};

/// The shortest `DurationSeconds` the fake's `AssumeRole` accepts, as STS: 15 minutes.
pub const MIN_DURATION_SECS: u64 = 900;

/// The longest `DurationSeconds` the fake's `AssumeRole` accepts, as STS: 12 hours.
pub const MAX_DURATION_SECS: u64 = 43_200;

/// The content type of an `AssumeRole` request's body.
const FORM: &str = "application/x-www-form-urlencoded";

/// What the resource of an object begins with, before the bucket's name.
const S3_ARN: &str = "arn:aws:s3:::";

/// The bytes a session name may hold besides letters and digits.
const SESSION_NAME_EXTRA: &[u8] = b"+=,.@_-";

/// Whether `request` is an STS call: a `POST` to the path `/`.
pub(super) fn is_sts(request: &HttpRequest) -> bool {
    request.method == Method::Post && split_url(&request.url).is_some_and(|(path, _)| path == "/")
}

impl Policy {
    /// The policy the session policy document `text` grants on the objects of `bucket`, or
    /// `None` when the text is not one the fake reads exactly: a JSON object with exactly a
    /// `Version` string and a `Statement` array of exactly one object, which has exactly
    /// `Effect` (`"Allow"`), `Action` (an array of at least one of `s3:PutObject`,
    /// `s3:DeleteObject`, `s3:GetObject` and `s3:ListBucket`) and `Resource`
    /// (`arn:aws:s3:::BUCKET/` + a prefix + `*`, the trailing `*` being its only `*`, `?` or
    /// `$`). Another key (`NotResource`, `Condition`, ...), another bucket or another wildcard is
    /// `None`: the fake grants nothing it cannot read exactly.
    pub fn from_session_policy(text: &str, bucket: &str) -> Option<Self> {
        let document: Value = serde_json::from_str(text).ok()?;
        let document = exactly(&document, &["Statement", "Version"])?;
        document["Version"].as_str()?;
        let statement = match document["Statement"].as_array()?.as_slice() {
            [only] => exactly(only, &["Action", "Effect", "Resource"])?,
            _ => return None,
        };
        let allow = statement["Effect"].as_str()? == "Allow";
        let actions = statement["Action"]
            .as_array()?
            .iter()
            .map(|action| action_of(action.as_str()?))
            .collect::<Option<Vec<Action>>>()?;
        let prefix = statement["Resource"]
            .as_str()?
            .strip_prefix(S3_ARN)?
            .strip_prefix(bucket)?
            .strip_prefix('/')?
            .strip_suffix('*')?;
        let plain = allow && !actions.is_empty() && !prefix.contains(['*', '?', '$']);
        plain.then(|| Self {
            actions,
            prefix: prefix.to_owned(),
        })
    }
}

impl FakeStore {
    /// Serves the STS call `request`: the answer, or the refusal.
    pub(super) fn assume_role(&self, state: &mut State, request: &HttpRequest) -> HttpResponse {
        match self.assumed(state, request) {
            Ok(answer) | Err(answer) => answer,
        }
    }

    /// The answer of an `AssumeRole` call the fake accepts, or the refusal.
    fn assumed(
        &self,
        state: &mut State,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpResponse> {
        if !self.verify(state, request, "sts")?.issued.may_assume {
            return Err(sts_error(403, "AccessDenied"));
        }
        let form = request
            .header("content-type")
            .filter(|kind| *kind == FORM)
            .and_then(|_| form_pairs(&request.body))
            .ok_or_else(|| sts_error(400, "MalformedQueryString"))?;
        let value = |name: &str| {
            form.iter()
                .find(|(have, _)| have == name)
                .map(|(_, value)| value.as_str())
        };
        if value("Action") != Some("AssumeRole") {
            return Err(sts_error(400, "InvalidAction"));
        }
        let checked = (
            value("Version").is_some_and(|version| !version.is_empty()),
            value("RoleArn").is_some_and(|arn| arn.starts_with("arn:")),
            value("RoleSessionName").is_some_and(session_name_ok),
            value("DurationSeconds")
                .and_then(|text| text.parse::<u64>().ok())
                .filter(|seconds| (MIN_DURATION_SECS..=MAX_DURATION_SECS).contains(seconds)),
        );
        let (true, true, true, Some(seconds)) = checked else {
            return Err(sts_error(400, "ValidationError"));
        };
        let bucket = self.shared.endpoint.bucket();
        let policy = value("Policy")
            .and_then(|text| Policy::from_session_policy(text, bucket))
            .ok_or_else(|| sts_error(400, "MalformedPolicyDocument"))?;
        let lifetime = state.sts_lifetime.unwrap_or(Duration::from_secs(seconds));
        let credential = self.issue_in(state, policy, lifetime, false);
        let body = assume_role_answer(&credential).into_bytes();
        Ok(answer(200, vec![xml()], body))
    }
}

/// `value` when it is an object with exactly the keys `keys`.
fn exactly<'v>(value: &'v Value, keys: &[&str]) -> Option<&'v Value> {
    let map = value.as_object()?;
    let exact = map.len() == keys.len() && keys.iter().all(|key| map.contains_key(*key));
    exact.then_some(value)
}

/// The action a policy names `name`.
fn action_of(name: &str) -> Option<Action> {
    match name {
        "s3:PutObject" => Some(Action::Put),
        "s3:DeleteObject" => Some(Action::Delete),
        "s3:GetObject" => Some(Action::Get),
        "s3:ListBucket" => Some(Action::List),
        _ => None,
    }
}

/// Whether `name` is a session name STS accepts.
fn session_name_ok(name: &str) -> bool {
    (2..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || SESSION_NAME_EXTRA.contains(&b))
}

/// The decoded names and values of a form body; `None` when it is not UTF-8, a pair has no
/// `=` or an escape is bad.
fn form_pairs(body: &[u8]) -> Option<Vec<(String, String)>> {
    let text = std::str::from_utf8(body).ok()?;
    text.split('&')
        .map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((percent_decode(name)?, percent_decode(value)?))
        })
        .collect()
}

/// The XML answer of an `AssumeRole` call that issued `credential`.
fn assume_role_answer(credential: &Credential) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{}</AccessKeyId><SecretAccessKey>{}</SecretAccessKey><SessionToken>{}</SessionToken><Expiration>{}</Expiration></Credentials></AssumeRoleResult><ResponseMetadata><RequestId>fake</RequestId></ResponseMetadata></AssumeRoleResponse>",
        credential.access_key_id,
        credential.secret_access_key,
        credential.session_token.as_deref().unwrap_or_default(),
        rfc3339(credential.expires_at),
    )
}

/// An STS error answer.
fn sts_error(status: u16, code: &str) -> HttpResponse {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code><Message>refused by the fake STS</Message></Error><RequestId>fake</RequestId></ErrorResponse>"
    );
    answer(status, vec![xml()], body.into_bytes())
}
