//! The two retired names, `Hello.accepts_host_credential` and `Welcome.host_credential`:
//! a body from an older peer that still carries them decodes, and nothing this version
//! encodes carries them.

mod common;

use common::{hello, welcome};
use js_agent_wire::{Hello, Welcome};
use serde_json::json;

const SECRET: &str = "c3ed1c0f0000000000000000000000000000000000000000000000000000beef";

/// A hello from an agent that still says `accepts_host_credential`, and a welcome from a
/// server that still sends a `host_credential`, decode to the same values as the bodies
/// without them, and the welcome still passes the agent's checks. It catches a decoder that
/// refuses an unknown member of a body an older peer sent.
#[test]
fn a_body_that_still_carries_the_retired_names_decodes_and_the_names_are_ignored() {
    let mut old_hello = serde_json::to_value(hello()).unwrap();
    old_hello["accepts_host_credential"] = json!(true);
    let decoded: Hello = serde_json::from_value(old_hello).unwrap();
    assert_eq!(decoded, hello());
    assert_eq!(decoded.validate(), Ok(()));

    let mut old_welcome = serde_json::to_value(welcome()).unwrap();
    old_welcome["host_credential"] = json!(SECRET);
    let decoded: Welcome = serde_json::from_value(old_welcome).unwrap();
    assert_eq!(decoded, welcome());
    assert_eq!(decoded.validate(), Ok(()));
}

/// A hello and a welcome encode without either retired name, so an older peer reads them
/// as the bodies it always read when no credential was asked for or issued. It catches a
/// retired member written back out.
#[test]
fn a_hello_and_a_welcome_no_longer_encode_the_retired_names() {
    let sent = serde_json::to_value(hello()).unwrap();
    assert!(sent.get("accepts_host_credential").is_none(), "{sent}");
    let sent = serde_json::to_value(welcome()).unwrap();
    assert!(sent.get("host_credential").is_none(), "{sent}");
}
