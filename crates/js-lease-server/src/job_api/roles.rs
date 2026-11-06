//! Who may call what on the job service: the [`Caller`] the bearer layer names for a request,
//! the rules of its [`Role`], and the outcome a call's log line records.
//!
//! | call | `client` | `operator` |
//! |---|---|---|
//! | `SubmitJob` | yes; a retry of a job id another client holds is `job-id-conflict` | yes |
//! | `GetJob`, `GetResult`, `GetLogs`, `CancelJob` | its own jobs; any other job is `unknown-job`, as one with no row | every job |
//! | `ListJobs` | its own jobs only, whatever the filter | every job |
//! | `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools`, `ListProviders`, `GetUsage` | `PERMISSION_DENIED` `operator-only` (403) | yes |
//!
//! A client's own jobs are those recorded with its name; a job recorded with no client (before
//! clients were named) is no client's, so only an operator reads or cancels it. A job of
//! another client is answered exactly as a job id with no row, so a client learns nothing of
//! the jobs it does not own; only the call's log line ([`outcome`]) says `denied`.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use js_jobs::{ClientName, JobRecord};
use tonic::{Code, Request, Response, Status};

pub(crate) use crate::config::Role;

/// The message of the status a client's fleet call answers.
pub(crate) const OPERATOR_ONLY: &str = "operator-only";

/// The client a request was admitted as: its name and its role, from its token alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Caller {
    /// The client's name.
    pub(crate) name: ClientName,
    /// What it may call.
    pub(crate) role: Role,
}

impl Caller {
    /// Whether this caller may read or cancel `job`: an operator any job, a client the jobs
    /// recorded with its name.
    pub(crate) fn owns(&self, job: &JobRecord) -> bool {
        self.role == Role::Operator || job.client.as_ref() == Some(&self.name)
    }

    /// The client whose jobs alone a listing of this caller may show: its own name for a
    /// client, none for an operator.
    pub(crate) fn only_jobs_of(&self) -> Option<&ClientName> {
        match self.role {
            Role::Client => Some(&self.name),
            Role::Operator => None,
        }
    }

    /// `Ok` for an operator; `PERMISSION_DENIED` `operator-only` for a client.
    pub(crate) fn operator(&self) -> Result<(), Status> {
        match self.role {
            Role::Operator => Ok(()),
            Role::Client => Err(Status::permission_denied(OPERATOR_ONLY)),
        }
    }
}

/// The caller the bearer layer named for `request`; `None` for a request that did not pass
/// it.
pub(crate) fn caller_of<T>(request: &Request<T>) -> Option<Caller> {
    request.extensions().get::<Caller>().cloned()
}

/// `caller`, or the answer to a request that no bearer layer admitted: `UNAUTHENTICATED`
/// with an empty message, as the layer answers one itself. Every request that reaches the
/// service over a transport has a caller; this refuses a call made any other way.
pub(crate) fn admitted(caller: Option<&Caller>) -> Result<&Caller, Status> {
    caller.ok_or_else(|| Status::unauthenticated(""))
}

/// The mark of an answer that hides a job of another client: the wire sees `unknown-job`,
/// and only the log line reads the mark.
#[derive(Debug)]
struct Hidden;

impl fmt::Display for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the job is another client's")
    }
}

impl Error for Hidden {}

/// The answer to a call of a client on a job it does not own: `NOT_FOUND` `unknown-job`, the
/// answer to a job id with no row, marked so that the call's log line says `denied`. The mark
/// is the status's source, which neither transport sends.
pub(crate) fn hidden() -> Status {
    let mut status = Status::not_found("unknown-job");
    status.set_source(Arc::new(Hidden));
    status
}

/// What a call's log line says of `answer`: `ok`; `denied` (a fleet call of a client, a job
/// of another client, a request no bearer layer admitted); `not-found`; `refused` (the
/// request is at fault: an argument, a conflict, a precondition, a size); or `failed` (the
/// server could not answer: a store, a save, a call not served yet).
pub(crate) fn outcome<T>(answer: &Result<Response<T>, Status>) -> &'static str {
    let Err(status) = answer else {
        return "ok";
    };
    if status.source().is_some_and(|source| source.is::<Hidden>()) {
        return "denied";
    }
    match status.code() {
        Code::Ok => "ok",
        Code::PermissionDenied | Code::Unauthenticated => "denied",
        Code::NotFound => "not-found",
        Code::InvalidArgument
        | Code::AlreadyExists
        | Code::FailedPrecondition
        | Code::OutOfRange => "refused",
        Code::Cancelled
        | Code::Unknown
        | Code::DeadlineExceeded
        | Code::ResourceExhausted
        | Code::Aborted
        | Code::Unimplemented
        | Code::Internal
        | Code::Unavailable
        | Code::DataLoss => "failed",
    }
}

/// `request` as the bearer layer passes on a request of the operator `default`, for a test
/// that calls the service directly.
#[cfg(test)]
pub(crate) fn as_operator<T>(mut request: Request<T>) -> Request<T> {
    request.extensions_mut().insert(Caller {
        name: ClientName::default_client(),
        role: Role::Operator,
    });
    request
}

#[cfg(test)]
#[path = "roles_tests.rs"]
mod tests;
