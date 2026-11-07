//! The boot payload: delivered as it was rendered, never logged, never over the limit.

use js_elastic::{
    AcquireRequest, Acquired, BootPayload, CapacityProvider, Reason, Refusal, RefusalCode,
};
use js_provider_gce::PAYLOAD_LIMIT;
use tracing::Level;

use crate::rig::{ask, live, logged, rig};

/// A payload with a join token on its own line.
const RENDERED: &str = "#!/bin/sh\nJS_JOIN_TOKEN=join-MARKER-9f3a\necho \"é\"\n";

fn with_payload(acquire: &str, payload: String) -> AcquireRequest {
    AcquireRequest {
        payload: BootPayload::new(payload),
        ..ask(acquire)
    }
}

/// Asserts that the metadata item of the insert is the payload byte for byte, a join-token
/// line and a multi-byte character included; that no line of the log at trace level carries
/// any of it; that its `Debug` text says how many bytes there are and nothing else; that
/// the limit is 256 KiB, one metadata value; that a payload of exactly the limit is
/// inserted and one byte more is refused as a shape error naming its size and the limit,
/// with nothing sent. It catches a payload that is trimmed, re-encoded or logged, and a
/// limit that is off by one.
#[test]
fn the_boot_payload_is_delivered_as_rendered() {
    let lab = rig(live(2));
    let (answer, lines) = logged(Level::TRACE, || {
        lab.provider
            .acquire(&with_payload("a1", RENDERED.to_owned()))
    });
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    let sent = lab.account.inserts();
    assert_eq!(sent[0].payload.expose(), RENDERED);
    assert_eq!(sent[0].payload.len(), RENDERED.len());
    assert!(lines.contains("inserting one instance"), "{lines}");
    for secret in ["MARKER", "JS_JOIN_TOKEN", "#!/bin/sh"] {
        assert!(!lines.contains(secret), "{secret} in {lines}");
    }
    assert_eq!(
        format!("{:?}", sent[0].payload),
        format!("Payload({} bytes)", RENDERED.len())
    );

    assert_eq!(PAYLOAD_LIMIT, 256 * 1024);
    assert_eq!(lab.provider.payload_limit(), 256 * 1024);
    let over = with_payload("a2", "x".repeat(PAYLOAD_LIMIT + 1));
    assert_eq!(
        lab.provider.acquire(&over),
        Acquired::Refused(Refusal {
            code: RefusalCode::Shape,
            reason: Reason::PayloadTooLarge {
                len: PAYLOAD_LIMIT + 1,
                limit: PAYLOAD_LIMIT
            }
        })
    );
    assert_eq!(lab.account.inserted(), 1);
    let exact = with_payload("a3", "x".repeat(PAYLOAD_LIMIT));
    assert!(matches!(
        lab.provider.acquire(&exact),
        Acquired::Accepted { .. }
    ));
    assert_eq!(lab.account.inserts()[1].payload.len(), PAYLOAD_LIMIT);
}
