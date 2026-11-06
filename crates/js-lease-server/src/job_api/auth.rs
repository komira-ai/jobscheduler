//! The bearer layer of the job service: one check of `Authorization: Bearer <token>` in
//! front of both transports, gRPC (where the header is the metadata `authorization`) and the
//! HTTP/JSON gateway.
//!
//! The header is read with the lease routes' [`bearer`] and the token compared with every
//! client's by [`Clients::identify`]; the layer holds no comparison of its own. A request
//! whose token is a client's goes on with that client's name ([`ClientName`]) in its
//! extensions, where the calls read it (the gateway hands a call the extensions of its HTTP
//! request), so the name comes from the token and never from the request's body. A request
//! whose header is missing, is not `Bearer <token>`, or names no client's token reaches no
//! handler: a gRPC request (`content-type: application/grpc...`) is answered
//! `UNAUTHENTICATED` with an empty message, any other request 401 with an empty body.

use std::future::{Future, ready};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::{HeaderMap, Request, Response, StatusCode, header};
use js_jobs::ClientName;
use tonic::Status;
use tower::{Layer, Service};

use crate::api::bearer;
use crate::config::Clients;

/// The content type every gRPC request carries, alone or with a suffix such as `+proto`.
const GRPC_CONTENT_TYPE: &[u8] = b"application/grpc";

/// The layer that puts [`Bearer`] in front of a service.
#[derive(Clone)]
pub(crate) struct BearerLayer {
    clients: Clients,
}

impl BearerLayer {
    /// The layer that admits requests that present the token of one of `clients`.
    pub(crate) fn new(clients: Clients) -> BearerLayer {
        BearerLayer { clients }
    }
}

impl<S> Layer<S> for BearerLayer {
    type Service = Bearer<S>;

    fn layer(&self, inner: S) -> Bearer<S> {
        Bearer {
            clients: self.clients.clone(),
            inner,
        }
    }
}

/// A service that passes on a request that presents a client's token, with the client's
/// name, and answers any other itself.
///
/// It is ready when the service behind it is. A refused request is answered without a
/// call, which leaves that readiness unused; the services it guards (the generated gRPC
/// server and the gateway's routes) are always ready and keep no state between calls.
#[derive(Clone)]
pub(crate) struct Bearer<S> {
    clients: Clients,
    inner: S,
}

/// The answer of [`Bearer`]: the inner service's, or the refusal.
type Answer<R, E> = Pin<Box<dyn Future<Output = Result<R, E>> + Send>>;

impl<S, B, R> Service<Request<B>> for Bearer<S>
where
    S: Service<Request<B>, Response = Response<R>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Default + Send + 'static,
{
    type Response = Response<R>;
    type Error = S::Error;
    type Future = Answer<Response<R>, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        match client(&self.clients, request.headers()) {
            Some(name) => {
                request.extensions_mut().insert(name);
                Box::pin(self.inner.call(request))
            }
            None => Box::pin(ready(Ok(refusal(request.headers())))),
        }
    }
}

/// The client whose token `headers` present, as an `Authorization` value `Bearer <token>`.
fn client(clients: &Clients, headers: &HeaderMap) -> Option<ClientName> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer)
        .and_then(|given| clients.identify(given.as_bytes()))
}

/// The answer to a request that does not present the token, with an empty body of the
/// service's body type: the head of [`refusal_head`].
fn refusal<R: Default>(headers: &HeaderMap) -> Response<R> {
    let (head, ()) = refusal_head(headers).into_parts();
    Response::from_parts(head, R::default())
}

/// The head of the answer to a request that does not present the token: `UNAUTHENTICATED`
/// to a gRPC request, 401 to any other. It is not generic over the body, so each of its
/// two answers is one piece of code whichever transport the layer guards.
fn refusal_head(headers: &HeaderMap) -> Response<()> {
    if is_grpc(headers) {
        Status::unauthenticated("").into_http()
    } else {
        let mut response = Response::new(());
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        response
    }
}

/// Whether `headers` are those of a gRPC request: its content type starts with
/// `application/grpc`.
pub(super) fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(GRPC_CONTENT_TYPE))
}
