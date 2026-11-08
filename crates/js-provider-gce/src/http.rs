//! The real client of the port: [`HttpGce`] makes each call of [`GceApi`] as one JSON
//! request over `https` (or one walk over the pages of one list) over a
//! [`js_logstore::Transport`], and reads the answer into the crate's own records.
//!
//! **Endpoints.** Compute Engine is [`COMPUTE_BASE`] and the Cloud Billing Catalog API
//! [`BILLING_BASE`]: the cloud's public endpoints. Every request carries the access token
//! of the credential ([`Credentials`]) as a bearer token.
//!
//! **Answers.** A success is read into the records of [`crate::model`] and
//! [`crate::launch`]; a field the documentation says is there and is not makes the call
//! [`ApiError::Unreadable`]. A failure is told by the reason of the cloud's error answer
//! and by nothing else; the cloud's message is never kept. HTTP 401 is always
//! [`ApiError::Denied`], HTTP 403 is denied unless the reason is a throttle or
//! `quotaExceeded` (a refusal), and HTTP 429 is a throttle. An error answer with no reason
//! is [`ApiError::Unreadable`], a transport that timed out is [`ApiError::Timeout`] and any
//! other transport failure is [`ApiError::Transport`].
//!
//! **Pages.** A list is read page by page under one call. A page that fails fails the
//! call, and a token the cloud repeats is [`ApiError::Incomplete`]: the walk never ends
//! short in silence.
//!
//! **Retries.** None. A call is made once and the caller decides.
//!
//! **Secrets.** No `Debug` output, error or log line of this module holds an access
//! token, a private key, an assertion or the boot payload of an instance.

use std::fmt;
use std::sync::Arc;

use js_logstore::sigv4::canonical_query;
use js_logstore::{Clock as WallClock, HttpRequest, Method, Transport, TransportError};
use serde_json::Value;

use crate::api::GceApi;
use crate::error::{ApiError, ErrorReason};
use crate::launch::{
    InsertRequest, Instance, InstanceRef, ListFilter, Operation, OperationRef, Page,
};
use crate::model::{
    AcceleratorTypeInfo, MachineTypeInfo, Project, QuotaMetric, Region, SkuPrice, Zone,
};

mod catalog_calls;
mod credential;
mod launch_calls;
mod skus;

pub use credential::{CredentialError, Credentials, KEY_FILE_VARIABLE, ServiceAccountKey};

use credential::TokenSource;

/// The base of the Compute Engine API.
pub const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";

/// The base of the Cloud Billing Catalog API.
pub const BILLING_BASE: &str = "https://cloudbilling.googleapis.com/v1";

/// The name of Compute Engine in the Cloud Billing Catalog API (its public service id).
pub const COMPUTE_ENGINE_SERVICE: &str = "services/6F81-5844-456A";

/// The content type of a request with a body.
const JSON: &str = "application/json";

/// The reason of a throttle that carries no reason of its own (HTTP 429).
const RATE_LIMIT: &str = "RATE_LIMIT_EXCEEDED";

/// The reason of a 401 that carries no reason of its own.
const UNAUTHENTICATED: &str = "UNAUTHENTICATED";

/// The reason of a 403 that carries no reason of its own.
const FORBIDDEN: &str = "forbidden";

/// The reason of an answer that says a limit of the project is used up (the standard error
/// reasons of Google's JSON APIs: `quotaExceeded`, domain `usageLimits`, at HTTP 403). At
/// a 403 it is a refusal, never a denial: the credential may make the call.
const QUOTA_EXCEEDED: &str = "quotaExceeded";

/// The client of the port over a transport. It reads the clock only to know when its
/// access token runs out.
pub struct HttpGce {
    tokens: TokenSource,
    transport: Arc<dyn Transport>,
    wall: Arc<dyn WallClock>,
}

impl fmt::Debug for HttpGce {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("HttpGce").finish_non_exhaustive()
    }
}

impl HttpGce {
    /// A client calling with `credentials` over `transport`. `wall` is the time of the
    /// expiry of an access token.
    pub fn new(
        credentials: Credentials,
        transport: Arc<dyn Transport>,
        wall: Arc<dyn WallClock>,
    ) -> Self {
        Self {
            tokens: TokenSource::new(credentials),
            transport,
            wall,
        }
    }

    /// One request of `method` to `url` with `query` (percent-encoded) and `body` as JSON:
    /// the JSON of the answer when it is a success.
    fn request(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let token = self.tokens.access_token(&*self.transport, &*self.wall)?;
        let mut headers = vec![("authorization".to_owned(), format!("Bearer {token}"))];
        headers.extend(body.map(|_| ("content-type".to_owned(), JSON.to_owned())));
        let request = HttpRequest {
            method,
            url: with_query(url, query),
            headers,
            body: body
                .map(|body| body.to_string().into_bytes())
                .unwrap_or_default(),
        };
        let answer = self.transport.send(request).map_err(transport_error)?;
        tracing::debug!(
            method = method.as_str(),
            status = answer.status,
            "the cloud answered"
        );
        match (200..300).contains(&answer.status) {
            true => serde_json::from_slice(&answer.body)
                .map_err(|_| unreadable("the answer is not JSON")),
            false => Err(refusal(answer.status, &answer.body)),
        }
    }

    /// A GET of `url` with `query`: the JSON of the answer.
    pub(super) fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        self.request(Method::Get, url, query, None)
    }
}

/// `url` with `query` appended, names and values percent-encoded.
fn with_query(url: &str, query: &[(&str, String)]) -> String {
    let pairs: Vec<(String, String)> = query
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    match pairs.is_empty() {
        true => url.to_owned(),
        false => format!("{url}?{}", canonical_query(&pairs)),
    }
}

/// Reads a list in pages: `fetch` takes the token of the page (none for the first) and
/// answers its rows and the token of the next page, if there is one.
///
/// # Errors
///
/// The error of the first page that fails, or [`ApiError::Incomplete`] when the cloud
/// answers the token it was just given.
pub(super) fn pages<T>(
    mut fetch: impl FnMut(Option<&str>) -> Result<(Vec<T>, Option<String>), ApiError>,
) -> Result<Vec<T>, ApiError> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (rows, next) = fetch(token.as_deref())?;
        all.extend(rows);
        match next {
            None => return Ok(all),
            Some(next) if token.as_deref() == Some(next.as_str()) => {
                return Err(ApiError::Incomplete);
            }
            Some(next) => token = Some(next),
        }
    }
}

/// The error of an answer that cannot be read, saying `what` in the crate's own words.
pub(super) fn unreadable(what: &str) -> ApiError {
    ApiError::Unreadable(what.to_owned())
}

/// The text after the last `/` of `url` (a resource URL's name), the whole text when it
/// holds none.
pub(super) fn last_segment(url: &str) -> &str {
    url.rsplit_once('/').map_or(url, |(_, last)| last)
}

/// The error a failed exchange is.
fn transport_error(error: TransportError) -> ApiError {
    match error {
        TransportError::Timeout => ApiError::Timeout,
        other => ApiError::Transport(other.to_string()),
    }
}

/// The reason of a Google error answer: the `reason` of the first of `error.errors`, else
/// the `error.status`. `None` when the body is not such an answer or the reason is empty.
fn error_reason(body: &[u8]) -> Option<String> {
    let answer: Value = serde_json::from_slice(body).ok()?;
    let error = answer.get("error")?;
    let first = error
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|first| first.get("reason"))
        .and_then(Value::as_str);
    first
        .or_else(|| error.get("status").and_then(Value::as_str))
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned)
}

/// The error an answer that is not a success is. HTTP 401 is always a denial; otherwise a
/// throttle reason is a throttle whatever the status, HTTP 429 is a throttle, a 403 is a
/// denial unless its reason is `quotaExceeded`, and any other answer is told by the reason.
/// The message of the answer is never kept.
fn refusal(status: u16, body: &[u8]) -> ApiError {
    let reason = error_reason(body);
    let answered = reason.as_deref().map(ApiError::answered);
    let reason_or = |default: &str| ErrorReason::new(reason.as_deref().unwrap_or(default));
    match (status, answered) {
        (401, _) => ApiError::Denied {
            reason: reason_or(UNAUTHENTICATED),
        },
        (_, Some(throttled @ ApiError::Throttled { .. })) => throttled,
        (429, _) => ApiError::Throttled {
            reason: ErrorReason::new(RATE_LIMIT),
        },
        (403, Some(error)) if reason.as_deref() == Some(QUOTA_EXCEEDED) => error,
        (403, _) => ApiError::Denied {
            reason: reason_or(FORBIDDEN),
        },
        (_, Some(error)) => error,
        (_, None) => unreadable(&format!("an error answer with no reason (HTTP {status})")),
    }
}

impl GceApi for HttpGce {
    fn machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError> {
        self.read_machine_types(project, zones)
    }

    fn accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
        self.read_accelerator_types(project, zones)
    }

    fn prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError> {
        self.read_prices(region)
    }

    fn region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError> {
        self.read_region_quotas(project, region)
    }

    fn insert_instance(&self, request: &InsertRequest) -> Result<Operation, ApiError> {
        self.insert(request)
    }

    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        self.read_instance(instance)
    }

    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        self.delete(instance)
    }

    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        self.list(filter)
    }

    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError> {
        self.read_operation(operation)
    }
}
