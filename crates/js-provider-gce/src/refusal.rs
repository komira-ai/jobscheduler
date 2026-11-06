//! What a failed insert means for the acquire that made it. One table, one function: the
//! cloud's error reason (of the insert's error answer) or error code (of the zone operation
//! the insert started), never its message text, picks the constant.

use js_elastic::{Acquired, Reason, Refusal, RefusalCode};

use crate::error::ApiError;

/// The code a missing credential is refused under; the cloud never sends it.
const NO_CREDENTIAL: &str = "NoCredential";

/// The refusals: a reason or code and the constant it is.
const TABLE: [(&str, RefusalCode); 7] = [
    // The zone has no capacity for the machine type now (zone operation error codes,
    // Compute Engine "Troubleshooting resource availability errors").
    ("ZONE_RESOURCE_POOL_EXHAUSTED", RefusalCode::Capacity),
    (
        "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS",
        RefusalCode::Capacity,
    ),
    // The project's limits: instances, CPUs, accelerators and addresses all answer this
    // code, the metric named only in the message.
    ("QUOTA_EXCEEDED", RefusalCode::Quota),
    // The request does not fit: a field the cloud refuses (a machine type that cannot run
    // the image's architecture among them), and an image, machine type, network or zone
    // the project does not have (operation codes), or the same refused as the insert's own
    // answer (HTTP 400 `invalid`, HTTP 404 `notFound`).
    ("INVALID_FIELD_VALUE", RefusalCode::Shape),
    ("RESOURCE_NOT_FOUND", RefusalCode::Shape),
    ("invalid", RefusalCode::Shape),
    ("notFound", RefusalCode::Shape),
];

/// The answer of an acquire whose insert failed with `error`, or whose zone operation
/// finished with an error code (as `ApiError::answered` reads the code).
///
/// - `ZONE_RESOURCE_POOL_EXHAUSTED`, `ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS`: refused,
///   [`RefusalCode::Capacity`]; the engine falls through to its next option.
/// - `QUOTA_EXCEEDED`: refused, [`RefusalCode::Quota`]; the engine passes over the type.
/// - `INVALID_FIELD_VALUE`, `RESOURCE_NOT_FOUND`, `invalid`, `notFound`: refused,
///   [`RefusalCode::Shape`], a configuration error.
/// - [`ApiError::Denied`] (HTTP 401 and 403 that are not a rate limit: `forbidden`,
///   `PERMISSION_DENIED`, `authError`, `UNAUTHENTICATED`) and [`ApiError::NoCredential`]
///   (as `NoCredential`): refused, [`RefusalCode::Denied`].
/// - Everything else is [`Acquired::Unknown`]: `RESOURCE_OPERATION_RATE_EXCEEDED` (a rate
///   limit on operations, not a quota), a throttle (`rateLimitExceeded`,
///   `RATE_LIMIT_EXCEEDED`), an HTTP 5xx, any reason or code not listed, a transport error
///   or reset, a timeout, an answer that cannot be read and an incomplete one. The insert
///   may have landed, so nothing says it did not.
///
/// A refusal's reason is [`Reason::Api`] with the reason or code.
pub fn launch_answer(error: &ApiError) -> Acquired {
    let refused = |code: RefusalCode, reason: &str| {
        Acquired::Refused(Refusal {
            code,
            reason: Reason::Api(reason.to_owned()),
        })
    };
    match error {
        ApiError::Denied { reason } => refused(RefusalCode::Denied, reason.as_str()),
        ApiError::NoCredential => refused(RefusalCode::Denied, NO_CREDENTIAL),
        other => other
            .reason()
            .and_then(|reason| TABLE.iter().find(|(listed, _)| *listed == reason.as_str()))
            .map_or(Acquired::Unknown, |(listed, constant)| {
                refused(*constant, listed)
            }),
    }
}
