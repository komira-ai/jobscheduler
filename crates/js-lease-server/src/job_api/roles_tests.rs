//! The rules of the roles that need no service: the outcome a log line records for each
//! answer, and a fleet call's gate.

use js_proto::refusal::refusal_status;
use js_proto::v1::RefusalCode;
use tonic::{Code, Response, Status};

use super::*;

/// A caller named `name` of `role`.
fn caller(name: &str, role: Role) -> Caller {
    Caller {
        name: ClientName::parse(name).unwrap(),
        role,
    }
}

/// The outcome of an answer that is `status`.
fn of(status: Status) -> &'static str {
    outcome::<()>(&Err(status))
}

/// Asserts the outcome of every gRPC code, of a hidden job and of a refusal: `ok`, `denied`
/// for a permission refusal, an unauthenticated call and a job of another client (although
/// its wire answer is `NOT_FOUND` `unknown-job`, the same as a job with no row), `not-found`
/// for a plain `NOT_FOUND`, `refused` for a fault of the request, `failed` for the rest.
/// Catches a denial logged as not found, a refusal logged as a server failure, and a hidden
/// job whose answer differs on the wire from an unknown one.
#[test]
fn every_answer_has_its_outcome() {
    assert_eq!(outcome(&Ok(Response::new(()))), "ok");
    let hidden = hidden();
    assert_eq!(
        (hidden.code(), hidden.message()),
        (Code::NotFound, "unknown-job")
    );
    let mark = hidden.source().map(ToString::to_string);
    assert_eq!(mark.as_deref(), Some("the job is another client's"));
    assert_eq!(of(hidden), "denied");
    assert_eq!(of(Status::not_found("unknown-job")), "not-found");
    let conflict = refusal_status(RefusalCode::JobIdConflict, "x");
    assert_eq!(of(conflict), "refused");
    let expected = [
        (Code::Ok, "ok"),
        (Code::Cancelled, "failed"),
        (Code::Unknown, "failed"),
        (Code::InvalidArgument, "refused"),
        (Code::DeadlineExceeded, "failed"),
        (Code::NotFound, "not-found"),
        (Code::AlreadyExists, "refused"),
        (Code::PermissionDenied, "denied"),
        (Code::ResourceExhausted, "failed"),
        (Code::FailedPrecondition, "refused"),
        (Code::Aborted, "failed"),
        (Code::OutOfRange, "refused"),
        (Code::Unimplemented, "failed"),
        (Code::Internal, "failed"),
        (Code::Unavailable, "failed"),
        (Code::DataLoss, "failed"),
        (Code::Unauthenticated, "denied"),
    ];
    for (code, said) in expected {
        assert_eq!(of(Status::new(code, "m")), said, "{code:?}");
    }
}

/// Asserts that only an operator passes the fleet gate: a client is `PERMISSION_DENIED`
/// `operator-only`, a request no bearer layer admitted `UNAUTHENTICATED` with an empty
/// message; and that a client's listing is held to its own name while an operator's is not.
/// Catches a client let through to the fleet, a gate that admits a call with no caller, and
/// an operator's listing cut to its own jobs.
#[test]
fn only_an_operator_passes_the_fleet_gate() {
    let ops = caller("ops", Role::Operator);
    let ci = caller("ci", Role::Client);
    assert!(ops.operator().is_ok());
    let denied = ci.operator().unwrap_err();
    assert_eq!(
        (denied.code(), denied.message()),
        (Code::PermissionDenied, OPERATOR_ONLY)
    );
    let none = admitted(None).unwrap_err();
    assert_eq!((none.code(), none.message()), (Code::Unauthenticated, ""));
    assert_eq!(admitted(Some(&ci)).unwrap(), &ci);
    assert_eq!(ci.only_jobs_of(), Some(&ci.name));
    assert_eq!(ops.only_jobs_of(), None);
}
