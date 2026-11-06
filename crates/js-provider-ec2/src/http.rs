//! The real client of the port: [`HttpEc2`] makes each call of [`Ec2Api`] as one signed
//! HTTPS request (or one walk over the pages of one list) over a
//! [`js_logstore::Transport`], and reads the answer into the crate's own records.
//!
//! **Endpoints.** EC2 is `https://ec2.<region>.amazonaws.com/` (the Query protocol: a
//! form-encoded POST, XML answers, signed for the service `ec2`). The Pricing API is
//! `https://api.pricing.us-east-1.amazonaws.com/` and Service Quotas
//! `https://servicequotas.<region>.amazonaws.com/` (JSON 1.1, the operation named in the
//! `x-amz-target` header). They are the cloud's public endpoint patterns; the Pricing API
//! lives in a few regions only, so it is always asked in `us-east-1`. Every request is
//! signed with Signature Version 4 ([`js_logstore::sigv4`]) and goes over `https`.
//!
//! **Answers.** A success is read into the records of [`crate::model`] and
//! [`crate::launch`]; a field the documentation says is there and is not makes the call
//! [`ApiError::Unreadable`]. A failure is told by the cloud's error code (the `Code` of an
//! XML answer, the `__type` of a JSON one) and by nothing else; the cloud's message is
//! never kept. An error answer with no code is [`ApiError::Unreadable`], a transport that
//! timed out is [`ApiError::Timeout`] and any other transport failure is
//! [`ApiError::Transport`].
//!
//! **Pages.** A list is read page by page under one call. A page that fails fails the
//! call, and a token the cloud repeats is [`ApiError::Incomplete`]: the walk never ends
//! short in silence.
//!
//! **Retries.** None. A call is made once and the caller decides.
//!
//! **Credentials.** See [`Credentials`]. No `Debug` output, error or log line of this
//! module holds a key, a session token or a signature.

use std::fmt;
use std::sync::Arc;

use js_logstore::sigv4::{Scope, Unsigned, canonical_query, sign};
use js_logstore::{Clock as WallClock, HttpRequest, Method, Transport, TransportError};
use serde_json::Value;

use crate::api::{Ec2Api, PurchaseKind};
use crate::error::ApiError;
use crate::launch::{
    BlockPurchase, CapacityReservation, DescribeFilter, Instance, InstanceId, RunRequest, Tag,
};
use crate::model::{
    BlockOffering, InstanceTypeInfo, Region, TypePrice, VcpuQuota, ZoneOffering, ZonePrice,
};
use crate::quota::QuotaFamily;

mod catalog_calls;
mod credential;
mod launch_calls;
mod pricing;
mod xml;

pub use credential::{Credentials, StaticKeys};
pub use pricing::QUOTAS_TARGET;

use credential::Resolver;
use xml::{Node, unreadable};

/// The version of the EC2 Query API every form request names, as the cloud spells it (a
/// protocol constant of the cloud, not a date of this project).
pub const EC2_API_VERSION: &str = "2016-11-15";

/// The content type of an EC2 request.
const FORM: &str = "application/x-www-form-urlencoded; charset=utf-8";

/// The content type of a Pricing or Service Quotas request.
const JSON: &str = "application/x-amz-json-1.1";

/// The region the Pricing API is asked in.
const PRICING_REGION: &str = "us-east-1";

/// How an error answer's body is read for its error code.
type CodeOf = fn(&[u8]) -> Option<String>;

/// The client of the port over a transport. It reads the clock only to sign.
pub struct HttpEc2 {
    region: Region,
    resolver: Resolver,
    transport: Arc<dyn Transport>,
    wall: Arc<dyn WallClock>,
}

impl fmt::Debug for HttpEc2 {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("HttpEc2")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl HttpEc2 {
    /// A client for `region`, signing with `credentials` and talking over `transport`.
    /// `wall` is the time of every signature and of the expiry of a role credential.
    /// The launch calls go to `region`; a catalog call goes to the region it is given.
    pub fn new(
        region: Region,
        credentials: Credentials,
        transport: Arc<dyn Transport>,
        wall: Arc<dyn WallClock>,
    ) -> Self {
        Self {
            region,
            resolver: Resolver::new(credentials),
            transport,
            wall,
        }
    }

    /// One signed request to `target` with `headers` and `body`: the body of the answer
    /// when it is a success.
    fn exchange(
        &self,
        target: &Target<'_>,
        mut headers: Vec<(String, String)>,
        body: Vec<u8>,
        code_of: CodeOf,
    ) -> Result<Vec<u8>, ApiError> {
        let credential = self.resolver.credential(&*self.transport, &*self.wall)?;
        let scope = Scope {
            region: target.region,
            service: target.service,
        };
        let unsigned = Unsigned {
            method: "POST",
            host: &target.host,
            path: "/",
            query: &[],
            headers: &headers,
            sign_also: &["content-type"],
            body: &body,
        };
        let signed = sign(&unsigned, &credential, &scope, self.wall.now());
        headers.push(("host".to_owned(), target.host.clone()));
        headers.extend(signed.headers);
        let request = HttpRequest {
            method: Method::Post,
            url: format!("https://{}/", target.host),
            headers,
            body,
        };
        let answer = self.transport.send(request).map_err(transport_error)?;
        tracing::debug!(host = %target.host, status = answer.status, "the cloud answered");
        match (200..300).contains(&answer.status) {
            true => Ok(answer.body),
            false => Err(refusal(answer.status, &answer.body, code_of)),
        }
    }

    /// An EC2 call in `region`: the body of the answer.
    fn ec2(&self, region: &str, form: &Form) -> Result<Vec<u8>, ApiError> {
        let target = Target {
            host: format!("ec2.{region}.amazonaws.com"),
            region,
            service: "ec2",
        };
        let headers = vec![("content-type".to_owned(), FORM.to_owned())];
        self.exchange(&target, headers, form.body(), xml_code)
    }

    /// An EC2 call in `region` whose answer is XML: the root element.
    fn query(&self, region: &str, form: &Form) -> Result<Node, ApiError> {
        xml::parse(&self.ec2(region, form)?)
    }

    /// A JSON call of `operation` to `target`: the answer.
    fn json(&self, target: &Target<'_>, operation: &str, body: &Value) -> Result<Value, ApiError> {
        let headers = vec![
            ("content-type".to_owned(), JSON.to_owned()),
            ("x-amz-target".to_owned(), operation.to_owned()),
        ];
        let answer = self.exchange(target, headers, body.to_string().into_bytes(), json_code)?;
        serde_json::from_slice(&answer).map_err(|_| unreadable("the answer is not JSON"))
    }

    /// A list of an EC2 describe call, in `region`, read page by page: `form` with the
    /// token of each page, and `read` of each page's rows.
    fn listing<T>(
        &self,
        region: &str,
        form: &Form,
        read: impl Fn(&Node) -> Result<Vec<T>, ApiError>,
    ) -> Result<Vec<T>, ApiError> {
        pages(|token| {
            let page = form.clone().with_opt("NextToken", token);
            let answer = self.query(region, &page)?;
            let next = answer
                .text_at("nextToken")
                .filter(|text| !text.is_empty())
                .map(str::to_owned);
            Ok((read(&answer)?, next))
        })
    }
}

/// Where a request goes and what it is signed for.
struct Target<'a> {
    host: String,
    region: &'a str,
    service: &'static str,
}

/// The parameters of an EC2 Query request.
#[derive(Clone)]
struct Form(Vec<(String, String)>);

impl Form {
    /// A request of `action`.
    fn new(action: &str) -> Self {
        Self(vec![
            ("Action".to_owned(), action.to_owned()),
            ("Version".to_owned(), EC2_API_VERSION.to_owned()),
        ])
    }

    /// The form with the parameter `key` set to `value`.
    fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.push((key.into(), value.into()));
        self
    }

    /// The form with the parameter `key` set to `value` when there is one.
    fn with_opt(mut self, key: &str, value: Option<&str>) -> Self {
        self.0
            .extend(value.map(|value| (key.to_owned(), value.to_owned())));
        self
    }

    /// The form as the body of a request: the names and values percent-encoded.
    fn body(&self) -> Vec<u8> {
        canonical_query(&self.0).into_bytes()
    }
}

/// Reads a list in pages: `fetch` takes the token of the page (none for the first) and
/// answers its rows and the token of the next page, if there is one.
///
/// # Errors
///
/// The error of the first page that fails, or [`ApiError::Incomplete`] when the cloud
/// answers the token it was just given.
fn pages<T>(
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

/// The error a failed exchange is.
fn transport_error(error: TransportError) -> ApiError {
    match error {
        TransportError::Timeout => ApiError::Timeout,
        other => ApiError::Transport(other.to_string()),
    }
}

/// The error an answer that is not a success is: the cloud's error code when `code_of`
/// finds one, else an unreadable answer. The message of the answer is never kept.
fn refusal(status: u16, body: &[u8], code_of: CodeOf) -> ApiError {
    code_of(body).filter(|code| !code.is_empty()).map_or_else(
        || ApiError::Unreadable(format!("an error answer with no code (HTTP {status})")),
        |code| ApiError::answered(&code),
    )
}

/// The `Code` of an XML error answer (`Response/Errors/Error/Code`, or the `Error/Code` of
/// an `ErrorResponse`).
fn xml_code(body: &[u8]) -> Option<String> {
    let root = xml::parse(body).ok()?;
    root.find("Code").map(|code| code.text().to_owned())
}

/// The code of a JSON error answer: the `__type` after its last `#`.
fn json_code(body: &[u8]) -> Option<String> {
    let answer: Value = serde_json::from_slice(body).ok()?;
    answer
        .get("__type")
        .and_then(Value::as_str)
        .and_then(|kind| kind.rsplit('#').next())
        .map(str::to_owned)
}

impl Ec2Api for HttpEc2 {
    fn instance_types(&self, region: &Region) -> Result<Vec<InstanceTypeInfo>, ApiError> {
        self.read_instance_types(region)
    }

    fn zone_offerings(&self, region: &Region) -> Result<Vec<ZoneOffering>, ApiError> {
        self.read_zone_offerings(region)
    }

    fn on_demand_prices(&self, region: &Region) -> Result<Vec<TypePrice>, ApiError> {
        self.read_on_demand_prices(region)
    }

    fn spot_prices(&self, region: &Region) -> Result<Vec<ZonePrice>, ApiError> {
        self.read_spot_prices(region)
    }

    fn vcpu_quota(
        &self,
        region: &Region,
        family: QuotaFamily,
        purchase: PurchaseKind,
    ) -> Result<Option<VcpuQuota>, ApiError> {
        self.read_vcpu_quota(region, family, purchase)
    }

    fn capacity_block_offerings(
        &self,
        region: &Region,
        hours: u32,
    ) -> Result<Vec<BlockOffering>, ApiError> {
        self.read_capacity_blocks(region, hours)
    }

    fn run_instances(&self, request: &RunRequest) -> Result<Vec<InstanceId>, ApiError> {
        self.launch(request)
    }

    fn describe_instances(&self, filter: &DescribeFilter) -> Result<Vec<Instance>, ApiError> {
        self.read_instances(filter)
    }

    fn terminate_instances(&self, ids: &[InstanceId]) -> Result<(), ApiError> {
        self.terminate(ids)
    }

    fn purchase_capacity_block(
        &self,
        request: &BlockPurchase,
    ) -> Result<CapacityReservation, ApiError> {
        self.purchase_block(request)
    }

    fn capacity_reservations(&self, tags: &[Tag]) -> Result<Vec<CapacityReservation>, ApiError> {
        self.read_reservations(tags)
    }
}
