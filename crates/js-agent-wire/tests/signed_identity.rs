//! The hello's signed identity (`Hello::identity`): its wire form, its bounds, and both
//! directions against the hellos of agents that predate it.
//!
//! The hellos from before the field are files: `fixtures/v0.1.135/hello.json` (see its
//! README) and `fixtures/agent/default_hello.json`, the hello `js-agent` sent with its
//! default flags before the field existed (written by the agent of that commit, from the
//! fake Mac host of its tests). The reader of the previous release is a copy of that
//! release's `Hello` (`v0.1.176`, whose `src/` is the same as this crate's before the
//! field), kept below as it was.

mod common;

use common::hello;
use js_agent_wire::limits::{MAX_IDENTITY_DOCUMENT_LEN, MAX_IDENTITY_SIGNATURE_LEN};
use js_agent_wire::{Hello, Rule, SignedIdentity, refusal};
use serde_json::{Value, json};

const RELEASE_HELLO: &str = include_str!("fixtures/v0.1.135/hello.json");
const AGENT_DEFAULT_HELLO: &str = include_str!("fixtures/agent/default_hello.json");

fn identity(document: &str, signature: &str) -> SignedIdentity {
    SignedIdentity {
        document: document.to_owned(),
        signature: signature.to_owned(),
    }
}

fn signed_hello(document: &str, signature: &str) -> Hello {
    Hello {
        identity: Some(identity(document, signature)),
        ..hello()
    }
}

/// The `Hello` of the previous release (`v0.1.176`), as far as its fields go: the field
/// types are this crate's, which that release shares. It has no `identity` and, as every
/// type of the wire, no `deny_unknown_fields`.
mod previous {
    use std::collections::BTreeMap;

    use js_agent_wire::{GpuDevice, HeldRun, InstanceIdentity, Phase, ProtocolRange, WireShape};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[allow(dead_code, reason = "the fields are decoded to show they still decode")]
    pub struct Hello {
        pub protocol: ProtocolRange,
        pub host_id: String,
        pub pool: String,
        pub agent_version: String,
        #[serde(default)]
        pub instance: Option<InstanceIdentity>,
        pub shape: WireShape,
        #[serde(default)]
        pub capabilities: BTreeMap<String, String>,
        #[serde(default)]
        pub gpu_devices: Vec<GpuDevice>,
        #[serde(default)]
        pub labels: BTreeMap<String, String>,
        pub phase: Phase,
        #[serde(default)]
        pub held: Vec<HeldRun>,
        #[serde(default)]
        pub accepts_host_credential: bool,
    }
}

/// Asserts that every hello from before the field (the previous release's golden hello
/// and the agent's own default hello) decodes with this version's type, passes its checks,
/// has no identity, and encodes back to the same JSON, so a hello without an identity is
/// sent as it was. It catches an `identity` without a default, which would make every
/// deployed agent's hello unreadable, and one encoded when absent.
#[test]
fn old_wire_fixtures_still_decode() {
    for (name, text) in [
        ("v0.1.135/hello.json", RELEASE_HELLO),
        ("agent/default_hello.json", AGENT_DEFAULT_HELLO),
    ] {
        let value: Value = serde_json::from_str(text).unwrap();
        let decoded: Hello = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.identity, None, "{name}");
        assert_eq!(decoded.validate(), Ok(()), "{name}");
        assert_eq!(serde_json::to_value(&decoded).unwrap(), value, "{name}");
    }
}

/// Asserts the exact JSON of a hello's identity: the object `{"document", "signature"}`
/// beside the other fields, both texts as given (a document over several lines, an empty
/// signature), and that it decodes back to the same hello. It catches a renamed field,
/// which an encode-decode round trip cannot see.
#[test]
fn the_identity_wire_form() {
    let sent = signed_hello("{\n  \"instanceId\" : \"i-0abc\"\n}", "");
    let value = serde_json::to_value(&sent).unwrap();
    assert_eq!(
        value["identity"],
        json!({"document": "{\n  \"instanceId\" : \"i-0abc\"\n}", "signature": ""})
    );
    let mut without = value.clone();
    without.as_object_mut().unwrap().remove("identity");
    assert_eq!(without, serde_json::to_value(hello()).unwrap());
    assert_eq!(serde_json::from_value::<Hello>(value).unwrap(), sent);
}

/// Asserts that an identity without its `signature`, or whose fields are not texts, is a
/// decode error, not an identity made up of defaults. It catches a reader that turns a
/// malformed identity into an empty one, which a provider would then be asked to verify.
#[test]
fn a_malformed_identity_does_not_decode() {
    let base = serde_json::to_value(hello()).unwrap();
    for bad in [
        json!({"document": "d"}),
        json!({"signature": "s"}),
        json!({"document": 1, "signature": "s"}),
        json!("d"),
    ] {
        let mut value = base.clone();
        value["identity"] = bad.clone();
        assert!(serde_json::from_value::<Hello>(value).is_err(), "{bad}");
    }
    let mut null = base;
    null["identity"] = Value::Null;
    let decoded: Hello = serde_json::from_value(null).unwrap();
    assert_eq!(decoded.identity, None);
}

/// Asserts that the previous release's reader decodes a hello that carries an identity,
/// with every other field as sent: a new agent's hello reaches an old server. It catches a
/// field an old reader would refuse.
#[test]
fn the_previous_release_reads_a_hello_with_an_identity() {
    let sent = signed_hello("doc", "sig");
    let read: previous::Hello = serde_json::from_value(serde_json::to_value(&sent).unwrap())
        .expect("the previous release decodes it");
    assert_eq!(read.host_id, sent.host_id);
    assert_eq!(read.shape, sent.shape);
    assert_eq!(read.held, sent.held);
    assert_eq!(read.labels, sent.labels);
}

/// Asserts the bounds of an identity: a document of one byte and one at
/// `MAX_IDENTITY_DOCUMENT_LEN`, an empty signature and one at `MAX_IDENTITY_SIGNATURE_LEN`,
/// and texts with line breaks are accepted; an empty document and either text one byte
/// over its cap are refused as an invalid hello naming the field. It catches a hello whose
/// identity is unbounded, and one that holds the cloud's bytes to the rules of free text.
#[test]
fn an_identity_is_held_to_its_bounds() {
    let document = "d".repeat(MAX_IDENTITY_DOCUMENT_LEN);
    let signature = "s".repeat(MAX_IDENTITY_SIGNATURE_LEN);
    for (doc, sig) in [
        ("d", ""),
        (document.as_str(), signature.as_str()),
        ("{\n\t\"a\": 1\n}\n", "c2ln\nbmF0dXJl\n"),
    ] {
        assert_eq!(signed_hello(doc, sig).validate(), Ok(()), "{}", doc.len());
    }
    let long_document = "d".repeat(MAX_IDENTITY_DOCUMENT_LEN + 1);
    let long_signature = "s".repeat(MAX_IDENTITY_SIGNATURE_LEN + 1);
    for (doc, sig, field, rule) in [
        ("", "", "identity.document", Rule::Empty),
        (
            long_document.as_str(),
            "",
            "identity.document",
            Rule::TooLong,
        ),
        (
            "d",
            long_signature.as_str(),
            "identity.signature",
            Rule::TooLong,
        ),
    ] {
        let err = signed_hello(doc, sig).validate().unwrap_err();
        assert_eq!(
            (err.field(), err.rule(), err.code()),
            (field, rule, refusal::INVALID_HELLO),
            "{field} {}",
            doc.len()
        );
    }
}
