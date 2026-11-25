//! The `host_nonfile_bytes` of a heartbeat's load: the whole host's non-file memory. That it is
//! optional on the wire across releases, that a value that is not a number reads as absent, its
//! range, and which memory a receiver reads as the host's.

mod common;

use common::{expect, heartbeat, load};
use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_agent_wire::{Heartbeat, NodeLoad, Rule, refusal};
use serde_json::{Value, json};

fn with_nonfile(held: Option<u64>) -> NodeLoad {
    NodeLoad {
        host_nonfile_bytes: held,
        ..load()
    }
}

/// The load of a heartbeat whose load text is `load` with `host_nonfile_bytes` set to `value`.
fn read_with(value: Value) -> Option<NodeLoad> {
    let mut text = serde_json::to_value(load()).unwrap();
    text["host_nonfile_bytes"] = value;
    read_text(text)
}

/// The load a heartbeat carrying the load text `text` reads as.
fn read_text(text: Value) -> Option<NodeLoad> {
    let mut body = serde_json::to_value(heartbeat()).unwrap();
    body["load"] = text;
    let beat: Heartbeat = serde_json::from_value(body).unwrap();
    beat.load
}

/// Asserts a load without the figure writes no key and reads back with none, and one with it
/// writes the key and reads back the same load. Catches a `null` written for an absent figure
/// and a key renamed.
#[test]
fn host_nonfile_bytes_is_optional_on_the_wire() {
    let absent = serde_json::to_value(with_nonfile(None)).unwrap();
    assert!(absent.get("host_nonfile_bytes").is_none(), "{absent}");
    assert_eq!(read_text(absent), Some(with_nonfile(None)));

    let held = with_nonfile(Some(5 << 30));
    let text = serde_json::to_value(&held).unwrap();
    assert_eq!(text["host_nonfile_bytes"], json!(5u64 << 30));
    assert_eq!(read_text(text), Some(held));
}

/// Asserts the load of a heartbeat reads the figure as a number, and reads a value that is not a
/// number (text, negative, fractional, null) as absent while the rest of the load stands.
/// Catches a bad figure costing the load.
#[test]
fn a_figure_that_is_not_a_number_reads_as_absent() {
    assert_eq!(read_with(json!(123)), Some(with_nonfile(Some(123))));
    for bad in [json!("1"), json!(-1), json!(1.5), Value::Null] {
        assert_eq!(read_with(bad.clone()), Some(with_nonfile(None)), "{bad}");
    }
}

/// Asserts a figure at `MAX_MEMORY_BYTES` vets and one above is refused as out of range on
/// `heartbeat.load.host_nonfile_bytes`, and that an absent figure vets. Catches `<` for `<=`
/// and a rule on the wrong field.
#[test]
fn host_nonfile_bytes_is_bounded() {
    assert_eq!(with_nonfile(None).vet(), Ok(()));
    assert_eq!(with_nonfile(Some(MAX_MEMORY_BYTES)).vet(), Ok(()));
    let err = with_nonfile(Some(MAX_MEMORY_BYTES + 1)).vet().unwrap_err();
    expect(
        err,
        "heartbeat.load.host_nonfile_bytes",
        Rule::OutOfRange,
        refusal::INVALID_HEARTBEAT,
    );
}

/// Asserts the memory a receiver reads is the host's non-file figure with no active file pages
/// when the agent sends it, and the actions cgroup's working set and active file otherwise;
/// including a figure of 0, which is sent and so read. Catches the fallback used when the figure
/// is present and the other way round.
#[test]
fn node_memory_is_the_host_figure_when_sent_else_the_working_set() {
    let (working_set, active_file) = (3u64 << 30, 1u64 << 20);
    assert_eq!(
        with_nonfile(None).node_memory_bytes(),
        (working_set, active_file)
    );
    assert_eq!(with_nonfile(Some(7)).node_memory_bytes(), (7, 0));
    assert_eq!(with_nonfile(Some(0)).node_memory_bytes(), (0, 0));
}
