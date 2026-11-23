//! The capability `full-disk-access` in a hello, against the wire of release `v0.1.208`,
//! which does not know it. Capabilities are a map of text, so the key is additive: a Mac
//! agent of this version reports it, a reader of that release keeps it as one more entry,
//! and a hello of that release, which has none, still decodes and validates here.
//!
//! The release's hello is a file under `fixtures/v0.1.208`, written by that tag's code. Its
//! reader is a copy of the fields a v0.1.208 server reads a hello with, kept below as it was.

use std::collections::BTreeMap;

use js_agent_wire::Hello;
use serde::Deserialize;
use serde_json::Value;

const HELLO_NATIVE: &str = include_str!("fixtures/v0.1.208/hello_native.json");

/// The fields a v0.1.208 server reads of a hello: the host, and the capabilities as a map
/// of text. Fields it does not know are ignored, as they were.
#[derive(Debug, Deserialize)]
struct PreviousHello {
    host_id: String,
    capabilities: BTreeMap<String, String>,
}

/// Asserts that a native hello of this version carrying `full-disk-access` in each of its
/// three states validates, round-trips through JSON, and is read by the previous release's
/// reader with the same host and every capability it knew unchanged beside the new entry;
/// and that the previous release's own native hello decodes here with no `full-disk-access`
/// and encodes back to the same JSON. Catches a key the previous release refuses, a check
/// here that refuses a hello without it (every deployed Mac agent would be unable to
/// register), and a capability that changes another one's value.
#[test]
fn the_capability_is_one_more_entry_for_the_previous_release_and_not_required_of_it() {
    let previous: Value = serde_json::from_str(HELLO_NATIVE).unwrap();
    let old: Hello = serde_json::from_value(previous.clone()).unwrap();
    assert_eq!(old.validate(), Ok(()));
    assert!(!old.capabilities.contains_key("full-disk-access"));
    assert_eq!(serde_json::to_value(&old).unwrap(), previous);
    for state in ["granted", "not-granted", "undetermined"] {
        let mut hello = old.clone();
        hello
            .capabilities
            .insert("full-disk-access".to_owned(), state.to_owned());
        assert_eq!(hello.validate(), Ok(()), "{state}");
        let wire = serde_json::to_value(&hello).unwrap();
        assert_eq!(
            serde_json::from_value::<Hello>(wire.clone()).unwrap(),
            hello
        );
        let read: PreviousHello = serde_json::from_value(wire).unwrap();
        assert_eq!(read.host_id, "mac-01");
        assert_eq!(read.capabilities["full-disk-access"], state);
        let mut rest = read.capabilities;
        rest.remove("full-disk-access");
        assert_eq!(rest, old.capabilities);
    }
}
