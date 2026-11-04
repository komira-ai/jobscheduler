//! The router: one axum route per resource of [`ROUTES`], each handled by matching the
//! request against the routes' templates, decoding the request message, calling the
//! service through [`crate::call`] and writing the response as JSON.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use http_body_util::LengthLimitError;
use js_proto::v1::job_service_server::JobService;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};

use crate::annotation::{Segment, Template};
use crate::answer::Answer;
use crate::{BODY_LIMIT, ROUTES, Route, input};

/// A route with what serving it needs: its parsed template and its messages' descriptors.
struct Binding {
    route: &'static Route,
    template: Template,
    status: StatusCode,
    input: MessageDescriptor,
    output: MessageDescriptor,
}

/// The state every request shares: the service and the bindings of [`ROUTES`], or `None`
/// when the descriptor set the router was made with does not hold them (every request is
/// then a 500 `internal`).
struct Gateway<T> {
    service: Arc<T>,
    bindings: Option<Vec<Binding>>,
}

/// The router of [`ROUTES`] over `service`, with the routes' messages read from
/// `descriptors`, an encoded descriptor set.
pub(crate) fn router_over<T: JobService>(service: Arc<T>, descriptors: &[u8]) -> Router {
    let gateway = Arc::new(Gateway {
        service,
        bindings: bindings(descriptors),
    });
    let resources: BTreeSet<&str> = ROUTES
        .iter()
        .map(|route| resource(route.template))
        .collect();
    let mut router: Router<Arc<Gateway<T>>> = Router::new();
    for resource in resources {
        router = router
            .route(resource, any(handle::<T>))
            .route(&format!("{resource}/{{*rest}}"), any(handle::<T>));
    }
    router.with_state(gateway)
}

/// The bindings of [`ROUTES`] in the descriptor set `descriptors`, or `None` when it does
/// not decode or lacks a route's message.
fn bindings(descriptors: &[u8]) -> Option<Vec<Binding>> {
    let pool = DescriptorPool::decode(descriptors).ok()?;
    ROUTES
        .iter()
        .map(|route| {
            Some(Binding {
                route,
                template: Template::parse(route.template).ok()?,
                status: StatusCode::from_u16(route.status).ok()?,
                input: pool.get_message_by_name(route.input)?,
                output: pool.get_message_by_name(route.output)?,
            })
        })
        .collect()
}

/// The path of a template's resource, its first two segments: `/v1/jobs` for
/// `/v1/jobs/{job_id}:cancel`.
fn resource(template: &str) -> &str {
    let end = template
        .match_indices('/')
        .nth(2)
        .map_or(template.len(), |(index, _)| index);
    &template[..end]
}

/// Answers one request.
async fn handle<T: JobService>(
    State(gateway): State<Arc<Gateway<T>>>,
    request: Request,
) -> Response {
    match gateway.serve(request).await {
        Ok(response) => response,
        Err(answer) => answer.into_response(),
    }
}

impl<T: JobService> Gateway<T> {
    /// The response to `request`, or the answer that refuses it. The body is read, up to
    /// [`BODY_LIMIT`], only once a route matches.
    async fn serve(&self, request: Request) -> Result<Response, Answer> {
        let bindings = self.bindings.as_deref().ok_or_else(Answer::internal)?;
        let (parts, body) = request.into_parts();
        let (binding, variables) = find(bindings, &parts.method, parts.uri.path())?;
        let body = read(body).await?;
        let input = input::build(
            &binding.input,
            binding.route.body,
            &variables,
            parts.uri.query(),
            &body,
        )?;
        let output = crate::call(
            self.service.as_ref(),
            binding.route.rpc,
            input,
            &binding.output,
        )
        .await?;
        Ok(respond(binding.status, &output))
    }
}

/// A binding that serves a request, with the path variables it captured (field name and
/// text).
type Matched<'a> = (&'a Binding, Vec<(String, String)>);

/// The binding that serves `method` on `path`, with the path variables it captures; the
/// first in the order of [`ROUTES`] when two match. A 404 when no template matches the
/// path, a 405 naming the methods that would when only other methods' do.
fn find<'a>(bindings: &'a [Binding], method: &Method, path: &str) -> Result<Matched<'a>, Answer> {
    let mut allow = Vec::new();
    for binding in bindings {
        let Some(variables) = capture(&binding.template, path) else {
            continue;
        };
        if binding.route.verb.as_str() == method.as_str() {
            return Ok((binding, variables));
        }
        allow.push(binding.route.verb);
    }
    if allow.is_empty() {
        Err(Answer::not_found())
    } else {
        Err(Answer::method_not_allowed(allow))
    }
}

/// The variables `template` captures from `path` (field name and text), or `None` when the
/// path does not match. A variable captures one whole segment, the empty one too; path
/// text is not percent-decoded.
fn capture(template: &Template, path: &str) -> Option<Vec<(String, String)>> {
    let path = match &template.verb {
        Some(verb) => path.strip_suffix(verb.as_str())?.strip_suffix(':')?,
        None => path,
    };
    let parts: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    if parts.len() != template.segments.len() {
        return None;
    }
    let mut variables = Vec::new();
    for (segment, part) in template.segments.iter().zip(parts) {
        match segment {
            Segment::Literal(literal) => {
                if literal != part {
                    return None;
                }
            }
            Segment::Variable(field) => variables.push((field.clone(), part.to_owned())),
        }
    }
    Some(variables)
}

/// The body, refusing one over [`BODY_LIMIT`] with a 413 and one that cannot be read with a
/// 400.
async fn read(body: Body) -> Result<Bytes, Answer> {
    to_bytes(body, BODY_LIMIT).await.map_err(|err| {
        if err.into_inner().is::<LengthLimitError>() {
            Answer::too_large()
        } else {
            Answer::invalid(None, "the body could not be read")
        }
    })
}

/// The success answer `status` of a call's response, written by the proto3 JSON mapping
/// (`DynamicMessage`'s own serialization, the default options of prost-reflect).
fn respond(status: StatusCode, output: &DynamicMessage) -> Response {
    (status, Json(output)).into_response()
}
