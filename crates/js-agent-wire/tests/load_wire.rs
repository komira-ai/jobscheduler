//! The wire form of a heartbeat's load and how it decodes: exact text, defaults, the
//! lease id grammar, and that a bad load or record never costs the beat around it.

mod common;

use common::{heartbeat, heartbeat_with_load, load};
use js_agent_wire::limits::*;
use js_agent_wire::{Heartbeat, LoadDrops, NodeLoad, lease_parts};
use serde_json::{Value, json};

/// A heartbeat body (with results, held runs and a taint) whose `load` is `load`.
fn body_with(load: Value) -> Value {
    let mut value = serde_json::to_value(heartbeat()).unwrap();
    value["load"] = load;
    value
}

fn decode(value: Value) -> Heartbeat {
    serde_json::from_value(value).unwrap()
}

/// The load of [`load`] as JSON.
fn good_load() -> Value {
    serde_json::to_value(load()).unwrap()
}

fn minimal() -> NodeLoad {
    NodeLoad {
        seq: 1,
        mono_usec: 2,
        cpu_usage_usec: 3,
        working_set_bytes: 4,
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        dropped: LoadDrops::default(),
    }
}

/// Asserts that the beat still has its results, held runs and taint and passes
/// `validate`, whatever the load was.
fn beat_survives(beat: &Heartbeat) {
    assert_eq!(beat.results, heartbeat().results);
    assert_eq!(beat.held, heartbeat().held);
    assert_eq!(beat.taint, heartbeat().taint);
    assert_eq!(beat.validate(), Ok(()));
}

/// Asserts the exact text of a load with every field set. It catches a renamed or
/// reordered field, and a mean and peak written under each other's names.
#[test]
fn load_wire_form() {
    assert_eq!(
        serde_json::to_string(&load()).unwrap(),
        concat!(
            r#"{"seq":7,"mono_usec":9000000,"cpu_usage_usec":4500000,"#,
            r#""working_set_bytes":3221225472,"active_file_bytes":1048576,"#,
            r#""actions_cpu_limited":true,"covered":["3-4"],"#,
            r#""clocks":[{"lease_id":"3-4","started_mono_usec":1000000}],"#,
            r#""minutes":[{"lease_id":"3-4","minute":2,"cpu_mean_millis":1500,"#,
            r#""cpu_peak_window_millis":2500,"ws_max_bytes":2147483648}]}"#
        )
    );
    let beat = serde_json::to_value(heartbeat_with_load()).unwrap();
    assert_eq!(beat["load"], good_load());
}

/// Asserts that a load with only its four required scalars writes no list keys. It
/// catches a list written when empty (the next test then also fails to read it back).
#[test]
fn minimal_load_wire_form() {
    assert_eq!(
        serde_json::to_string(&minimal()).unwrap(),
        concat!(
            r#"{"seq":1,"mono_usec":2,"cpu_usage_usec":3,"working_set_bytes":4,"#,
            r#""active_file_bytes":0,"actions_cpu_limited":false}"#
        )
    );
}

/// Asserts that a body with four scalars and nothing else decodes to a load whose
/// optional scalars and lists are empty. It catches a missing default on any of them.
#[test]
fn minimal_load_takes_defaults() {
    let text = json!({"seq": 1, "mono_usec": 2, "cpu_usage_usec": 3, "working_set_bytes": 4});
    let beat = decode(body_with(text));
    assert_eq!(beat.load, Some(minimal()));
}

/// Asserts that a body from before the load existed decodes with `load` `None` and
/// encodes again without the key. It catches a missing default on `load`, and a
/// `"load":null` written for `None`.
#[test]
fn a_body_from_before_load_still_decodes() {
    let old = serde_json::to_value(heartbeat()).unwrap();
    assert!(old.get("load").is_none());
    let beat: Heartbeat = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(beat.load, None);
    assert_eq!(serde_json::to_value(&beat).unwrap(), old);
}

/// Asserts that a load with unknown keys in itself and in its records decodes to the
/// same load. It catches a `deny_unknown_fields`, which would break a newer sender.
#[test]
fn unknown_fields_are_ignored_in_load_and_records() {
    let mut value = serde_json::to_value(heartbeat_with_load()).unwrap();
    value["load"]["field_from_the_future"] = json!({"nested": [1]});
    value["load"]["minutes"][0]["field_from_the_future"] = json!(1);
    value["load"]["clocks"][0]["field_from_the_future"] = json!("x");
    assert_eq!(decode(value), heartbeat_with_load());
}

/// Asserts the lease id grammar: the root `term-seq` form and nothing else, including
/// every id the root type prints. It catches a parse that only checks digits (leading
/// zeros, a sign), one that wraps on overflow, one that accepts the `.` form, and drift
/// from the root `Display`.
#[test]
fn lease_parts_table() {
    let max = u64::MAX;
    assert_eq!(lease_parts("1-1"), Some((1, 1)));
    assert_eq!(lease_parts("0-0"), Some((0, 0)));
    assert_eq!(lease_parts(&format!("{max}-{max}")), Some((max, max)));
    assert_eq!(format!("{max}-{max}").len(), MAX_LEASE_REF_LEN);
    for (term, seq) in [(1, 1), (0, 0), (12, 345), (max, 0), (0, max), (max, max)] {
        let text = js_types::LeaseId::new(term, seq).to_string();
        assert_eq!(lease_parts(&text), Some((term, seq)), "{text}");
    }
    let too_long = "1".repeat(1 << 20);
    for bad in [
        "",
        "01-1",
        "1-01",
        "+1-1",
        "1-+1",
        "1-",
        "-1",
        "-",
        "1",
        "1-1-1",
        "1.1",
        "1 -1",
        " 1-1",
        "1-18446744073709551616",
        "18446744073709551616-1",
        "\u{ff11}-1",
        too_long.as_str(),
    ] {
        assert_eq!(lease_parts(bad), None, "{:?}", bad.get(..30).unwrap_or(bad));
    }
}

/// Asserts that a load of the wrong shape reads as `None` and the beat around it is
/// whole: not an object, a required scalar missing, or of another type. It catches the
/// decode error reaching the heartbeat, and `{}` read as an all-zero load.
#[test]
fn a_load_of_the_wrong_shape_decodes_as_none() {
    let mut rows = vec![
        json!(5),
        json!("x"),
        json!([]),
        json!(null),
        json!({}),
        json!(true),
    ];
    for key in ["seq", "mono_usec", "cpu_usage_usec", "working_set_bytes"] {
        let mut missing = good_load();
        missing.as_object_mut().unwrap().remove(key);
        rows.push(missing);
        for wrong in [json!("7"), json!(1.5), json!(-1), json!(null)] {
            let mut typed = good_load();
            typed[key] = wrong;
            rows.push(typed);
        }
    }
    for row in rows {
        let beat = decode(body_with(row.clone()));
        assert_eq!(beat.load, None, "{row}");
        beat_survives(&beat);
    }
}

/// Asserts that optional scalars of the wrong type read as their default and the rest of
/// the load is kept. It catches an optional scalar that refuses the whole load.
#[test]
fn optional_scalars_of_the_wrong_type_take_their_default() {
    let mut value = good_load();
    value["active_file_bytes"] = json!("many");
    value["actions_cpu_limited"] = json!(1);
    let got = decode(body_with(value)).load.unwrap();
    assert_eq!((got.active_file_bytes, got.actions_cpu_limited), (0, false));
    assert_eq!(got.minutes, load().minutes);
}

/// Asserts that decoding alone never vets: a record that breaks a rule (minute 64, a bad
/// lease id) is still present after decode and nothing is counted invalid. It catches a
/// decode that quietly drops rule-breaking records, which would leave the server's drop
/// accounting (`vet`) unpinned.
#[test]
fn decode_leaves_rule_breaking_records_for_vet() {
    let mut value = good_load();
    value["minutes"][0]["minute"] = json!(64);
    value["clocks"][0]["lease_id"] = json!("not-a-lease");
    let got = decode(body_with(value)).load.unwrap();
    assert_eq!(got.minutes[0].minute, 64);
    assert_eq!(got.clocks[0].lease_id, "not-a-lease");
    assert_eq!(got.dropped.invalid, 0);
    let mut vetted = got;
    assert_eq!(vetted.vet(), Ok(()));
    assert_eq!(vetted.dropped.invalid, 2);
    assert!(vetted.minutes.is_empty() && vetted.clocks.is_empty());
}

/// Asserts that, in each list, one record that does not decode is dropped and counted
/// and its neighbours stay: wrong types, `null`, a missing key (each key in turn), a
/// minute index too large for the wire type. It catches an all-or-nothing list decode and
/// a wrong `malformed` count.
#[test]
fn a_malformed_record_is_dropped_alone() {
    let minute = good_load()["minutes"][0].clone();
    let clock = good_load()["clocks"][0].clone();
    let lists = [
        (
            "minutes",
            minute.clone(),
            vec!["lease_id", "minute", "cpu_mean_millis"],
        ),
        (
            "minutes",
            minute,
            vec!["cpu_peak_window_millis", "ws_max_bytes"],
        ),
        ("clocks", clock, vec!["lease_id", "started_mono_usec"]),
        ("covered", json!("3-4"), vec![]),
    ];
    for (key, good, removable) in lists {
        let mut bad = vec![
            json!({"lease_id": 5}),
            json!("str"),
            json!(null),
            json!(7),
            json!([]),
        ];
        if key == "covered" {
            // Any string is a well-formed element at decode; `vet` judges the grammar.
            bad.retain(|item| !item.is_string());
        }
        for field in removable {
            let mut cut = good.clone();
            if let Some(object) = cut.as_object_mut() {
                object.remove(field);
            }
            bad.push(cut);
        }
        let mut too_big = good.clone();
        if key == "minutes" {
            too_big["minute"] = json!(70_000);
            too_big["cpu_mean_millis"] = json!(u64::from(u32::MAX) + 1);
            bad.push(too_big);
        }
        let mut items = vec![good.clone()];
        items.extend(bad.iter().cloned());
        items.push(good.clone());
        let mut value = good_load();
        value[key] = Value::Array(items);
        let beat = decode(body_with(value));
        beat_survives(&beat);
        let got = beat.load.unwrap();
        let kept = match key {
            "minutes" => got.minutes.len(),
            "clocks" => got.clocks.len(),
            _ => got.covered.len(),
        };
        assert_eq!(kept, 2, "{key}");
        let malformed = u32::try_from(bad.len()).unwrap();
        let want = LoadDrops {
            malformed,
            ..LoadDrops::default()
        };
        assert_eq!(got.dropped, want, "{key}");
    }
}

/// Asserts that a list that is not an array reads as empty with one `malformed`, and that
/// an absent list reads as empty with none. It catches a refusal of the load for a list
/// of the wrong type, and an absent list counted as bad.
#[test]
fn a_list_that_is_not_an_array_reads_as_empty() {
    for key in ["minutes", "clocks", "covered"] {
        for wrong in [json!(5), json!(null), json!({"a": 1}), json!("x")] {
            let mut value = good_load();
            value[key] = wrong;
            let got = decode(body_with(value)).load.unwrap();
            let want = LoadDrops {
                malformed: 1,
                ..LoadDrops::default()
            };
            assert_eq!(got.dropped, want, "{key}");
            assert!(got.minutes.len() + got.clocks.len() + got.covered.len() < 3);
        }
        let mut value = good_load();
        value.as_object_mut().unwrap().remove(key);
        let got = decode(body_with(value)).load.unwrap();
        assert_eq!(got.dropped, LoadDrops::default(), "{key}");
    }
}

/// Asserts that a list at its cap is kept whole, and one over it is cut to the cap with
/// the excess counted as `over_cap`, however long. A bad element past the cap counts as
/// over the cap too, not as malformed. It catches `>` against `>=`, cap constants that
/// drift from the table, and a load or beat refused for being long.
#[test]
fn a_list_over_its_cap_is_cut_not_refused() {
    let cases = [
        ("minutes", MAX_LOAD_MINUTES),
        ("clocks", MAX_LOAD_LEASES),
        ("covered", MAX_LOAD_LEASES),
    ];
    for (key, cap) in cases {
        for n in [cap, cap + 1, cap + 1000] {
            let item = |i: usize| match key {
                "minutes" => json!({"lease_id": format!("1-{i}"), "minute": 0,
                    "cpu_mean_millis": 1, "cpu_peak_window_millis": 1, "ws_max_bytes": 1}),
                "clocks" => json!({"lease_id": format!("1-{i}"), "started_mono_usec": 1}),
                _ => json!(format!("1-{i}")),
            };
            let mut value = good_load();
            value[key] = Value::Array((0..n).map(item).collect());
            let beat = decode(body_with(value));
            beat_survives(&beat);
            let got = beat.load.unwrap();
            let kept = match key {
                "minutes" => got.minutes.len(),
                "clocks" => got.clocks.len(),
                _ => got.covered.len(),
            };
            assert_eq!(kept, cap, "{key} {n}");
            let want = LoadDrops {
                over_cap: u32::try_from(n - cap).unwrap(),
                ..LoadDrops::default()
            };
            assert_eq!(got.dropped, want, "{key} {n}");
        }
        let mut items: Vec<Value> = vec![json!(null); cap + 1];
        items[..cap].fill(match key {
            "minutes" => json!({"lease_id": "1-1", "minute": 0, "cpu_mean_millis": 1,
                "cpu_peak_window_millis": 1, "ws_max_bytes": 1}),
            "clocks" => json!({"lease_id": "1-1", "started_mono_usec": 1}),
            _ => json!("1-1"),
        });
        let mut value = good_load();
        value[key] = Value::Array(items);
        let got = decode(body_with(value)).load.unwrap();
        let want = LoadDrops {
            over_cap: 1,
            ..LoadDrops::default()
        };
        assert_eq!(got.dropped, want, "{key} bad past the cap");
    }
}

/// Asserts that a load which decodes but is full of nonsense (a megabyte lease id, a
/// minute far past the tail) leaves the heartbeat's results, held runs and taint whole,
/// and `validate` does not look at the load. It catches `validate` calling `vet`, and a
/// decode that errors on a hostile record.
#[test]
fn a_hostile_load_does_not_cost_the_beat() {
    let mut value = good_load();
    value["working_set_bytes"] = json!(u64::MAX);
    value["minutes"] = json!([{"lease_id": "x".repeat(1 << 20), "minute": 60000,
        "cpu_mean_millis": 9, "cpu_peak_window_millis": 1, "ws_max_bytes": u64::MAX}]);
    value["clocks"] = json!([{"lease_id": "..", "started_mono_usec": u64::MAX}]);
    let beat = decode(body_with(value));
    beat_survives(&beat);
    assert!(beat.load.is_some());
}
