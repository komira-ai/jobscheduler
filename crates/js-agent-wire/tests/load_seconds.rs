//! The per-second lists of a heartbeat's load, `second_cpu_millis` and
//! `second_cpu_pressure_some_permille`: their wire form, that a bad value or element is
//! dropped and counted without costing the beat, their caps, and that a load with them and a
//! load without them are read by the release now deployed and by this one.

mod common;

use common::{heartbeat, load};
use js_agent_wire::limits::*;
use js_agent_wire::{Heartbeat, LeaseClock, LeaseMinute, LoadDrops, NodeLoad};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The `NodeLoad` of the release before the two lists, kept as it was: it stands for the
/// servers and agents of the release now deployed, and must not follow later changes to
/// [`NodeLoad`]. Fields it does not know are skipped, as they were.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ReleasedLoad {
    seq: u64,
    mono_usec: u64,
    cpu_usage_usec: u64,
    working_set_bytes: u64,
    #[serde(default)]
    active_file_bytes: u64,
    #[serde(default)]
    actions_cpu_limited: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    covered: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clocks: Vec<ReleasedClock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    minutes: Vec<ReleasedMinute>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ReleasedClock {
    lease_id: String,
    started_mono_usec: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ReleasedMinute {
    lease_id: String,
    minute: u16,
    cpu_mean_millis: u32,
    cpu_peak_window_millis: u32,
    ws_max_bytes: u64,
}

/// The bytes the two lists add to a load at their widest: `,"second_cpu_millis":[` 22, 16
/// values of 10 digits and 15 commas 175, `]` 1; `,"second_cpu_pressure_some_permille":[`
/// 38, 16 values of 4 digits and 15 commas 79, `]` 1.
const WIDEST_SECONDS: usize = (22 + 175 + 1) + (38 + 79 + 1);

fn with_seconds(millis: Vec<u32>, pressure: Vec<u16>) -> NodeLoad {
    NodeLoad {
        second_cpu_millis: millis,
        second_cpu_pressure_some_permille: pressure,
        ..load()
    }
}

/// A heartbeat body whose load is the fixture's with `key` set to `value`.
fn body_with(key: &str, value: Value) -> Value {
    let mut body = serde_json::to_value(heartbeat()).unwrap();
    body["load"] = serde_json::to_value(load()).unwrap();
    body["load"][key] = value;
    body
}

fn decoded(body: Value) -> NodeLoad {
    let beat: Heartbeat = serde_json::from_value(body).unwrap();
    assert_eq!(beat.validate(), Ok(()));
    beat.load.expect("the load survives")
}

/// Asserts the exact text of the two lists: their keys, the values in order, after
/// `minutes`; that each reads back as the same load; and that an empty list writes no key.
/// Catches a renamed or reordered key, a list sent when empty, and a pressure list that
/// is dropped when the busy list is empty.
#[test]
fn the_lists_have_the_exact_wire_form() {
    let got = with_seconds(vec![1500, 0, 250], vec![10, 0, 1000]);
    let text = serde_json::to_string(&got).unwrap();
    assert!(
        text.ends_with(concat!(
            r#""cpu_peak_window_millis":2500,"ws_max_bytes":2147483648}],"#,
            r#""second_cpu_millis":[1500,0,250],"#,
            r#""second_cpu_pressure_some_permille":[10,0,1000]}"#
        )),
        "{text}"
    );
    assert_eq!(
        serde_json::from_str::<Heartbeat>(&heartbeat_text(&got))
            .unwrap()
            .load,
        Some(got)
    );

    let plain = serde_json::to_value(load()).unwrap();
    assert!(plain.get("second_cpu_millis").is_none(), "{plain}");
    assert!(plain.get("second_cpu_pressure_some_permille").is_none());
    let busy_only = serde_json::to_value(with_seconds(vec![5], vec![])).unwrap();
    assert_eq!(busy_only["second_cpu_millis"], json!([5]));
    assert!(busy_only.get("second_cpu_pressure_some_permille").is_none());
}

fn heartbeat_text(load: &NodeLoad) -> String {
    let beat = Heartbeat {
        load: Some(load.clone()),
        ..heartbeat()
    };
    serde_json::to_string(&beat).unwrap()
}

/// Asserts the bytes the lists add to a load at their widest (the figure
/// `docs/design/agent-wire.md` gives), and that the widest values pass `vet` untouched.
/// Catches lists that outgrow the room `MAX_LOAD_BYTES` leaves.
#[test]
fn the_lists_at_their_widest_add_their_documented_bytes_to_a_load() {
    let widest = with_seconds(
        vec![u32::MAX; MAX_LOAD_SECONDS],
        vec![1000; MAX_LOAD_SECONDS],
    );
    let without = serde_json::to_string(&load()).unwrap().len();
    let with = serde_json::to_string(&widest).unwrap().len();
    assert_eq!(with - without, WIDEST_SECONDS);
    assert_eq!(WIDEST_SECONDS, 316);
    assert_eq!(MAX_LOAD_SECONDS, 16);
    let mut checked = widest.clone();
    assert_eq!(checked.vet(), Ok(()));
    assert_eq!(checked, widest);
}

/// Asserts that a load whose lists are absent, or whose pressure list is absent beside a
/// busy one (a host without pressure files), decodes with those lists empty and nothing
/// dropped. Catches a missing default.
#[test]
fn absent_lists_read_as_empty() {
    let mut body = body_with("second_cpu_millis", json!([7]));
    assert_eq!(decoded(body.clone()), with_seconds(vec![7], vec![]));
    body["load"]
        .as_object_mut()
        .unwrap()
        .remove("second_cpu_millis");
    assert_eq!(decoded(body), load());
}

/// Asserts that a list that is not an array, and an element that is not a number of its
/// type (a string, a negative, a fraction, a busy value past 32 bits, a pressure value
/// past 16 bits), is dropped and counted as malformed, with the other list, the other
/// elements and the beat kept. Catches a bad list or element costing the load or the beat.
#[test]
fn a_malformed_list_or_element_is_dropped_and_counted() {
    let got = decoded(body_with("second_cpu_millis", json!("1,2,3")));
    assert_eq!(got, load_with_dropped(0, 1, 0));

    let got = decoded(body_with(
        "second_cpu_millis",
        json!([1, "2", -3, 4.5, 4_294_967_296_u64, null, 7]),
    ));
    assert_eq!(got.second_cpu_millis, vec![1, 7]);
    assert_eq!(
        got.dropped,
        LoadDrops {
            malformed: 5,
            ..LoadDrops::default()
        }
    );

    let got = decoded(body_with(
        "second_cpu_pressure_some_permille",
        json!([1, 65_536, "x", 1000]),
    ));
    assert_eq!(got.second_cpu_pressure_some_permille, vec![1, 1000]);
    assert_eq!(
        got.dropped,
        LoadDrops {
            malformed: 2,
            ..LoadDrops::default()
        }
    );

    let got = decoded(body_with(
        "second_cpu_pressure_some_permille",
        json!({"a": 1}),
    ));
    assert_eq!(got.second_cpu_pressure_some_permille, Vec::<u16>::new());
    assert_eq!(
        got.dropped,
        LoadDrops {
            malformed: 1,
            ..LoadDrops::default()
        }
    );
}

fn load_with_dropped(invalid: u32, malformed: u32, over_cap: u32) -> NodeLoad {
    NodeLoad {
        dropped: LoadDrops {
            malformed,
            invalid,
            duplicate: 0,
            over_cap,
        },
        ..load()
    }
}

/// Asserts that a list of exactly `MAX_LOAD_SECONDS` values is read whole and one with more
/// keeps its first values and counts the rest as over the cap, for each list, without
/// reading the excess. Catches an off-by-one at the cap and a cap that is not enforced on
/// decode.
#[test]
fn decoding_cuts_each_list_to_its_cap() {
    let at_cap: Vec<u32> = (0..MAX_LOAD_SECONDS as u32).collect();
    let got = decoded(body_with("second_cpu_millis", json!(at_cap)));
    assert_eq!(got.second_cpu_millis, at_cap);
    assert_eq!(got.dropped, LoadDrops::default());

    let over: Vec<u32> = (0..MAX_LOAD_SECONDS as u32 + 3).collect();
    let got = decoded(body_with("second_cpu_millis", json!(over)));
    assert_eq!(got.second_cpu_millis, at_cap);
    assert_eq!(
        got.dropped,
        LoadDrops {
            over_cap: 3,
            ..LoadDrops::default()
        }
    );

    let over: Vec<u16> = (0..MAX_LOAD_SECONDS as u16 + 2).collect();
    let got = decoded(body_with("second_cpu_pressure_some_permille", json!(over)));
    assert_eq!(
        got.second_cpu_pressure_some_permille.len(),
        MAX_LOAD_SECONDS
    );
    assert_eq!(
        got.dropped,
        LoadDrops {
            over_cap: 2,
            ..LoadDrops::default()
        }
    );
}

/// Asserts `vet`: a pressure of 1000 is kept; one of 1001 empties the pressure list whole
/// and counts one invalid, leaving the busy list; lists over their caps are cut with the
/// excess counted; and a second `vet` changes nothing. Catches `<` for `<=` at 1000, a hole
/// left in a list, and a cap that `vet` does not enforce for a locally built load.
#[test]
fn vet_holds_pressure_to_a_thousand_and_each_list_to_its_cap() {
    let mut ok = with_seconds(vec![1, 2], vec![0, 1000]);
    assert_eq!(ok.vet(), Ok(()));
    assert_eq!(ok, with_seconds(vec![1, 2], vec![0, 1000]));

    let mut bad = with_seconds(vec![1, 2, 3], vec![5, 1001, 7]);
    assert_eq!(bad.vet(), Ok(()));
    assert_eq!(
        bad,
        NodeLoad {
            dropped: LoadDrops {
                invalid: 1,
                ..LoadDrops::default()
            },
            ..with_seconds(vec![1, 2, 3], vec![])
        }
    );
    let once = bad.clone();
    assert_eq!(bad.vet(), Ok(()));
    assert_eq!(bad, once);

    let mut long = with_seconds(vec![9; MAX_LOAD_SECONDS + 4], vec![9; MAX_LOAD_SECONDS + 5]);
    assert_eq!(long.vet(), Ok(()));
    assert_eq!(long.second_cpu_millis.len(), MAX_LOAD_SECONDS);
    assert_eq!(
        long.second_cpu_pressure_some_permille.len(),
        MAX_LOAD_SECONDS
    );
    assert_eq!(
        long.dropped,
        LoadDrops {
            over_cap: 9,
            ..LoadDrops::default()
        }
    );
}

/// Asserts the release now deployed, with its own copy of `NodeLoad`, reads a load that
/// carries both lists as the same load without them and sends back text that this release
/// reads with empty lists; and that this release reads a load written by that release (no
/// lists). Catches a field an older server cannot skip, and one this release demands of an
/// older agent.
#[test]
fn the_released_server_and_this_one_read_each_others_loads() {
    let new = with_seconds(vec![1500, 20], vec![3, 4]);
    // A load that also carries the host's non-file memory reads in the released server the same.
    let held = NodeLoad {
        host_nonfile_bytes: Some(5 << 30),
        ..new.clone()
    };
    let old: ReleasedLoad = serde_json::from_value(serde_json::to_value(&held).unwrap()).unwrap();
    let plain: ReleasedLoad =
        serde_json::from_value(serde_json::to_value(load()).unwrap()).unwrap();
    assert_eq!(old, plain);

    let written = ReleasedLoad {
        seq: 7,
        mono_usec: 9_000_000,
        cpu_usage_usec: 4_500_000,
        working_set_bytes: 3 << 30,
        active_file_bytes: 1 << 20,
        actions_cpu_limited: true,
        covered: vec!["3-4".into()],
        clocks: vec![ReleasedClock {
            lease_id: "3-4".into(),
            started_mono_usec: 1_000_000,
        }],
        minutes: vec![ReleasedMinute {
            lease_id: "3-4".into(),
            minute: 2,
            cpu_mean_millis: 1500,
            cpu_peak_window_millis: 2500,
            ws_max_bytes: 2 << 30,
        }],
    };
    let mut body = serde_json::to_value(heartbeat()).unwrap();
    body["load"] = serde_json::to_value(&written).unwrap();
    let read = decoded(body);
    // The released agent sends no host counter: the load reads without one.
    let released = NodeLoad {
        host_cpu_usec: None,
        ..load()
    };
    assert_eq!(read, released);
    assert_eq!(
        read.minutes[0],
        LeaseMinute {
            lease_id: "3-4".into(),
            minute: 2,
            cpu_mean_millis: 1500,
            cpu_peak_window_millis: 2500,
            ws_max_bytes: 2 << 30,
        }
    );
    assert_eq!(
        read.clocks[0],
        LeaseClock {
            lease_id: "3-4".into(),
            started_mono_usec: 1_000_000,
        }
    );
}
