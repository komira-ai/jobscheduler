//! The `actions_oom_events` of a heartbeat's load: the `oom` count of the actions cgroup's
//! `memory.events.local`. That it is optional on the wire across releases, and that a value
//! that is not a number reads as absent.

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_cpu_pressure_some_usec: Option<u64>,
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

fn with_oom(oom: Option<u64>) -> NodeLoad {
    NodeLoad {
        actions_oom_events: oom,
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
    text["actions_oom_events"] = value;
    read_text(text)
}

/// Asserts a load without the count writes no key and reads back with none, and one with it
/// (0 included) writes the key and reads back the same load.
#[test]
fn actions_oom_events_is_optional_on_the_wire() {
    let absent = serde_json::to_value(with_oom(None)).unwrap();
    assert!(absent.get("actions_oom_events").is_none(), "{absent}");
    assert_eq!(read_text(absent), with_oom(None));

    for count in [0, 7] {
        let held = with_oom(Some(count));
        let text = serde_json::to_value(&held).unwrap();
        assert_eq!(text["actions_oom_events"], json!(count));
        assert_eq!(read_text(text), held);
    }
}

/// Asserts the load of a heartbeat reads the count as a number, and reads a value that is not
/// a number (text, negative, fractional, null) as absent while the rest of the load stands,
/// and that the counter vets at `u64::MAX`.
#[test]
fn a_count_that_is_not_a_number_reads_as_absent_and_any_number_vets() {
    assert_eq!(read_with(json!(123)), with_oom(Some(123)));
    for bad in [json!("1"), json!(-1), json!(1.5), Value::Null] {
        assert_eq!(read_with(bad.clone()), with_oom(None), "{bad}");
    }
    assert_eq!(with_oom(Some(u64::MAX)).vet(), Ok(()));
}

/// Asserts the release now deployed, with its own copy of `NodeLoad`, reads a load that carries
/// the count (beside the pressure and the per-second lists that release knows) as the same
/// load without it; that the lenient decoder, whose reading of every other key that release
/// shares, skips a key it does not know and keeps the rest of the load; and that this release
/// reads a load that release wrote (no count) as a load without one.
#[test]
fn the_released_server_and_this_one_read_each_others_loads() {
    let to_released = |load: &NodeLoad| -> ReleasedLoad {
        serde_json::from_value(serde_json::to_value(load).unwrap()).unwrap()
    };
    let full = |oom| NodeLoad {
        host_cpu_pressure_some_usec: Some(40),
        second_cpu_millis: vec![1000, 2000],
        second_cpu_pressure_some_permille: vec![10, 20],
        ..with_oom(oom)
    };
    let released = to_released(&full(Some(9)));
    assert_eq!(released, to_released(&full(None)));
    assert_eq!(released.host_cpu_pressure_some_usec, Some(40));
    assert_eq!(released.second_cpu_millis, [1000, 2000]);

    let mut later = serde_json::to_value(full(Some(9))).unwrap();
    later["a_count_of_a_later_release"] = json!(5);
    assert_eq!(read_text(later), full(Some(9)));

    let written = serde_json::to_value(to_released(&load())).unwrap();
    assert!(written.get("actions_oom_events").is_none());
    assert_eq!(read_text(written), with_oom(None));
}
