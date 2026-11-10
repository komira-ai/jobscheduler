//! The parts of a refusal body the session reads: the `error` code, the majors served, the
//! capability at fault and the condition that failed. Each is held to the wire's rules before
//! it is kept; the server's text is never repeated as it came.

use js_agent_wire::{ProtocolRange, Refused};
use serde_json::Value;

/// The code shown when the server's refusal has none the agent can print.
const UNREADABLE_CODE: &str = "unreadable";

/// The `error` code of a refusal body, held to the wire's rules for a code; the server's
/// text is never repeated as it came.
pub(crate) fn code_of(body: &Value) -> String {
    let refused = Refused {
        code: body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        supported: None,
        field: None,
        reason: None,
    };
    match refused.validate() {
        Ok(()) => refused.code,
        Err(_) => UNREADABLE_CODE.to_owned(),
    }
}

/// The majors a refusal body names under `supported` (the server sends them with
/// `unsupported-protocol`), when they are a sane range and `code` is a plain code.
pub(crate) fn supported_of(body: &Value, code: &str) -> Option<ProtocolRange> {
    let supported = body
        .get("supported")
        .and_then(|range| serde_json::from_value::<ProtocolRange>(range.clone()).ok());
    let refused = Refused {
        code: code.to_owned(),
        supported,
        field: None,
        reason: None,
    };
    refused.validate().ok().and(refused.supported)
}

/// The capability a refusal body names under `field`, when it is a plain token.
pub(crate) fn field_of(body: &Value, code: &str) -> Option<String> {
    let refused = Refused {
        code: code.to_owned(),
        supported: None,
        field: body.get("field").and_then(Value::as_str).map(str::to_owned),
        reason: None,
    };
    refused.validate().ok().and(refused.field)
}

/// The reason a refusal body names under `reason`, when it is a plain token.
pub(crate) fn reason_of(body: &Value, code: &str) -> Option<String> {
    let refused = Refused {
        code: code.to_owned(),
        supported: None,
        field: None,
        reason: body
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    refused.validate().ok().and(refused.reason)
}
