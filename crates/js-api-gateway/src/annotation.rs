//! The routes of a service, read from the `google.api.http` option of its methods, and the
//! checks each must pass. The build script includes this file as a module of its own and
//! generates the gateway from [`routes`] over `jobscheduler.v1.JobService`; the crate's tests
//! run the same function over test descriptors. It uses nothing else of the crate.
//!
//! What the gateway serves of the option; anything else is a [`Fault`], and the build fails
//! with the method's name and the fault:
//!
//! - one `get` or `post` pattern (`put`, `delete`, `patch` and `custom` are refused, and so
//!   are `additional_bindings` and `response_body`);
//! - a body of `*`, the whole request, on a POST, or no body (a named body field is refused);
//! - a path template under `/v1/` of literal segments (`[a-z0-9._-]`) and variables
//!   (`{field}`), with an optional custom verb (`:cancel`, lower-case letters) after a final
//!   variable. Its second segment, the resource, is a literal other than `leases` and
//!   `agent`, so no route can be produced under the lease or agent routes (`/healthz` and
//!   `/status` are outside `/v1/`);
//! - each variable names a singular string field of the request, once;
//! - no two methods share an HTTP method and a path that differ only in the names of their
//!   variables;
//! - the method's `jobscheduler.v1.http_success_status` option, the status of a success, is
//!   200 or 201; absent, it is 200.
//!
//! A method without the `google.api.http` option has no route.

use std::collections::{BTreeMap, BTreeSet};

use prost::Message as _;
use prost_reflect::{Cardinality, DescriptorPool, Kind, MethodDescriptor};

/// The resources under `/v1/` that no route may name: the lease routes and the agent routes
/// of the lease server.
pub const RESERVED: [&str; 2] = ["leases", "agent"];

/// An HTTP method the gateway serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verb {
    /// `GET`: a call that changes nothing. It takes no body.
    Get,
    /// `POST`: a call that changes state.
    Post,
}

impl Verb {
    /// The method as HTTP spells it: `GET` or `POST`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// One route as the option of a method declares it, after the checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    /// The method's name in the service, such as `GetJob`.
    pub rpc: String,
    /// The HTTP method.
    pub verb: Verb,
    /// The path template as the option spells it, such as `/v1/jobs/{job_id}:cancel`.
    pub template: String,
    /// The body is the whole request (`body: "*"`). Without it the route takes no body.
    pub body: bool,
    /// The HTTP status of a success: 200, or 201 where the method's
    /// `http_success_status` option says so.
    pub status: u16,
    /// The full name of the request message.
    pub input: String,
    /// The full name of the response message.
    pub output: String,
}

/// One `/`-separated segment of a path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// A segment a request path must spell exactly.
    Literal(String),
    /// A segment that sets the request field it names to the text of the request path.
    Variable(String),
}

/// A path template, parsed: its segments (the first is always `v1`) and the custom verb
/// after the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The segments, in order.
    pub segments: Vec<Segment>,
    /// The custom verb, without its colon: `cancel` for `/v1/jobs/{job_id}:cancel`.
    pub verb: Option<String>,
}

impl Template {
    /// Parses a path template, refusing one the gateway does not serve (the module's list).
    ///
    /// # Errors
    ///
    /// [`Fault::NotUnderV1`] for a path that does not start with `/v1/`, [`Fault::Template`]
    /// for one outside the grammar, and [`Fault::Reserved`] for one whose resource is not a
    /// literal of its own.
    pub fn parse(text: &str) -> Result<Self, Fault> {
        let rest = text.strip_prefix("/v1/").ok_or(Fault::NotUnderV1)?;
        let (path, verb) = match rest.split_once(':') {
            Some((path, verb)) => (path, Some(verb)),
            None => (rest, None),
        };
        let mut segments = vec![Segment::Literal("v1".to_owned())];
        for part in path.split('/') {
            segments.push(segment(part).ok_or(Fault::Template)?);
        }
        let verb_ok = verb.is_none_or(|verb| {
            all_of(verb, |c| c.is_ascii_lowercase())
                && matches!(segments.last(), Some(Segment::Variable(_)))
        });
        if !verb_ok {
            return Err(Fault::Template);
        }
        let resource_ok = matches!(
            segments.get(1),
            Some(Segment::Literal(resource)) if !RESERVED.contains(&resource.as_str())
        );
        if !resource_ok {
            return Err(Fault::Reserved);
        }
        Ok(Self {
            segments,
            verb: verb.map(str::to_owned),
        })
    }

    /// The names of the template's variables, in order.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Variable(name) => Some(name.as_str()),
            Segment::Literal(_) => None,
        })
    }

    /// The template with every variable written `{}`: two templates of one shape match the
    /// same request paths.
    #[must_use]
    pub fn shape(&self) -> String {
        let mut shape = String::new();
        for segment in &self.segments {
            shape.push('/');
            match segment {
                Segment::Literal(literal) => shape.push_str(literal),
                Segment::Variable(_) => shape.push_str("{}"),
            }
        }
        if let Some(verb) = &self.verb {
            shape.push(':');
            shape.push_str(verb);
        }
        shape
    }
}

/// Why a method's option is not one the gateway serves.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// The option's bytes do not decode as a `google.api.HttpRule`.
    #[error("its google.api.http option does not decode as a google.api.HttpRule")]
    Undecodable,
    /// The rule names no pattern.
    #[error("its route names no HTTP method")]
    NoMethod,
    /// The rule's pattern is another method than GET or POST; the method and path follow.
    #[error("its route is {0}; the gateway serves GET and POST only")]
    Method(String),
    /// The rule has additional bindings.
    #[error("it has additional_bindings; the gateway serves one route per method")]
    AdditionalBindings,
    /// The rule has a response body.
    #[error("it has a response_body; the gateway answers the whole response")]
    ResponseBody,
    /// A GET rule names a body.
    #[error("it is a GET route with a body")]
    BodyOnGet,
    /// The rule names one field as the body.
    #[error("its body is `{0}`; the gateway takes the whole request (`*`) or no body")]
    NamedBody(String),
    /// The path does not start with `/v1/`.
    #[error("its path is not under /v1/")]
    NotUnderV1,
    /// The path is outside the grammar of the module.
    #[error(
        "its path is not literal segments of [a-z0-9._-] and {{field}} variables, with an \
         optional :verb of lower-case letters after a final variable"
    )]
    Template,
    /// The path's resource is reserved or is a variable.
    #[error(
        "its path names no resource of its own after /v1/ (a literal other than leases and \
         agent)"
    )]
    Reserved,
    /// A path variable names no field of the request.
    #[error("its path variable {0} names no field of the request")]
    UnknownVariable(String),
    /// A path variable names a field that is not a singular string.
    #[error("its path variable {0} is not a singular string field")]
    NotAString(String),
    /// A path variable appears twice in the path.
    #[error("its path variable {0} appears twice")]
    RepeatedVariable(String),
    /// The method's `http_success_status` is neither 200 nor 201.
    #[error("its http_success_status is {0}; the gateway answers a success 200 or 201")]
    SuccessStatus(u32),
    /// Another method, named here, has the same HTTP method and the same path shape.
    #[error("its method and path are those of {0}")]
    Duplicate(String),
}

/// Why the routes of a service could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnnotationError {
    /// The descriptor set has no service of the full name given.
    #[error("the descriptor set has no service {0}")]
    NoService(String),
    /// A method's option is not one the gateway serves.
    #[error("{rpc}: {fault}")]
    Rpc {
        /// The method's name.
        rpc: String,
        /// What is wrong with its option.
        fault: Fault,
    },
}

/// The routes of the service `service` (a full name, such as `jobscheduler.v1.JobService`)
/// in `pool`: one for each method that carries a `google.api.http` option, in the order of
/// the service definition.
///
/// # Errors
///
/// [`AnnotationError::NoService`] when the pool has no such service, and
/// [`AnnotationError::Rpc`] for the first method whose option the gateway does not serve.
pub fn routes(pool: &DescriptorPool, service: &str) -> Result<Vec<RouteSpec>, AnnotationError> {
    let service_descriptor = pool
        .get_service_by_name(service)
        .ok_or_else(|| AnnotationError::NoService(service.to_owned()))?;
    let mut shapes: BTreeMap<(Verb, String), String> = BTreeMap::new();
    let mut routes = Vec::new();
    for method in service_descriptor.methods() {
        let rpc = method.name().to_owned();
        let at = |fault: Fault| AnnotationError::Rpc {
            rpc: rpc.clone(),
            fault,
        };
        let Some((route, shape)) = read_route(&method).map_err(at)? else {
            continue;
        };
        if let Some(other) = shapes.insert((route.verb, shape), rpc.clone()) {
            return Err(at(Fault::Duplicate(other)));
        }
        routes.push(route);
    }
    Ok(routes)
}

/// The route of one method and the shape of its path, or `None` for a method without the
/// option.
fn read_route(method: &MethodDescriptor) -> Result<Option<(RouteSpec, String)>, Fault> {
    let options = method.options().encode_to_vec();
    let option = RouteOptions::decode(options.as_slice()).map_err(|_| Fault::Undecodable)?;
    let Some(rule) = option.http else {
        return Ok(None);
    };
    let (verb, template) = match rule.pattern {
        Some(Pattern::Get(path)) => (Verb::Get, path),
        Some(Pattern::Post(path)) => (Verb::Post, path),
        Some(Pattern::Put(path)) => return Err(Fault::Method(format!("PUT {path}"))),
        Some(Pattern::Delete(path)) => return Err(Fault::Method(format!("DELETE {path}"))),
        Some(Pattern::Patch(path)) => return Err(Fault::Method(format!("PATCH {path}"))),
        Some(Pattern::Custom(custom)) => {
            return Err(Fault::Method(format!("{} {}", custom.kind, custom.path)));
        }
        None => return Err(Fault::NoMethod),
    };
    if !rule.additional_bindings.is_empty() {
        return Err(Fault::AdditionalBindings);
    }
    if !rule.response_body.is_empty() {
        return Err(Fault::ResponseBody);
    }
    let body = match (verb, rule.body.as_str()) {
        (_, "") => false,
        (Verb::Post, "*") => true,
        (Verb::Get, "*") => return Err(Fault::BodyOnGet),
        (_, named) => return Err(Fault::NamedBody(named.to_owned())),
    };
    let status = match option.success_status {
        None | Some(200) => 200,
        Some(201) => 201,
        Some(other) => return Err(Fault::SuccessStatus(other)),
    };
    let parsed = Template::parse(&template)?;
    let input = method.input();
    let mut seen = BTreeSet::new();
    for name in parsed.variables() {
        let field = input
            .get_field_by_name(name)
            .ok_or_else(|| Fault::UnknownVariable(name.to_owned()))?;
        let singular_string = matches!(
            (field.kind(), field.cardinality()),
            (Kind::String, Cardinality::Optional | Cardinality::Required)
        );
        if !singular_string {
            return Err(Fault::NotAString(name.to_owned()));
        }
        if !seen.insert(name) {
            return Err(Fault::RepeatedVariable(name.to_owned()));
        }
    }
    let route = RouteSpec {
        rpc: method.name().to_owned(),
        verb,
        template,
        body,
        status,
        input: input.full_name().to_owned(),
        output: method.output().full_name().to_owned(),
    };
    Ok(Some((route, parsed.shape())))
}

/// One segment of a template, or `None` for a segment outside the grammar.
fn segment(part: &str) -> Option<Segment> {
    match part
        .strip_prefix('{')
        .and_then(|inner| inner.strip_suffix('}'))
    {
        Some(name) => all_of(name, |c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
            .then(|| Segment::Variable(name.to_owned())),
        None => all_of(
            part,
            |c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-'),
        )
        .then(|| Segment::Literal(part.to_owned())),
    }
}

/// Whether `text` is not empty and every character of it is `allowed`.
fn all_of(text: &str, allowed: impl Fn(char) -> bool) -> bool {
    !text.is_empty() && text.chars().all(allowed)
}

/// The options of a method, as far as the gateway reads them: the `google.api.http` option
/// (`google/api/annotations.proto`) and `jobscheduler.v1.http_success_status`
/// (`service.proto`). Decoding the encoded options skips every other field.
#[derive(Clone, PartialEq, prost::Message)]
struct RouteOptions {
    #[prost(message, optional, tag = "72295728")]
    http: Option<HttpRule>,
    #[prost(uint32, optional, tag = "50100")]
    success_status: Option<u32>,
}

/// The fields of `google.api.HttpRule` (`google/api/http.proto`) the gateway reads; the
/// others (`selector`) are skipped.
#[derive(Clone, PartialEq, prost::Message)]
struct HttpRule {
    #[prost(oneof = "Pattern", tags = "2, 3, 4, 5, 6, 8")]
    pattern: Option<Pattern>,
    #[prost(string, tag = "7")]
    body: String,
    #[prost(string, tag = "12")]
    response_body: String,
    #[prost(message, repeated, tag = "11")]
    additional_bindings: Vec<HttpRule>,
}

/// The `pattern` oneof of `google.api.HttpRule`.
#[derive(Clone, PartialEq, prost::Oneof)]
enum Pattern {
    #[prost(string, tag = "2")]
    Get(String),
    #[prost(string, tag = "3")]
    Put(String),
    #[prost(string, tag = "4")]
    Post(String),
    #[prost(string, tag = "5")]
    Delete(String),
    #[prost(string, tag = "6")]
    Patch(String),
    #[prost(message, tag = "8")]
    Custom(CustomPattern),
}

/// `google.api.CustomHttpPattern`.
#[derive(Clone, PartialEq, prost::Message)]
struct CustomPattern {
    #[prost(string, tag = "1")]
    kind: String,
    #[prost(string, tag = "2")]
    path: String,
}
