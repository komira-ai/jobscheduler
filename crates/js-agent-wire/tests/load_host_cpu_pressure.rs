//! The `host_cpu_pressure_some_usec` of a heartbeat's load: the `some` total of the host's
//! `/proc/pressure/cpu`. That it is optional on the wire across releases, that a value that is
//! not a number reads as absent, and that the counter has no bound of its own.

mod common;

use common::{heartbeat, load};
use js_agent_wire::{Heartbeat, NodeLoad};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The `NodeLoad` of the release before this field, kept as it was: it stands for the servers
/// and agents of the release now deployed, and must not follow later changes to [`NodeLoad`].
/// Fields it does not know are skipped, as they were.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ReleasedLoad {
    seq: u64,
    mono_usec: u64,
    cpu_usage_usec: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_cpu_usec: Option<u64>,
    working_set_bytes: u64,
    #[serde(default)]
    active_file_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_nonfile_bytes: Option<u64>,
    #[serde(default)]
    actions_cpu_limited: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    covered: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clocks: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    minutes: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    second_cpu_millis: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    second_cpu_pressure_some_permille: Vec<u16>,
}

fn with_pressure(pressure: Option<u64>) -> NodeLoad {
    NodeLoad {
        host_cpu_pressure_some_usec: pressure,
        ..load()
    }
}

/// The load a heartbeat carrying the load text `text` reads as.
fn read_text(text: Value) -> NodeLoad {
    let mut body = serde_json::to_value(heartbeat()).unwrap();
    body["load"] = text;
    let beat: Heartbeat = serde_json::from_value(body).unwrap();
    assert_eq!(beat.validate(), Ok(()));
    beat.load.expect("the load survives")
}

/// The load the fixture's load text reads as with the key set to `value`.
fn read_with(value: Value) -> NodeLoad {
    let mut text = serde_json::to_value(load()).unwrap();
    text["host_cpu_pressure_some_usec"] = value;
    read_text(text)
}

/// Asserts a load without the counter writes no key and reads back with none, and one with it
/// writes the key and reads back the same load. Catches a `null` written for an absent counter
/// and a key renamed.
#[test]
fn host_cpu_pressure_some_usec_is_optional_on_the_wire() {
    let absent = serde_json::to_value(with_pressure(None)).unwrap();
    assert!(
        absent.get("host_cpu_pressure_some_usec").is_none(),
        "{absent}"
    );
    assert_eq!(read_text(absent), with_pressure(None));

    let held = with_pressure(Some(4_321));
    let text = serde_json::to_value(&held).unwrap();
    assert_eq!(text["host_cpu_pressure_some_usec"], json!(4_321));
    assert_eq!(read_text(text), held);
}

/// Asserts the load of a heartbeat reads the counter as a number, and reads a value that is not
/// a number (text, negative, fractional, null) as absent while the rest of the load stands; and
/// that the counter, which counts from boot, vets at `u64::MAX`. Catches a bad counter costing
/// the load, and a bound that refuses a host up long enough.
#[test]
fn a_counter_that_is_not_a_number_reads_as_absent_and_any_number_vets() {
    assert_eq!(read_with(json!(123)), with_pressure(Some(123)));
    for bad in [json!("1"), json!(-1), json!(1.5), Value::Null] {
        assert_eq!(read_with(bad.clone()), with_pressure(None), "{bad}");
    }
    assert_eq!(with_pressure(Some(u64::MAX)).vet(), Ok(()));
    assert_eq!(with_pressure(Some(0)).vet(), Ok(()));
}

/// Asserts the release now deployed, with its own copy of `NodeLoad`, reads a load that carries
/// the counter as the same load without it, and that this release reads a load that release
/// wrote (no counter) as a load without one, which the scheduler reads as not measured. Catches
/// a field an older server cannot skip, and one this release demands of an older agent.
#[test]
fn the_released_server_and_this_one_read_each_others_loads() {
    let to_released = |load: &NodeLoad| -> ReleasedLoad {
        serde_json::from_value(serde_json::to_value(load).unwrap()).unwrap()
    };
    let new = with_pressure(Some(9_000_000_000));
    assert_eq!(to_released(&new), to_released(&with_pressure(None)));

    let written = serde_json::to_value(to_released(&load())).unwrap();
    assert!(written.get("host_cpu_pressure_some_usec").is_none());
    assert_eq!(read_text(written), with_pressure(None));
}
