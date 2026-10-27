//! The routes of the lease server, as calls that return what the answer means.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{Client, Reply};
use crate::failure::Failure;

/// A lease that is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lease {
    pub(crate) id: String,
    pub(crate) host: String,
    pub(crate) expires_at_ms: u64,
}

/// The answer to a start.
#[derive(Debug)]
pub(crate) enum Granted {
    /// 201: the lease is held.
    Held(Lease),
    /// 202: the provider has not confirmed the start; this is the lease's id.
    Pending(String),
}

/// The answer to a release.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Freed {
    /// 200: the host is free.
    Freed,
    /// 202: the stop is not confirmed; the server retries it.
    Releasing,
}

/// Where a lease stands in the server's list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Held now.
    Held(Lease),
    /// Still waiting for the provider.
    Pending,
    /// Not listed, or on its way out.
    Gone,
}

/// One lease of the list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) host: Option<String>,
    pub(crate) expires_at_ms: u64,
    pub(crate) labels: BTreeMap<String, String>,
}

impl Entry {
    fn standing(self) -> Standing {
        match self.state.as_str() {
            "pending" => Standing::Pending,
            "held" => Standing::Held(Lease {
                id: self.id,
                host: self.host.unwrap_or_default(),
                expires_at_ms: self.expires_at_ms,
            }),
            _ => Standing::Gone,
        }
    }
}

impl Client {
    /// `POST /v1/leases`: 201 held, 202 pending, anything else a failure. The request is
    /// sent once. When its answer is lost or unusable the lease may exist, and the failure
    /// says so ([`Failure::Unconfirmed`]); a refusal the server meant (400, 401, 404, 409,
    /// 422) created nothing.
    pub(crate) fn acquire(&self, body: &Value) -> Result<Granted, Failure> {
        let reply = self.post("/v1/leases", body).map_err(unconfirmed)?;
        match reply.status {
            201 => held(&reply.body).map(Granted::Held).map_err(unconfirmed),
            202 => text(&reply.body, "id")
                .map(Granted::Pending)
                .map_err(unconfirmed),
            400 | 401 | 404 | 409 | 422 => Err(refusal(&reply)),
            _ => Err(Failure::Unconfirmed(unexpected(&reply))),
        }
    }

    /// `POST /v1/leases/{id}/renew`: the new expiry in milliseconds.
    pub(crate) fn renew(&self, id: &str, ttl: Option<Duration>) -> Result<u64, Failure> {
        self.renew_within(id, ttl, Duration::MAX)
    }

    /// `POST /v1/leases/{id}/renew` that must be answered within `limit`.
    pub(crate) fn renew_within(
        &self,
        id: &str,
        ttl: Option<Duration>,
        limit: Duration,
    ) -> Result<u64, Failure> {
        let body = json!({ "ttl_seconds": ttl.map(|ttl| ttl.as_secs()) });
        let reply = self.post_within(&format!("/v1/leases/{id}/renew"), &body, limit)?;
        match reply.status {
            200 => number(&reply.body, "expires_at_ms"),
            _ => Err(refusal(&reply)),
        }
    }

    /// `DELETE /v1/leases/{id}`.
    pub(crate) fn release(&self, id: &str) -> Result<Freed, Failure> {
        let reply = self.delete(&format!("/v1/leases/{id}"))?;
        match reply.status {
            200 => Ok(Freed::Freed),
            202 => Ok(Freed::Releasing),
            _ => Err(refusal(&reply)),
        }
    }

    /// `GET /v1/leases`: the server's body.
    pub(crate) fn list(&self) -> Result<Value, Failure> {
        let reply = self.get("/v1/leases")?;
        match reply.status {
            200 => Ok(reply.body),
            _ => Err(refusal(&reply)),
        }
    }

    /// Where lease `id` stands now.
    pub(crate) fn standing(&self, id: &str) -> Result<Standing, Failure> {
        let entries = entries(&self.list()?)?;
        let found = entries.into_iter().find(|entry| entry.id == id);
        Ok(found.map_or(Standing::Gone, Entry::standing))
    }
}

/// The leases of a list body.
pub(crate) fn entries(body: &Value) -> Result<Vec<Entry>, Failure> {
    let leases = body["leases"]
        .as_array()
        .ok_or_else(|| malformed("leases"))?;
    leases
        .iter()
        .map(|lease| {
            Ok(Entry {
                id: text(lease, "id")?,
                state: text(lease, "state")?,
                host: lease["host"].as_str().map(str::to_owned),
                expires_at_ms: number(lease, "expires_at_ms")?,
                labels: lease["labels"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| {
                        (key.clone(), value.as_str().unwrap_or_default().to_owned())
                    })
                    .collect(),
            })
        })
        .collect()
}

/// A lease of a start's answer.
fn held(body: &Value) -> Result<Lease, Failure> {
    Ok(Lease {
        id: text(body, "id")?,
        host: text(body, "host")?,
        expires_at_ms: number(body, "expires_at_ms")?,
    })
}

/// A failure of the answer to a start, as the one that says the start may have created a
/// lease.
fn unconfirmed(failure: Failure) -> Failure {
    Failure::Unconfirmed(failure.to_string())
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
