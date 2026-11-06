//! The boot payload: delivered as it was rendered, never logged, never over the limit.

use js_elastic::{AcquireRequest, Acquired, BootPayload, Reason, Refusal, RefusalCode};
use js_provider_ec2::PAYLOAD_LIMIT;
use tracing::Level;

use crate::rig::{ask, live, logged, rig};

/// A payload with a join token on its own line.
const RENDERED: &str = "#!/bin/sh\nJS_JOIN_TOKEN=tskey-auth-MARKER-9f3a\necho \"é\"\n";

fn with_payload(acquire: &str, payload: String) -> AcquireRequest {
    AcquireRequest {
        payload: BootPayload::new(payload),
        ..ask(acquire)
    }
}

/// Asserts that the user data of the launch is the payload byte for byte, a join-token line
/// and a multi-byte character included, and that no line of the log at trace level carries
/// any of it. It catches a payload that is trimmed, re-encoded or logged.
#[test]
fn the_boot_payload_is_delivered_as_rendered() {
    let rig = rig(live(1));
    let (answer, lines) = logged(Level::TRACE, || {
        rig.provider
            .acquire(&with_payload("a1", RENDERED.to_owned()))
    });
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    let sent = rig.account.run_requests();
    assert_eq!(sent[0].user_data.expose(), RENDERED.as_bytes());
    assert!(lines.contains("launching one instance"), "{lines}");
    for secret in ["MARKER", "JS_JOIN_TOKEN", "#!/bin/sh"] {
        assert!(!lines.contains(secret), "{secret} in {lines}");
    }
    assert_eq!(
        format!("{:?}", sent[0].user_data),
        format!("UserData({} bytes)", RENDERED.len())
    );
}

/// Asserts that a payload over the limit the cloud takes as user data is refused as a shape
/// error naming its size and the limit, with nothing sent, and that one of exactly the limit
/// is launched.
#[test]
fn an_oversized_payload_is_refused_and_one_at_the_limit_is_launched() {
    let rig = rig(live(2));
    let over = with_payload("a1", "x".repeat(PAYLOAD_LIMIT + 1));
    assert_eq!(
        rig.provider.acquire(&over),
        Acquired::Refused(Refusal {
            code: RefusalCode::Shape,
            reason: Reason::PayloadTooLarge {
                len: PAYLOAD_LIMIT + 1,
                limit: PAYLOAD_LIMIT
            }
        })
    );
    assert!(rig.account.actions().is_empty());
    let exact = with_payload("a2", "x".repeat(PAYLOAD_LIMIT));
    assert!(matches!(
        rig.provider.acquire(&exact),
        Acquired::Accepted { .. }
    ));
    assert_eq!(rig.account.run_requests()[0].user_data.len(), PAYLOAD_LIMIT);
}
