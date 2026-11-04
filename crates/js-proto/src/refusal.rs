//! A refused submit as a gRPC error: a [`Status`] whose code says what kind of refusal it
//! is and whose details hold the [`Refusal`].
//!
//! | refusal | gRPC code |
//! |---|---|
//! | `job-id-conflict` | `ALREADY_EXISTS` |
//! | `no-capable-host` | `FAILED_PRECONDITION` |
//! | `wall-time-not-positive` | `INVALID_ARGUMENT` |
//! | `wall-time-above-ceiling` | `INVALID_ARGUMENT` |
//!
//! The details are what gRPC carries in a status's details everywhere: an encoded
//! `google.rpc.Status` (code, message and a list of `google.protobuf.Any`), whose one
//! detail is the encoded [`Refusal`] under [`REFUSAL_TYPE_URL`]. A client in any language
//! reads the details with its own rich-error support, and a client that does not know a
//! refusal still gets an ordinary error with a code and a message.

use bytes::Bytes;
use prost::Message;
use prost_types::Any;
use tonic::{Code, Status};

use crate::v1::{Refusal, RefusalCode};

/// The wire form of `google.rpc.Status` (`proto/jobscheduler/rpc/status.proto`), generated
/// by the build script.
mod rpc {
    include!(concat!(env!("OUT_DIR"), "/jobscheduler.rpc.rs"));
}

use rpc::Status as RpcStatus;

/// The type URL under which a [`Refusal`] is packed in the details of a status: the type
/// name `jobscheduler.v1.Refusal` with the usual prefix.
pub const REFUSAL_TYPE_URL: &str = "type.googleapis.com/jobscheduler.v1.Refusal";

impl RefusalCode {
    /// The code's name: `job-id-conflict`, `no-capable-host`, `wall-time-not-positive` or
    /// `wall-time-above-ceiling`, and `unspecified` for the value that is no refusal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::JobIdConflict => "job-id-conflict",
            Self::NoCapableHost => "no-capable-host",
            Self::WallTimeNotPositive => "wall-time-not-positive",
            Self::WallTimeAboveCeiling => "wall-time-above-ceiling",
        }
    }

    /// The gRPC code a refusal of this kind is answered with (the table of the module).
    /// The value that is no refusal answers `UNKNOWN`.
    #[must_use]
    pub fn grpc_code(self) -> Code {
        match self {
            Self::Unspecified => Code::Unknown,
            Self::JobIdConflict => Code::AlreadyExists,
            Self::NoCapableHost => Code::FailedPrecondition,
            Self::WallTimeNotPositive | Self::WallTimeAboveCeiling => Code::InvalidArgument,
        }
    }
}

/// The status a server answers a refused submit with: the code's [`RefusalCode::grpc_code`],
/// the message `<code>: <detail>`, and the [`Refusal`] in the details.
#[must_use]
pub fn refusal_status(code: RefusalCode, detail: impl Into<String>) -> Status {
    let refusal = Refusal {
        code: code.into(),
        detail: detail.into(),
    };
    let grpc_code = code.grpc_code();
    let message = format!("{}: {}", code.as_str(), refusal.detail);
    let details = RpcStatus {
        code: grpc_code.into(),
        message: message.clone(),
        details: vec![Any {
            type_url: REFUSAL_TYPE_URL.to_owned(),
            value: refusal.encode_to_vec(),
        }],
    };
    Status::with_details(grpc_code, message, Bytes::from(details.encode_to_vec()))
}

/// The [`Refusal`] a status carries, if it carries one: `None` for a status without
/// details, with details that do not decode as a `google.rpc.Status`, with no detail of
/// [`REFUSAL_TYPE_URL`], or with one whose value does not decode as a [`Refusal`].
#[must_use]
pub fn refusal_of(status: &Status) -> Option<Refusal> {
    let details = RpcStatus::decode(status.details()).ok()?;
    let packed = details
        .details
        .into_iter()
        .find(|any| any.type_url == REFUSAL_TYPE_URL)?;
    Refusal::decode(packed.value.as_slice()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RefusalCode; 4] = [
        RefusalCode::JobIdConflict,
        RefusalCode::NoCapableHost,
        RefusalCode::WallTimeNotPositive,
        RefusalCode::WallTimeAboveCeiling,
    ];

    /// Asserts that the refusal codes are the four of design section 5.3, each with its
    /// kebab-case name, that no two share a name, and that the value that is no refusal
    /// has a name of its own.
    #[test]
    fn refusal_codes_are_exactly_the_four_of_the_design() {
        let names: Vec<&str> = ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            [
                "job-id-conflict",
                "no-capable-host",
                "wall-time-not-positive",
                "wall-time-above-ceiling",
            ]
        );
        let mut distinct = names.clone();
        distinct.push(RefusalCode::Unspecified.as_str());
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 5);
        assert_eq!(RefusalCode::Unspecified.as_str(), "unspecified");
        // The enum holds no fifth refusal: the value after the last is not a code.
        assert_eq!(
            RefusalCode::try_from(RefusalCode::WallTimeAboveCeiling as i32 + 1).ok(),
            None
        );
    }

    /// Asserts that each code, with its detail text, comes back unchanged from a status
    /// made by `refusal_status`, and that the status's message names the code.
    #[test]
    fn refusal_survives_a_status_round_trip() {
        for code in ALL {
            let detail = format!("detail of {}", code.as_str());
            let status = refusal_status(code, detail.clone());
            assert_eq!(
                refusal_of(&status),
                Some(Refusal {
                    code: code.into(),
                    detail: detail.clone(),
                })
            );
            assert_eq!(status.message(), format!("{}: {detail}", code.as_str()));
        }
    }

    /// Asserts that each refusal is answered with the gRPC code of the module's table, and
    /// that the details' own code agrees with the status's.
    #[test]
    fn each_refusal_uses_its_documented_grpc_code() {
        let expected = [
            (RefusalCode::JobIdConflict, Code::AlreadyExists),
            (RefusalCode::NoCapableHost, Code::FailedPrecondition),
            (RefusalCode::WallTimeNotPositive, Code::InvalidArgument),
            (RefusalCode::WallTimeAboveCeiling, Code::InvalidArgument),
            (RefusalCode::Unspecified, Code::Unknown),
        ];
        for (refusal, code) in expected {
            let status = refusal_status(refusal, "x");
            assert_eq!(status.code(), code, "{}", refusal.as_str());
            let details = RpcStatus::decode(status.details()).expect("details decode");
            assert_eq!(details.code, i32::from(code));
            assert_eq!(details.message, status.message());
        }
    }

    /// Asserts that a status with no details at all is not read as a refusal (an empty
    /// buffer decodes as an empty `google.rpc.Status`, which holds no refusal).
    #[test]
    fn status_without_details_is_not_a_refusal() {
        assert_eq!(refusal_of(&Status::already_exists("job-id-conflict")), None);
    }

    /// Asserts that details that do not decode, a `google.rpc.Status` without a refusal
    /// among its details, and a refusal detail whose value does not decode are each not
    /// read as a refusal, and that a refusal after a detail of another type is found.
    #[test]
    fn status_with_undecodable_details_is_not_a_refusal() {
        let with = |details: Vec<u8>| Status::with_details(Code::Unknown, "m", details.into());
        // A field of number 0 is never valid protobuf.
        assert_eq!(refusal_of(&with(vec![0x00, 0x01])), None);
        let other = Any {
            type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
            value: vec![0x0a, 0x01, b'x'],
        };
        let only_other = RpcStatus {
            code: 2,
            message: "m".to_owned(),
            details: vec![other.clone()],
        };
        assert_eq!(refusal_of(&with(only_other.encode_to_vec())), None);
        let broken = RpcStatus {
            details: vec![Any {
                type_url: REFUSAL_TYPE_URL.to_owned(),
                // A length-delimited field 2 that claims more bytes than follow.
                value: vec![0x12, 0x05, b'x'],
            }],
            ..only_other.clone()
        };
        assert_eq!(refusal_of(&with(broken.encode_to_vec())), None);
        let refusal = Refusal {
            code: RefusalCode::NoCapableHost.into(),
            detail: "after another detail".to_owned(),
        };
        let second = RpcStatus {
            details: vec![
                other,
                Any {
                    type_url: REFUSAL_TYPE_URL.to_owned(),
                    value: refusal.encode_to_vec(),
                },
            ],
            ..only_other
        };
        assert_eq!(refusal_of(&with(second.encode_to_vec())), Some(refusal));
    }
}
