//! What a failed `RunInstances` means for the acquire that made it. One table, one
//! function: the cloud's error code, never its message text and never the HTTP status
//! (EC2 sends some capacity refusals as server errors), picks the constant.

use js_elastic::{Acquired, Reason, Refusal, RefusalCode};

use crate::error::{ApiError, ErrorCode};

/// The code a missing credential is refused under; the cloud never sends it.
const NO_CREDENTIAL: &str = "NoCredential";

/// The refusals: an error code and the constant it is.
const TABLE: [(&str, RefusalCode); 13] = [
    // The zone has no capacity for the type now, or the spot market is above the price.
    ("InsufficientInstanceCapacity", RefusalCode::Capacity),
    ("SpotMaxPriceTooLow", RefusalCode::Capacity),
    // The account's limits.
    ("InstanceLimitExceeded", RefusalCode::Quota),
    ("VcpuLimitExceeded", RefusalCode::Quota),
    ("MaxSpotInstanceCountExceeded", RefusalCode::Quota),
    // The request does not fit what the account or the zone has.
    ("InvalidAMIID.NotFound", RefusalCode::Shape),
    ("InvalidParameterValue", RefusalCode::Shape),
    ("Unsupported", RefusalCode::Shape),
    ("InsufficientFreeAddressesInSubnet", RefusalCode::Shape),
    // The credential may not launch, or there is none.
    ("UnauthorizedOperation", RefusalCode::Denied),
    ("AuthFailure", RefusalCode::Denied),
    ("OptInRequired", RefusalCode::Denied),
    (NO_CREDENTIAL, RefusalCode::Denied),
];

/// The answer of an acquire whose `RunInstances` failed with `error`.
///
/// - `InsufficientInstanceCapacity`, `SpotMaxPriceTooLow`: refused, [`RefusalCode::Capacity`].
/// - `InstanceLimitExceeded`, `VcpuLimitExceeded`, `MaxSpotInstanceCountExceeded`: refused,
///   [`RefusalCode::Quota`].
/// - `InvalidAMIID.NotFound`, `InvalidParameterValue`, `Unsupported`,
///   `InsufficientFreeAddressesInSubnet`: refused, [`RefusalCode::Shape`].
/// - `UnauthorizedOperation`, `AuthFailure`, `OptInRequired`, and [`ApiError::NoCredential`]
///   (as `NoCredential`): refused, [`RefusalCode::Denied`].
/// - Everything else is [`Acquired::Unknown`]: `RequestLimitExceeded`, `Throttling`,
///   `ServiceUnavailable`, `InternalError`, any code not listed, a transport error, a
///   timeout, an answer that cannot be read and an incomplete one. The launch may have
///   landed, so nothing says it did not.
///
/// A refusal's reason is [`Reason::Api`] with the code.
pub fn launch_answer(error: &ApiError) -> Acquired {
    let code = match error {
        ApiError::NoCredential => Some(NO_CREDENTIAL),
        other => other.code().map(ErrorCode::as_str),
    };
    code.and_then(|code| TABLE.iter().find(|(listed, _)| *listed == code))
        .map_or(Acquired::Unknown, |(listed, constant)| {
            Acquired::Refused(Refusal {
                code: *constant,
                reason: Reason::Api((*listed).to_owned()),
            })
        })
}
