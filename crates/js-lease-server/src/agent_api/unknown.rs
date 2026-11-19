//! The agent routes' bearer when this server does not hold it: read from the store's row of
//! it, which a token another server minted, or a credential it issued, after this server
//! started is in ([`crate::join_tokens::JoinTokens::identify_stored`]), and answered 401 only
//! when the store holds no such row.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use js_registry::Credential;
use tracing::warn;

use super::{Refused, STATE_WRITE_FAILED};
use crate::agents::Agents;
use crate::util::blocking;

/// The answer to a bearer that is neither a pool's token nor one this server or the store
/// holds (`Ok(None)`): 401. A bearer whose row the store could not be read for (`Err`) is
/// answered 503 `state-write-failed` instead, which the agent retries: it is not known to be
/// unknown.
pub(super) fn unknown_bearer(unknown: Result<Option<Credential>, String>) -> Response {
    match unknown {
        Err(error) => {
            warn!(%error, "agent request refused: its token could not be read from the store");
            Refused::new(StatusCode::SERVICE_UNAVAILABLE, STATE_WRITE_FAILED).into_response()
        }
        Ok(_) => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The credential of the one-time token or host credential `given`: from the tokens this
/// server holds, or else from the store's row of it, which a token another server minted or
/// a credential it issued after this server started is in
/// ([`crate::join_tokens::JoinTokens::identify_stored`]).
pub(super) async fn identify_single(
    agents: &Arc<Agents>,
    given: &str,
) -> Result<Option<Credential>, String> {
    if let Some(known) = agents.joins.identify(given) {
        return Ok(Some(known));
    }
    let this = Arc::clone(agents);
    let given = given.to_owned();
    blocking(move || this.joins.identify_stored(&given)).await
}
