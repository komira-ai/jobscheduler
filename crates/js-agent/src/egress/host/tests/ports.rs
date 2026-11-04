//! The forwards of declared ports against the scripted machine: their rules loaded after the
//! route and read back, a host port held by one run at a time, their removal at release, and
//! the check that tells the forwards of the runs wired now from any other.

use std::net::Ipv4Addr;

use serde_json::Value;

use super::*;
use crate::egress::forward::fixture::{filter_rule, nat_rule, printed, with};

/// The declared ports `host -> container`.
fn ports(pairs: &[(u16, u16)]) -> Vec<PortMap> {
    pairs
        .iter()
        .map(|&(host, container)| PortMap { host, container })
        .collect()
}

/// `INET` with `rules` at the head of the chain `forward`, where `insert rule` puts them.
fn inet_with(rules: &[Value]) -> String {
    let mut value: Value = serde_json::from_str(INET).expect("json");
    let items = value["nftables"].as_array_mut().expect("items");
    let at = items
        .iter()
        .position(|item| item.get("rule").is_some())
        .expect("a rule");
    for (n, rule) in rules.iter().enumerate() {
        items.insert(at + n, rule.clone());
    }
    value.to_string()
}

/// `NAT` with `rules` in it.
fn nat_with(rules: &[Value]) -> String {
    let value: Value = serde_json::from_str(NAT).expect("json");
    let mut items = value["nftables"].as_array().expect("items").clone();
    items.extend_from_slice(rules);
    serde_json::json!({ "nftables": items }).to_string()
}

/// Makes both tables read as `INET` and `NAT` with the rules of the forwards `held` (host
/// port, interface, job address, container port), each forward's acceptance with the handle
/// `100 + n` and its DNAT rule `200 + n`.
fn script(fake: &Fake, held: &[(u16, &str, &str, u16)]) {
    let (mut filters, mut nats) = (Vec::new(), Vec::new());
    for (n, &(host, iface, job, container)) in (0u64..).zip(held) {
        filters.push(filter_rule(host, iface, job, container, 100 + n));
        nats.push(nat_rule(host, job, container, 200 + n));
    }
    fake.answer("list table inet", Ok(inet_with(&filters)));
    fake.answer("list table ip", Ok(nat_with(&nats)));
}

/// The forward of host port `host` to the first block's job.
fn first(host: u16, container: u16) -> Forward {
    Forward {
        host,
        job: Ipv4Addr::new(198, 18, 0, 2),
        container,
        iface: "jsv0".to_owned(),
    }
}

/// The last input given to a program that starts with `start`.
fn stdin(fake: &Fake, start: &str) -> String {
    let state = fake.state();
    let found = state
        .stdins
        .iter()
        .rev()
        .find(|text| text.starts_with(start));
    found.cloned().expect("an input")
}

/// The calls of the forwards of a run after its wiring: the load, then both tables read back.
fn forward_calls(first_line: &str) -> Vec<String> {
    vec![
        format!("nft -f - << {first_line}"),
        "nft -j list table inet js_crun_egress".to_owned(),
        "nft -j list table ip js_crun_egress_nat".to_owned(),
    ]
}

/// The calls of the release of the forwards whose first deletion is `first_line`, then of the
/// interface `jsv<n>`.
fn unforward_calls(first_line: &str, n: u32) -> Vec<String> {
    let mut calls = vec![
        "nft -j list table inet js_crun_egress".to_owned(),
        "nft -j list table ip js_crun_egress_nat".to_owned(),
        format!("nft -f - << {first_line}"),
    ];
    calls.extend(release_calls(n));
    calls
}

/// Catches forwards loaded before the job's route, outside one transaction, or for a job with
/// no ports: a run with no ports makes exactly the calls it made before forwards existed; a
/// run with the ports `9000 -> 9001` and `8080 -> 80` makes those, then one `nft -f`
/// transaction of both forwards' rules (sorted by host port, each DNAT rule before its
/// acceptance) after the default route, then reads both tables back.
#[test]
fn attach_with_ports_adds_the_rules_in_order_after_the_route() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.log().len();
    let plain = egress
        .attach("plain", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(fake.log()[before..], attach_calls()[..]);
    let released = fake.log().len();
    plain.release().expect("release");
    assert_eq!(fake.log()[released..], release_calls(0)[..]);

    script(
        &fake,
        &[
            (8080, "jsv0", "198.18.0.2", 80),
            (9000, "jsv0", "198.18.0.2", 9001),
        ],
    );
    let before = fake.log().len();
    let ported = egress
        .attach(
            "ported",
            PID,
            JobNetwork::AllowList,
            &ports(&[(9000, 9001), (8080, 80)]),
        )
        .expect("attach");
    let log = fake.log()[before..].to_vec();
    assert_eq!(log[..7], attach_calls()[..]);
    let (nat, _) = first(8080, 80).rules();
    assert_eq!(log[7..], forward_calls(&nat)[..]);
    let loaded = stdin(&fake, "add rule");
    assert_eq!(
        loaded,
        forward::transaction(&[first(8080, 80), first(9000, 9001)])
    );
    egress.check().expect("the forwards of a live run");
    drop(ported);
}

/// Catches a host port forwarded to two runs, a refusal that leaves part of the second run,
/// and a look at the ports in use made after their rules are loaded: with `8080` forwarded to
/// one run, a second run that declares `8080` is `PortTaken` naming it; its calls are its
/// wiring and the removal of its wiring and nothing loaded; its block is free again (the next
/// run is wired as `jsv1`); and the first run's forward is still whole (the check passes). A
/// run that declares one host port twice is refused the same way.
#[test]
fn a_second_job_for_the_same_host_port_is_refused_and_leaves_nothing() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let held = egress
        .attach("first", PID, JobNetwork::AllowList, &ports(&[(8080, 80)]))
        .expect("attach");
    let before = fake.log().len();
    let refused = egress
        .attach(
            "second",
            PID,
            JobNetwork::AllowList,
            &ports(&[(9000, 90), (8080, 81)]),
        )
        .map(drop);
    assert_eq!(refused, Err(EgressError::PortTaken { port: 8080 }));
    let log = fake.log()[before..].to_vec();
    assert!(log.ends_with(&release_calls(1)), "{log:?}");
    assert!(
        log.iter().all(|call| !call.starts_with("nft -f -")),
        "something was loaded: {log:?}"
    );
    egress.check().expect("the first run's forward is whole");
    let twice = egress.attach(
        "twice",
        PID,
        JobNetwork::AllowList,
        &ports(&[(9000, 90), (9000, 91)]),
    );
    assert_eq!(twice.map(drop), Err(EgressError::PortTaken { port: 9000 }));
    let next = egress
        .attach("next", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(next.interface(), Some("jsv1"), "the block is free again");
    drop((held, next));
}

/// Catches forwards that are not read back, or that stay when they read back wrong: when the
/// listing after the load holds the forward with another container port, or holds it behind
/// the rule it must precede, the attach is `Rules`, the forward's rules are deleted by
/// handle, then the wiring; when the listing cannot be read the attach fails naming it, and
/// is undone the same way; when the load itself fails nothing is deleted (nothing was
/// loaded). Each time the block and the port are free for the next run, and no forward is
/// recorded (the check of the tables without forwards passes).
#[test]
fn a_forward_that_reads_back_wrong_fails_the_attach_and_is_undone() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let deleted = "delete rule inet js_crun_egress forward handle 100";
    let wrong_port = |fake: &Fake| script(fake, &[(8080, "jsv0", "198.18.0.2", 81)]);
    let behind = |fake: &Fake| {
        let mut value: Value = serde_json::from_str(INET).expect("json");
        let items = value["nftables"].as_array_mut().expect("items");
        items.push(filter_rule(8080, "jsv0", "198.18.0.2", 80, 100));
        fake.answer("list table inet", Ok(value.to_string()));
        fake.answer(
            "list table ip",
            Ok(nat_with(&[nat_rule(8080, "198.18.0.2", 80, 200)])),
        );
    };
    let wrongs: [&dyn Fn(&Fake); 2] = [&wrong_port, &behind];
    for wrong in wrongs {
        wrong(&fake);
        let before = fake.log().len();
        let error = egress
            .attach("wrong", PID, JobNetwork::AllowList, &ports(&[(8080, 80)]))
            .map(drop);
        assert_eq!(error, Err(EgressError::Rules));
        let log = fake.log()[before..].to_vec();
        let (nat, _) = first(8080, 80).rules();
        assert_eq!(log[7..10], forward_calls(&nat)[..]);
        assert_eq!(log[10..], unforward_calls(deleted, 0)[..], "{log:?}");
        assert_eq!(
            stdin(&fake, "delete rule"),
            "delete rule inet js_crun_egress forward handle 100\n\
             delete rule ip js_crun_egress_nat prerouting handle 200\n"
        );
    }

    fake.answer("list table inet", Ok("nope".to_owned()));
    let before = fake.log().len();
    let error = egress
        .attach("blind", PID, JobNetwork::AllowList, &ports(&[(8080, 80)]))
        .expect_err("unreadable");
    let text = error.to_string();
    assert!(text.contains("wiring run blind"), "{text}");
    assert!(text.contains("reading the firewall back"), "{text}");
    assert!(
        fake.log()[before..].ends_with(&release_calls(0)),
        "{:?}",
        fake.log()
    );

    script(&fake, &[]);
    let before = fake.calls();
    let logged = fake.log().len();
    fake.fail_call(before + 7);
    let error = egress
        .attach(
            "unloaded",
            PID,
            JobNetwork::AllowList,
            &ports(&[(8080, 80)]),
        )
        .expect_err("not loaded");
    let text = error.to_string();
    assert!(text.contains("loading the forwards"), "{text}");
    assert!(text.contains("injected"), "{text}");
    let log = fake.log()[logged..].to_vec();
    assert!(
        log.iter().all(|call| !call.contains("delete rule")),
        "a deletion for a load that failed: {log:?}"
    );
    assert!(log.ends_with(&release_calls(0)), "{log:?}");
    egress.check().expect("no forward is recorded");
    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let again = egress
        .attach("again", PID, JobNetwork::AllowList, &ports(&[(8080, 80)]))
        .expect("the port and the block are free");
    assert_eq!(again.interface(), Some("jsv0"));
}

/// A way a release's listing fails: what makes it fail, what the error says, and the interface
/// and address of the run it fails for.
type Failure<'a> = (&'a dyn Fn(&Fake), &'a str, &'a str, &'a str);

/// Catches a release that leaves a forward, frees its port while the rules stay without
/// saying so, or lets a failing deletion keep the interface: a release deletes the run's
/// forward rules by handle (one transaction, found by a listing of each table), then the set
/// member and the veth, and the port is free again (a new run that declares it is wired);
/// a drop does the same. A table that is gone, and a deletion that finds the rule gone, are
/// not errors. A deletion that fails for another reason, a listing that is not JSON, one
/// whose program cannot be started and one that fails are errors naming the forwards; the
/// set member and the veth are still removed, the port is free, and the block is kept out of
/// use (the next run is wired on the next block).
#[test]
fn release_removes_the_forwards_then_the_interface() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let deleted = "delete rule inet js_crun_egress forward handle 100";
    let attach = |run: &str| {
        egress
            .attach(run, PID, JobNetwork::AllowList, &ports(&[(8080, 80)]))
            .expect("attach")
    };
    let one = attach("one");
    let before = fake.log().len();
    one.release().expect("release");
    assert_eq!(fake.log()[before..], unforward_calls(deleted, 0)[..]);
    assert_eq!(
        stdin(&fake, "delete rule"),
        "delete rule inet js_crun_egress forward handle 100\n\
         delete rule ip js_crun_egress_nat prerouting handle 200\n"
    );
    let two = attach("two");
    assert_eq!(two.interface(), Some("jsv0"), "the port is free again");
    let before = fake.log().len();
    drop(two);
    assert_eq!(fake.log()[before..], unforward_calls(deleted, 0)[..]);

    let three = attach("three");
    fake.answer(
        "list table ip",
        Err("Error: No such file or directory".to_owned()),
    );
    fake.answer(
        "-f -",
        Err("Error: Could not process rule: No such file or directory".to_owned()),
    );
    let before = fake.log().len();
    three.release().expect("gone is not an error");
    assert_eq!(
        fake.log()[before..],
        [
            "nft -j list table inet js_crun_egress".to_owned(),
            "nft -j list table ip js_crun_egress_nat".to_owned(),
            "nft -f - << delete rule inet js_crun_egress forward handle 100".to_owned(),
            release_calls(0)[0].clone(),
            release_calls(0)[1].clone(),
        ]
    );
    fake.answer("-f -", Ok(String::new()));

    // Nothing to delete: the listings hold no rule of the run's port.
    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let four = attach("four");
    script(&fake, &[]);
    let before = fake.log().len();
    four.release().expect("nothing left to delete");
    let mut expected = vec![
        "nft -j list table inet js_crun_egress".to_owned(),
        "nft -j list table ip js_crun_egress_nat".to_owned(),
    ];
    expected.extend(release_calls(0));
    assert_eq!(fake.log()[before..], expected[..]);

    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let five = attach("five");
    let before = fake.calls();
    fake.fail_call(before + 2);
    let logged = fake.log().len();
    let error = five.release().expect_err("the deletion failed");
    let text = error.to_string();
    assert!(text.contains("removing the forwards"), "{text}");
    assert!(text.contains("injected"), "{text}");
    assert!(
        fake.log()[logged..].ends_with(&release_calls(0)),
        "the interface is removed whatever became of the forwards"
    );
    // The port is free; block 0 is not, since a forward may still name its address.
    script(&fake, &[(8080, "jsv1", "198.18.0.6", 80)]);
    let six = attach("six");
    assert_eq!(six.interface(), Some("jsv1"));

    // Each failed release below keeps its block out of use: the next run takes the next.
    let failures: [Failure<'_>; 3] = [
        (
            &|fake: &Fake| fake.answer("list table inet", Ok("nope".to_owned())),
            "line 1 column",
            "jsv2",
            "198.18.0.10",
        ),
        (
            &|fake: &Fake| fake.not_run("list table inet"),
            "not run",
            "jsv3",
            "198.18.0.14",
        ),
        (
            &|fake: &Fake| {
                fake.answer("list table inet", Err("Operation not permitted".to_owned()));
            },
            "Operation not permitted",
            "jsv4",
            "198.18.0.18",
        ),
    ];
    for (fail, said, iface, job) in failures {
        script(&fake, &[(9000, iface, job, 90)]);
        let held = egress
            .attach("seven", PID, JobNetwork::AllowList, &ports(&[(9000, 90)]))
            .expect("attach");
        assert_eq!(held.interface(), Some(iface));
        fail(&fake);
        let error = held.release().expect_err("the listing failed");
        let text = error.to_string();
        assert!(text.contains("listing the forwards"), "{text}");
        assert!(text.contains(said), "{text}");
        fake.run_again();
    }
    drop(six);
}

/// Catches a check that does not read the forwards back, or compares them with anything but
/// the forwards of the runs wired now: with one run's forward in place the check passes; the
/// forward's acceptance deleted, an extra whole forward of no run, a lone extra rule under
/// the prefix, the forward with another target, the forward twice, the acceptance behind the
/// rule it must precede, and a static rule changed with the forward in place are each
/// `Rules`; once the run is released and its rules are gone the check passes again.
#[test]
fn check_accepts_the_forwards_of_live_jobs_and_refuses_any_other() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let run = egress
        .attach("live", PID, JobNetwork::AllowList, &ports(&[(8080, 80)]))
        .expect("attach");
    egress.check().expect("the live run's forward");

    let filter = || filter_rule(8080, "jsv0", "198.18.0.2", 80, 100);
    let nat = || nat_rule(8080, "198.18.0.2", 80, 200);
    let stray = || filter_rule(9999, "jsv5", "198.18.0.22", 99, 300);
    let behind = {
        let mut value: Value = serde_json::from_str(INET).expect("json");
        let items = value["nftables"].as_array_mut().expect("items");
        items.push(filter());
        value.to_string()
    };
    let changed: Value = serde_json::from_str(INET_CHANGED).expect("json");
    let changed = printed(
        "inet",
        "js_crun_egress",
        &[
            changed["nftables"][2].clone(),
            filter(),
            changed["nftables"][3].clone(),
        ],
    );
    let cases = [
        ("the acceptance deleted", inet_with(&[]), nat_with(&[nat()])),
        (
            "a whole forward of no run",
            inet_with(&[stray(), filter()]),
            nat_with(&[nat(), nat_rule(9999, "198.18.0.22", 99, 301)]),
        ),
        (
            "a lone rule under the prefix",
            inet_with(&[stray(), filter()]),
            nat_with(&[nat()]),
        ),
        (
            "another target",
            inet_with(&[with(filter(), "/rule/expr/2/match/right", 81.into())]),
            nat_with(&[with(nat(), "/rule/expr/4/dnat/port", 81.into())]),
        ),
        (
            "the forward twice",
            inet_with(&[filter(), filter()]),
            nat_with(&[nat(), nat()]),
        ),
        ("the acceptance behind", behind, nat_with(&[nat()])),
        ("a static rule changed", changed, nat_with(&[nat()])),
    ];
    for (case, inet, nat) in cases {
        fake.answer("list table inet", Ok(inet));
        fake.answer("list table ip", Ok(nat));
        assert_eq!(egress.check(), Err(EgressError::Rules), "{case}");
    }

    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    egress.check().expect("as it was");
    run.release().expect("release");
    script(&fake, &[]);
    egress.check().expect("no run, no forward");
}

/// Catches a check that changed for a host whose runs declare no ports: with a run wired that
/// declares none, the check reads the two tables (and nothing more) and passes on the
/// tables as loaded, and a rule under the prefix `fwd-` in them, which no run made, is
/// `Rules`.
#[test]
fn a_job_without_ports_leaves_check_exactly_as_it_was() {
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let run = egress
        .attach("bare", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    let before = fake.log().len();
    egress.check().expect("as loaded");
    assert_eq!(
        fake.log()[before..],
        [
            "nft -j list table inet js_crun_egress",
            "nft -j list table ip js_crun_egress_nat"
        ]
    );
    script(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    assert_eq!(egress.check(), Err(EgressError::Rules));
    drop(run);
}
