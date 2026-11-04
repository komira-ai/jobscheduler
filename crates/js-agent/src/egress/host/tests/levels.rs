//! The host side for each job level against the scripted machine: `open` runs in `openifs`,
//! `none` runs wired with nothing, the `open` firewall read back, and a set member that
//! outlives its release.

use super::*;

/// The table `inet js_crun_egress` of an `open` ceiling as `nft -j` lists it, cut to the rules
/// the check must tell apart: the reply, the metadata refusal and the open acceptance.
const OPEN_INET: &str = r#"{"nftables":[{"metainfo":{"version":"1.0.9"}},{"table":{"family":"inet","name":"js_crun_egress","handle":1}},{"set":{"name":"jobifs","type":"ifname","handle":2}},{"set":{"name":"openifs","type":"ifname","handle":3,"elem":["jsv0"]}},{"rule":{"chain":"forward","handle":4,"comment":"reply","expr":[{"counter":{"packets":0,"bytes":0}},{"accept":null}]}},{"rule":{"chain":"forward","handle":5,"comment":"metadata","expr":[{"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"daddr"}},"right":{"prefix":{"addr":"169.254.0.0","len":16}}}},{"counter":{"packets":0,"bytes":0}},{"reject":{"type":"icmp","expr":"admin-prohibited"}}]}},{"rule":{"chain":"forward","handle":6,"comment":"open","expr":[{"match":{"op":"==","left":{"meta":{"key":"iifname"}},"right":"@openifs"}},{"counter":{"packets":0,"bytes":0}},{"accept":null}]}}]}"#;

fn open() -> Config {
    config_with("open", None)
}

/// Catches an `open` ceiling whose firewall can be changed under a job unseen: the start
/// loads the `open` ruleset and reads it back; a listing with other counters and another
/// member of `openifs` is still as loaded, and the same listing with the metadata refusal
/// removed is `Rules`, through the crun runtime's port too, which the runtime's check turns
/// into a refusal of the start (`egress-unchecked`) before anything is created.
#[test]
fn a_tampered_open_ruleset_is_refused_by_check() {
    let fake = Fake::new();
    fake.answer("list table inet", Ok(OPEN_INET.to_owned()));
    let egress = started(&fake, open());
    let loaded = fake.state().stdins[2].clone();
    assert_eq!(loaded, ruleset::render(&open()));
    assert!(loaded.contains("comment \"open\""), "{loaded}");
    egress.check().expect("as loaded");
    fake.answer(
        "list table inet",
        Ok(OPEN_INET
            .replace("\"packets\":0", "\"packets\":7")
            .replace("[\"jsv0\"]", "[\"jsv3\"]")),
    );
    egress
        .check()
        .expect("counters and members are not a change");
    let value: serde_json::Value = serde_json::from_str(OPEN_INET).expect("json");
    let mut items = value["nftables"].as_array().expect("items").clone();
    items.retain(|item| item["rule"]["comment"] != "metadata");
    assert_eq!(items.len(), 6, "the metadata refusal is gone");
    let tampered = serde_json::json!({ "nftables": items }).to_string();
    fake.answer("list table inet", Ok(tampered));
    assert_eq!(egress.check(), Err(EgressError::Rules));
    let port = crate::crun::port_of(&egress);
    assert_eq!(port.check(), Err(EgressError::Rules));
}

/// Catches a run wired into the set of another level, or a level above the ceiling wired at
/// all: under `open` an `open` job's interface goes in `openifs` and leaves it on release, an
/// `allow-list` job's in `jobifs`; under `allow-list` an `open` job is `AboveCeiling` and
/// makes no call.
#[test]
fn each_level_is_wired_into_its_own_set() {
    let fake = Fake::new();
    let egress = started(&fake, open());
    let before = fake.log().len();
    let wide = egress
        .attach("wide", PID, JobNetwork::Open)
        .expect("attach");
    assert_eq!(wide.interface(), Some("jsv0"));
    let calls: Vec<String> = attach_calls()
        .into_iter()
        .map(|call| call.replace(" jobifs ", " openifs "))
        .collect();
    assert_eq!(fake.log()[before..], calls[..]);
    let attached = fake.log().len();
    wide.release().expect("release");
    assert_eq!(
        fake.log()[attached..],
        [
            "nft delete element inet js_crun_egress openifs { \"jsv0\" }",
            "ip link del jsv0",
        ]
    );
    let before = fake.log().len();
    let listed = egress
        .attach("listed", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(fake.log()[before..], attach_calls()[..]);
    drop(listed);

    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.calls();
    let above = egress.attach("above", PID, JobNetwork::Open);
    assert_eq!(
        above.map(|a| a.interface().map(str::to_owned)),
        Err(EgressError::AboveCeiling {
            asked: JobNetwork::Open,
            ceiling: JobNetwork::AllowList,
        })
    );
    assert_eq!(fake.calls(), before, "nothing was made");
    let said = EgressError::AboveCeiling {
        asked: JobNetwork::Open,
        ceiling: JobNetwork::None,
    }
    .to_string();
    assert_eq!(
        said,
        "egress: a open job cannot be wired under the ceiling none"
    );
}

/// Catches a default that wires a job, and a `none` job given an interface under a ceiling
/// that has them: a start decoded with no `network` reads as `none`, and attaching it under
/// `open` (and under `allow-list`) makes no call (no veth, no set member, no rule) and gives
/// an attachment with no interface whose drop makes none either. The root test
/// `a_none_job_has_loopback_only` shows the namespace holds `lo` alone.
#[test]
fn none_is_the_default_and_has_loopback_only() {
    let start = serde_json::json!({
        "run_id": "r1",
        "lease_id": "1-1",
        "image": "app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "limits": {"cpu_millis": 1, "memory_bytes": 1, "wall_ms": 1},
    });
    let spec: js_agent_wire::JobSpec = serde_json::from_value(start).expect("a start");
    assert_eq!(spec.network, JobNetwork::None);
    assert!(spec.ports.is_empty());
    for config in [open(), allow_list()] {
        let fake = Fake::new();
        let egress = started(&fake, config);
        let before = fake.calls();
        let attachment = egress
            .attach("quiet", PID, spec.network)
            .expect("nothing to wire");
        assert_eq!(attachment.interface(), None);
        drop(attachment);
        assert_eq!(fake.calls(), before, "a none job is wired with nothing");
    }
}

/// Catches an `open` run's set member outliving its release and handing the next run of
/// another level its access: under `open`, an `open` run whose `openifs` member cannot be
/// removed (the veth is deleted) is a failed release, and its name `jsv0` is not given to the
/// `allow-list` run that comes next, which is wired as `jsv1` in `jobifs`; the same holds for
/// an attach that fails after its member was added and whose undoing cannot remove it. Once
/// removals work again a release gives its block back.
#[test]
fn a_member_left_in_a_set_is_never_reused_by_another_run() {
    let fake = Fake::new();
    let egress = started(&fake, open());
    let wide = egress
        .attach("wide", PID, JobNetwork::Open)
        .expect("attach");
    assert_eq!(wide.interface(), Some("jsv0"));
    fake.answer(
        "delete element",
        Err("Error: Operation not permitted".to_owned()),
    );
    let error = wide.release().expect_err("the member is still there");
    assert!(
        error.to_string().contains("Operation not permitted"),
        "{error}"
    );
    let before = fake.log().len();
    let listed = egress
        .attach("listed", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(listed.interface(), Some("jsv1"));
    assert!(
        fake.log()[before..]
            .contains(&"nft add element inet js_crun_egress jobifs { \"jsv1\" }".to_owned()),
        "{:?}",
        fake.log()
    );

    // An attach that fails after its member was added, whose undoing cannot remove it.
    fake.answer(
        "netns",
        Err("RTNETLINK answers: Invalid argument".to_owned()),
    );
    let failed = egress.attach("failed", PID, JobNetwork::Open);
    assert!(failed.is_err());
    fake.answer("netns", Ok(String::new()));
    let next = egress
        .attach("next", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(
        next.interface(),
        Some("jsv3"),
        "jsv2 keeps its stale member"
    );

    fake.answer("delete element", Ok(String::new()));
    next.release().expect("release");
    let again = egress
        .attach("again", PID, JobNetwork::AllowList)
        .expect("attach");
    assert_eq!(
        again.interface(),
        Some("jsv3"),
        "a clean release gives its block back"
    );
    drop((listed, again));
}
