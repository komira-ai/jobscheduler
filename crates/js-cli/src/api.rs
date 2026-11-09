//! The join-token routes of the server, as calls that return what the answer means.

use serde_json::Value;

use js_client::{Client, Reply};

use crate::failure::Failure;

/// A join token the server made. It has no `Debug`: the token is a secret.
pub(crate) struct Minted {
    pub(crate) token: String,
    pub(crate) expires_at_ms: u64,
}

/// The route of the join tokens.
const JOIN_TOKENS: &str = "/v1/agent/join-tokens";

/// The failure of a join-token call that did not succeed: a 404 is a server that admits
/// no agents (so has no such route).
fn join_tokens_refusal(reply: &Reply) -> Failure {
    match reply.status {
        404 => Failure::Refused("join-tokens-unsupported".to_owned()),
        _ => refusal(reply),
    }
}

/// `POST /v1/agent/join-tokens`: 201 with the token and its expiry. The route exists only
/// on a server that admits agents; a 404 says so.
pub(crate) fn mint_join_token(client: &Client, body: &Value) -> Result<Minted, Failure> {
    let reply = client.post(JOIN_TOKENS, body)?;
    match reply.status {
        201 => Ok(Minted {
            token: text(&reply.body, "token")?,
            expires_at_ms: number(&reply.body, "expires_at_ms")?,
        }),
        _ => Err(join_tokens_refusal(&reply)),
    }
}

/// `DELETE /v1/agent/join-tokens`: 204.
pub(crate) fn revoke_join_tokens(client: &Client, body: &Value) -> Result<(), Failure> {
    let reply = client.delete_with(JOIN_TOKENS, body)?;
    match reply.status {
        204 => Ok(()),
        _ => Err(join_tokens_refusal(&reply)),
    }
}

/// Text the server sent, as it may be printed: without control characters, which a
/// terminal would act on.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// What a status this client has no meaning for is called in a failure.
fn unexpected(reply: &Reply) -> String {
    let code = printable(reply.body["error"].as_str().unwrap_or("none"));
    format!("unexpected answer {} ({code})", reply.status)
}

fn text(body: &Value, key: &str) -> Result<String, Failure> {
    let found = body[key].as_str().map(str::to_owned);
    found.ok_or_else(|| malformed(key))
}

fn number(body: &Value, key: &str) -> Result<u64, Failure> {
    body[key].as_u64().ok_or_else(|| malformed(key))
}

fn malformed(key: &str) -> Failure {
    Failure::Server(format!("the server's answer has no usable {key:?}"))
}

/// The failure a reply that is not the success of its route stands for: 404 and 409 are
/// the server saying no (the code is the answer), 400 and 422 that the request itself
/// is refused, and anything else, a 401 included, that the server or the setup is wrong.
fn refusal(reply: &Reply) -> Failure {
    let code = printable(reply.body["error"].as_str().unwrap_or("none"));
    let reason = reply.body["reason"].as_str().map(printable);
    let detail = reason.map_or_else(|| code.clone(), |reason| format!("{code}: {reason}"));
    match reply.status {
        404 | 409 => Failure::Refused(code),
        400 | 422 => Failure::Contract(detail),
        401 => Failure::Server("the server refused the token (401)".to_owned()),
        _ => Failure::Server(unexpected(reply)),
    }
}
