//! Taking over the wiring an earlier process left, against the scripted machine and real
//! records in a scratch directory: what a start keeps from the records of the egress store,
//! the one transaction that puts it back, the claim a run makes on its own wiring, and the
//! settle that removes what no run claimed. The pure parts are in `adopt_tests.rs`.

use std::net::Ipv4Addr;

use js_testkit::dir::TestDir;
use serde_json::Value;

use super::records::{
    forwarding, maps, put, recorded, scratch, show, started_with, store_in, tables,
};
use super::*;
use crate::egress::forward::fixture::{filter_rule, inbound};

fn forward_of(host: u16, last: u8, container: u16, iface: &str) -> Forward {
    Forward {
        host,
        job: Ipv4Addr::new(198, 18, 0, last),
        container,
        iface: iface.to_owned(),
    }
}

/// The text of the refusal of an adoption of the run `run`.
fn cannot(run: &str, why: &str) -> String {
    format!("egress: the wiring of run {run} cannot be taken over: {why}")
}

/// The machine of an earlier process that left the interface `jsv0` and the record of the run
/// `r` for block 0 at `level` with `ports` (their forwards in the tables), started under
/// `config` with its store.
fn left_one(
    config: Config,
    level: JobNetwork,
    ports: &[(u16, u16)],
) -> (Arc<Fake>, TestDir, Store, Egress) {
    let dir = scratch("left-one");
    let store = store_in(&dir);
    put(&store, "r", (0, level), ports);
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsv0"])));
    let rules: Vec<(u16, &str, &str, u16)> = ports
        .iter()
        .map(|&(host, container)| (host, "jsv0", "198.18.0.2", container))
        .collect();
    show(&fake, &rules);
    let egress = started_with(&fake, config, &store);
    (fake, dir, store, egress)
}

/// What an adoption of `run` at `level` with `ports` is refused with, when the record of `r` is
/// for block 0 at `kept_level` with `kept_ports`, under `config`; and that the refusal took
/// nothing: the wiring and the record are still there for the settle, which then removes both.
fn refusal(
    config: Config,
    (kept_level, kept_ports): (JobNetwork, &[(u16, u16)]),
    (run, level, ports): (&str, JobNetwork, &[(u16, u16)]),
) -> String {
    let (fake, _dir, store, egress) = left_one(config, kept_level, kept_ports);
    let error = egress.adopt(run, level, &maps(ports)).expect_err("refused");
    assert_eq!(recorded(&store), ["r"], "the record stays");
    let before = fake.log().len();
    egress.settle().expect("settled");
    assert!(
        fake.log()[before..].contains(&"ip link del jsv0".to_owned()),
        "{:?}",
        fake.log()
    );
    assert_eq!(recorded(&store), Vec::<String>::new());
    error.to_string()
}

/// Asserts the start's one load, in full: the ruleset, then each kept record's interface back
/// in the set of its level in the order of the runs' names (`a` in `jobifs`, `b` in `openifs`,
/// `c` in `jobifs`), then the forwards of every kept record, sorted by host port; one `nft -f`
/// only; that the read back accepts it; and that the three blocks are out of use (the next
/// run gets block 2). Catches a start that loses a wired run's rules, or gives out a block an
/// earlier process still wires.
#[test]
fn the_start_loads_the_ruleset_and_the_kept_records_in_one_transaction() {
    let dir = scratch("load");
    let store = store_in(&dir);
    put(&store, "a", (0, JobNetwork::AllowList), &[(8080, 80)]);
    put(&store, "b", (3, JobNetwork::Open), &[(9000, 90)]);
    put(&store, "c", (1, JobNetwork::AllowList), &[]);
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsv0", "jsv1", "jsv3", "jsp3"])));
    show(
        &fake,
        &[
            (8080, "jsv0", "198.18.0.2", 80),
            (9000, "jsv3", "198.18.0.14", 90),
        ],
    );
    let config = forwarding("open");
    let egress = started_with(&fake, config.clone(), &store);
    let expected = format!(
        "{}add element inet js_crun_egress jobifs {{ \"jsv0\" }}\n\
         add element inet js_crun_egress openifs {{ \"jsv3\" }}\n\
         add element inet js_crun_egress jobifs {{ \"jsv1\" }}\n{}",
        ruleset::render(&config),
        forward::transaction(&[
            forward_of(8080, 2, 80, "jsv0"),
            forward_of(9000, 14, 90, "jsv3")
        ])
    );
    assert_eq!(loaded(&fake), expected);
    let loads = fake
        .log()
        .iter()
        .filter(|call| call.starts_with("nft -f -"))
        .count();
    assert_eq!(loads, 1, "one transaction");
    assert_eq!(
        egress.check(),
        Err(EgressError::Rules),
        "no run holds the forwards yet"
    );
    let next = egress
        .attach("d", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(next.interface(), Some("jsv2"), "blocks 0, 1 and 3 are kept");
}

/// Asserts that records the ceiling, the ports or the pool cannot keep, a block another record
/// has and a file that is not a record are not put back: only the usable records' members are in
/// the load; their interfaces, with an unrecorded `jsv7`, are found as strays; and the settle
/// removes the two kept runs' wiring and records, then the strays' (`jsv2` and `jsp2` of the
/// refused `open` record, which names no kept block, and block 7), and every file of the store:
/// the unusable records' too. Catches a start that keeps wiring it was not asked to keep, or
/// leaves a record file that is no run's.
#[test]
fn records_that_cannot_be_kept_are_not_put_back_and_the_settle_removes_their_files() {
    let dir = scratch("unusable");
    let store = store_in(&dir);
    put(&store, "above", (2, JobNetwork::Open), &[]);
    put(&store, "outside", (3, JobNetwork::AllowList), &[(9101, 80)]);
    put(&store, "pool", (99999, JobNetwork::AllowList), &[]);
    put(&store, "dup-a", (5, JobNetwork::AllowList), &[]);
    put(&store, "dup-b", (5, JobNetwork::AllowList), &[]);
    put(&store, "ok", (1, JobNetwork::AllowList), &[]);
    std::fs::write(store.dir().join("broken.json"), "nope").expect("a file");
    let fake = Fake::new();
    fake.answer(
        "link show",
        Ok(links(&["jsv1", "jsv2", "jsv3", "jsv5", "jsv7"])),
    );
    let config = forwarding("allow-list");
    let egress = started_with(&fake, config.clone(), &store);
    let expected = format!(
        "{}add element inet js_crun_egress jobifs {{ \"jsv5\" }}\n\
         add element inet js_crun_egress jobifs {{ \"jsv1\" }}\n",
        ruleset::render(&config),
    );
    assert_eq!(loaded(&fake), expected);
    assert_eq!(
        recorded(&store).len(),
        7,
        "every file is there until the settle"
    );
    let before = fake.log().len();
    egress.settle().expect("settled");
    let delete =
        |name: &str| format!("nft delete element inet js_crun_egress jobifs {{ \"{name}\" }}");
    assert_eq!(
        fake.log()[before..],
        [
            delete("jsv5"),
            "ip link del jsv5".to_owned(),
            delete("jsv1"),
            "ip link del jsv1".to_owned(),
            "ip link del jsv2".to_owned(),
            "ip link del jsp2".to_owned(),
            "ip link del jsv3".to_owned(),
            "ip link del jsp3".to_owned(),
            "ip link del jsv7".to_owned(),
            "ip link del jsp7".to_owned(),
        ]
    );
    assert_eq!(recorded(&store), Vec::<String>::new());
}

/// Asserts that a forward that does not read back as kept fails the start: a kept forward the
/// tables lack after the load, a forward's lone DNAT rule, a forward's acceptance behind
/// the `inbound` refusal, and one whole forward that is not the recorded one are each `Rules`. Catches a start that goes on with rules it did not
/// load.
#[test]
fn forwards_that_do_not_read_back_as_kept_fail_the_start() {
    let dir = scratch("read-back");
    let store = store_in(&dir);
    put(&store, "r", (0, JobNetwork::AllowList), &[(8080, 80)]);
    let kept = [(8080, "jsv0", "198.18.0.2", 80)];
    let start = |fake: &Arc<Fake>| {
        Egress::start_on(forwarding("allow-list"), Some(store.clone()), fake.clone()).map(drop)
    };
    let fake = Fake::new();
    assert_eq!(
        start(&fake),
        Err(EgressError::Rules),
        "the forward is not there after the load"
    );

    let fake = Fake::new();
    let (_, nat) = tables(&kept);
    fake.answer("list table ip", Ok(nat));
    assert_eq!(
        start(&fake),
        Err(EgressError::Rules),
        "a half of a forward is not a forward"
    );

    let fake = Fake::new();
    let mut late: Value = serde_json::from_str(&tables(&[]).0).expect("json");
    let items = late["nftables"].as_array_mut().expect("items");
    items.push(inbound(7));
    items.push(filter_rule(8080, "jsv0", "198.18.0.2", 80, 8));
    fake.answer("list table inet", Ok(late.to_string()));
    let (_, nat) = tables(&kept);
    fake.answer("list table ip", Ok(nat));
    assert_eq!(
        start(&fake),
        Err(EgressError::Rules),
        "an acceptance behind the inbound refusal accepts nothing"
    );

    let fake = Fake::new();
    let (inet, nat) = tables(&[(8080, "jsv0", "198.18.0.2", 81)]);
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
    assert_eq!(
        start(&fake),
        Err(EgressError::Rules),
        "one whole forward, but to another port than the recorded one"
    );
}

/// Asserts the reading a start makes of the host, with exact texts: the list of interfaces that
/// is not JSON or not a list, or that `ip` refuses, names the listing; a store whose directory is
/// gone fails the start with the reading of the store and its reason, before `ip` is run.
/// Catches a start that guesses what it cannot read.
#[test]
fn what_cannot_be_read_at_the_start_fails_it_with_its_name() {
    let ip = program("ip");
    let start = |fake: &Arc<Fake>| Egress::start_on(allow_list(), None, fake.clone()).map(drop);
    let fake = Fake::new();
    fake.answer("link show", Err("Operation not permitted".to_owned()));
    assert_eq!(
        start(&fake).expect_err("refused").to_string(),
        format!(
            "egress: listing the interfaces: {ip} [\"-j\", \"link\", \"show\"]: Operation not permitted"
        )
    );
    for text in ["nope", "{}"] {
        let fake = Fake::new();
        fake.answer("link show", Ok(text.to_owned()));
        let error = start(&fake).expect_err("not a list");
        assert!(
            error
                .to_string()
                .starts_with("egress: listing the interfaces: "),
            "{text}: {error}"
        );
    }

    let dir = scratch("no-store-dir");
    let store = store_in(&dir);
    std::fs::remove_dir_all(store.dir()).expect("removed");
    let fake = Fake::new();
    let error = Egress::start_on(allow_list(), Some(store), fake.clone()).map(drop);
    assert_eq!(
        error.expect_err("no store").to_string(),
        "egress: reading the egress store: No such file or directory (os error 2)"
    );
    assert_eq!(fake.log(), Vec::<String>::new(), "nothing else was run");
}

/// Asserts the claim of an allow-list run on its own wiring: nothing is run for it (no call
/// is made, and the settle finds nothing of it to do), its record stays, the block is the
/// run's and not given to a new attach, and its release removes the member, the host end and
/// the record and frees the block. Catches an adoption that changes the wiring it takes, or
/// leaves the block free to be given twice.
#[test]
fn adopting_an_allow_list_run_changes_nothing_and_its_release_removes_it_all() {
    let (fake, _dir, store, egress) = left_one(allow_list(), JobNetwork::AllowList, &[]);
    let before = fake.log().len();
    let adopted = egress
        .adopt("r", JobNetwork::AllowList, &[])
        .expect("adopted");
    assert_eq!(adopted.interface(), Some("jsv0"));
    egress.settle().expect("settled");
    assert_eq!(fake.log().len(), before, "no call for the run's own wiring");
    assert_eq!(recorded(&store), ["r"]);
    let other = egress
        .attach("other", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(other.interface(), Some("jsv1"), "block 0 is the run's");
    let released = fake.log().len();
    adopted.release().expect("release");
    assert_eq!(fake.log()[released..], release_calls(0)[..]);
    assert_eq!(
        recorded(&store),
        ["other"],
        "the record goes with the wiring"
    );
    let again = egress
        .attach("again", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(again.interface(), Some("jsv0"), "block 0 is free again");
    drop(other);
}

/// Asserts the claim of a run with ports: its forwards are the run's (the check refuses them
/// before the claim and the settle and passes after, as it does for a run attached in this
/// process; the ports given in another order are the same ports), and its release removes their
/// rules (found by comment, deleted by handle in one transaction), then the member, the host
/// end and the record. Catches an adoption that leaves its forwards out of the record of the
/// runs, so the check refuses them, or a release that leaves their rules.
#[test]
fn adopting_a_run_with_ports_takes_its_forwards_and_releases_them() {
    let ports = [(8080, 80), (9001, 90)];
    let (fake, _dir, store, egress) =
        left_one(forwarding("allow-list"), JobNetwork::AllowList, &ports);
    assert_eq!(egress.check(), Err(EgressError::Rules));
    let adopted = egress
        .adopt("r", JobNetwork::AllowList, &maps(&[(9001, 90), (8080, 80)]))
        .expect("adopted");
    egress.settle().expect("nothing is left");
    egress.check().expect("the forwards are a run's now");
    let before = fake.log().len();
    adopted.release().expect("release");
    assert_eq!(
        fake.log()[before..],
        [
            "nft -j list table inet js_crun_egress",
            "nft -j list table ip js_crun_egress_nat",
            "nft -f - << delete rule inet js_crun_egress forward handle 100",
            "nft delete element inet js_crun_egress jobifs { \"jsv0\" }",
            "ip link del jsv0",
        ]
    );
    let script = fake
        .state()
        .stdins
        .iter()
        .rev()
        .find(|text| text.starts_with("delete rule"))
        .cloned()
        .expect("a script");
    assert_eq!(
        script,
        "delete rule inet js_crun_egress forward handle 100\n\
         delete rule inet js_crun_egress forward handle 101\n\
         delete rule ip js_crun_egress_nat prerouting handle 200\n\
         delete rule ip js_crun_egress_nat prerouting handle 201\n"
    );
    assert_eq!(recorded(&store), Vec::<String>::new());
}

/// Asserts the claim of an `open` run: its record names `open`, the ceiling is `open`, and its
/// release removes its host end from `openifs`. Catches an `open` run taken over into the
/// other set's rules.
#[test]
fn adopting_an_open_run_takes_its_member_of_the_open_set() {
    let (fake, _dir, _store, egress) = left_one(open_config(), JobNetwork::Open, &[]);
    let adopted = egress.adopt("r", JobNetwork::Open, &[]).expect("adopted");
    let before = fake.log().len();
    adopted.release().expect("release");
    assert_eq!(
        fake.log()[before..],
        [
            "nft delete element inet js_crun_egress openifs { \"jsv0\" }",
            "ip link del jsv0"
        ]
    );
}

fn open_config() -> Config {
    config_with("open", None)
}

/// Asserts that a run with no network has nothing to take over and a level above the ceiling
/// is refused before anything is looked up: a `none` job gets an attachment with no interface
/// and no call is made, under every ceiling; an `open` job under `allow-list` is
/// `AboveCeiling`; mode none refuses an `allow-list` job the same way and settles to nothing.
/// Catches an adoption that wires a run that has no wiring.
#[test]
fn a_none_job_has_nothing_to_take_over_and_a_level_above_the_ceiling_is_refused() {
    for config in [allow_list(), open_config(), config_with("none", None)] {
        let fake = Fake::new();
        let egress = started(&fake, config);
        let before = fake.calls();
        let attachment = egress
            .adopt("r", JobNetwork::None, &[])
            .expect("nothing to take over");
        assert_eq!(attachment.interface(), None);
        drop(attachment);
        egress.settle().expect("nothing to settle");
        assert_eq!(fake.calls(), before);
    }
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.log().len();
    let above = egress.adopt("r", JobNetwork::Open, &[]).map(drop);
    assert_eq!(
        above,
        Err(EgressError::AboveCeiling {
            asked: JobNetwork::Open,
            ceiling: JobNetwork::AllowList
        })
    );
    let fake_none = Fake::new();
    let none = started(&fake_none, config_with("none", None));
    let above = none.adopt("r", JobNetwork::AllowList, &[]).map(drop);
    assert_eq!(
        above,
        Err(EgressError::AboveCeiling {
            asked: JobNetwork::AllowList,
            ceiling: JobNetwork::None
        })
    );
    assert_eq!(fake.log().len(), before);
    assert_eq!(fake_none.log(), Vec::<String>::new());
}

/// Asserts each refusal of an adoption with its exact text, and that each leaves the record
/// and the wiring for the settle: a run the store has no record of (no store at all gives the
/// same), a record of another level either way (an `open` ceiling allows both), and a record of
/// other ports (none declared, another container port, one more). Catches an adoption that
/// takes a wiring that is not the run's.
#[test]
fn an_adoption_that_does_not_fit_is_refused_and_leaves_the_record_to_the_settle() {
    let allow = (JobNetwork::AllowList, &[(8080, 80)][..]);
    let same = ("r", JobNetwork::AllowList, &[(8080, 80)][..]);
    let no_record = cannot("other", "the egress store holds no record of it");
    let other = ("other", JobNetwork::AllowList, &[(8080, 80)][..]);
    assert_eq!(refusal(forwarding("allow-list"), allow, other), no_record);

    let level = |kept: JobNetwork, job: JobNetwork| {
        refusal(forwarding("open"), (kept, &[]), ("r", job, &[]))
    };
    assert_eq!(
        level(JobNetwork::AllowList, JobNetwork::Open),
        cannot("r", "its record names the level allow-list, its job open")
    );
    assert_eq!(
        level(JobNetwork::Open, JobNetwork::AllowList),
        cannot("r", "its record names the level open, its job allow-list")
    );

    let other_ports = cannot("r", "its record names other ports than its job declared");
    for ports in [&[][..], &[(8080, 81)], &[(8080, 80), (9000, 90)]] {
        let wrong = refusal(forwarding("allow-list"), allow, ("r", same.1, ports));
        assert_eq!(wrong, other_ports, "{ports:?}");
    }

    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let error = egress.adopt("r", JobNetwork::AllowList, &[]).map(drop);
    assert_eq!(
        error.expect_err("no store").to_string(),
        cannot("r", "the egress store holds no record of it")
    );
}

/// Asserts that a host port another run of this agent forwards refuses the adoption with
/// `PortTaken` and its text, and that the record and the block stay for the settle. Catches an
/// adoption that forwards one host port to two runs.
#[test]
fn a_host_port_another_run_forwards_refuses_the_adoption() {
    let (fake, _dir, store, egress) = left_one(
        forwarding("allow-list"),
        JobNetwork::AllowList,
        &[(8080, 80)],
    );
    show(&fake, &[(8080, "jsv1", "198.18.0.6", 80)]);
    let other = egress
        .attach("other", PID, JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .expect("attach");
    assert_eq!(other.interface(), Some("jsv1"));
    let error = egress
        .adopt("r", JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .map(drop);
    let error = error.expect_err("taken");
    assert_eq!(error, EgressError::PortTaken { port: 8080 });
    assert_eq!(
        error.to_string(),
        "egress: host port 8080 is forwarded to another run already"
    );
    assert_eq!(recorded(&store), ["other", "r"], "the record stays");
    let before = fake.log().len();
    drop(egress);
    assert_eq!(fake.log().len(), before);
    drop(other);
}

/// Asserts what a settle removes and in what order: for each record no run took over its
/// forwards' rules (listed, then deleted by handle), its member and its host end, in the order
/// of the runs' names, and then its record; then each stray block's two ends; the blocks are
/// free afterwards, a second settle makes no call, and the check, which refused the kept
/// forwards, passes once they are gone. Catches a settle that leaves wiring or a record behind
/// or removes more than was left.
#[test]
fn settle_removes_what_no_run_took_over_once() {
    let dir = scratch("settle");
    let store = store_in(&dir);
    put(&store, "a", (0, JobNetwork::AllowList), &[(8080, 80)]);
    put(&store, "b", (1, JobNetwork::AllowList), &[]);
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsv0", "jsv1", "jsp4"])));
    show(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let egress = started_with(&fake, forwarding("allow-list"), &store);
    assert_eq!(egress.check(), Err(EgressError::Rules));
    let before = fake.log().len();
    egress.settle().expect("settled");
    assert_eq!(
        fake.log()[before..],
        [
            "nft -j list table inet js_crun_egress",
            "nft -j list table ip js_crun_egress_nat",
            "nft -f - << delete rule inet js_crun_egress forward handle 100",
            "nft delete element inet js_crun_egress jobifs { \"jsv0\" }",
            "ip link del jsv0",
            "nft delete element inet js_crun_egress jobifs { \"jsv1\" }",
            "ip link del jsv1",
            "ip link del jsv4",
            "ip link del jsp4",
        ]
    );
    assert_eq!(recorded(&store), Vec::<String>::new());
    show(&fake, &[]);
    egress.check().expect("no forward is left");
    let after = fake.log().len();
    egress.settle().expect("a second settle");
    assert_eq!(fake.log().len(), after, "nothing is left to do");
    let attached: Vec<Attachment> = (0..5)
        .map(|n| {
            egress
                .attach(&format!("r{n}"), PID, JobNetwork::AllowList, &[])
                .expect("attach")
        })
        .collect();
    let names: Vec<_> = attached.iter().map(Attachment::interface).collect();
    assert_eq!(
        names,
        [
            Some("jsv0"),
            Some("jsv1"),
            Some("jsv2"),
            Some("jsv3"),
            Some("jsv4")
        ]
    );
}

/// Asserts that a removal that fails keeps its block out of use and its record in the store,
/// and does not stop the rest: with the deletion of `jsv0` (a kept record's wiring) and of the
/// job end `jsp4` refused, `jsv1` and the stray block 6 are still removed, the first error is
/// the one reported (the veth of `jsv0`), the record of `jsv0`'s run stays and the other is
/// gone, and blocks 0 and 4 are not given to a new run while 1, 2, 3, 5 and 6 are. Catches a
/// settle that gives out a name a firewall set may still hold, forgets a run whose wiring is
/// still there, or stops at its first failure.
#[test]
fn a_settle_that_cannot_remove_a_block_keeps_it_and_its_record_and_reports_the_first_error() {
    let dir = scratch("settle-fails");
    let store = store_in(&dir);
    put(&store, "a", (0, JobNetwork::AllowList), &[]);
    put(&store, "b", (1, JobNetwork::AllowList), &[]);
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsv0", "jsv1", "jsp4", "jsp6"])));
    let egress = started_with(&fake, allow_list(), &store);
    fake.answer("link del jsv0", Err("Operation not permitted".to_owned()));
    fake.answer("link del jsp4", Err("Device busy".to_owned()));
    let before = fake.log().len();
    let error = egress.settle().expect_err("two removals fail");
    assert!(
        error.to_string().starts_with("egress: deleting the veth: ")
            && error.to_string().ends_with("Operation not permitted"),
        "{error}"
    );
    assert_eq!(
        fake.log()[before..],
        [
            "nft delete element inet js_crun_egress jobifs { \"jsv0\" }",
            "ip link del jsv0",
            "nft delete element inet js_crun_egress jobifs { \"jsv1\" }",
            "ip link del jsv1",
            "ip link del jsv4",
            "ip link del jsp4",
            "ip link del jsv6",
            "ip link del jsp6",
        ]
    );
    assert_eq!(recorded(&store), ["a"]);
    let attached: Vec<Attachment> = (0..5)
        .map(|n| {
            egress
                .attach(&format!("r{n}"), PID, JobNetwork::AllowList, &[])
                .expect("attach")
        })
        .collect();
    let names: Vec<_> = attached.iter().map(Attachment::interface).collect();
    assert_eq!(
        names,
        [
            Some("jsv1"),
            Some("jsv2"),
            Some("jsv3"),
            Some("jsv5"),
            Some("jsv6")
        ]
    );
}

/// Asserts that a stray job end alone is deleted with the text of what was being done: `jsp4`
/// failing is "deleting a leftover veth" and is reported when it is the only failure. Catches a
/// settle that hides the failure of a job end left alone.
#[test]
fn a_stray_job_end_that_cannot_be_deleted_is_reported() {
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsp4"])));
    let egress = started(&fake, allow_list());
    fake.answer("link del jsp4", Err("Device busy".to_owned()));
    let error = egress.settle().expect_err("fails");
    assert_eq!(
        error.to_string(),
        format!(
            "egress: deleting a leftover veth: {} [\"link\", \"del\", \"jsp4\"]: Device busy",
            program("ip")
        )
    );
}

/// Asserts that a record that cannot be removed is the settle's error with its text, whether
/// the run's wiring was kept (`x`, whose record file became a directory after the start) or the
/// record was unusable (`y.json`, a directory at the start), that the wiring of `x` is removed
/// all the same while its block stays out of use (its record still names it), and that the
/// first error is `x`'s. Catches a settle that reports success while a record stays, and one
/// that gives back a block a record still names.
#[test]
fn a_record_that_cannot_be_removed_is_the_settles_error() {
    let dir = scratch("settle-record");
    let store = store_in(&dir);
    put(&store, "x", (0, JobNetwork::AllowList), &[]);
    std::fs::create_dir(store.dir().join("y.json")).expect("a directory");
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsv0"])));
    let egress = started_with(&fake, allow_list(), &store);
    let path = store.dir().join("x.json");
    std::fs::remove_file(&path).expect("removed");
    std::fs::create_dir_all(path.join("inside")).expect("a directory");
    std::fs::create_dir_all(store.dir().join("y.json").join("inside")).expect("a directory");
    let before = fake.log().len();
    let error = egress.settle().expect_err("records stay");
    assert_eq!(
        error.to_string(),
        "egress: removing the egress record of x: Is a directory (os error 21)"
    );
    assert_eq!(fake.log()[before..], release_calls(0)[..]);
    assert_eq!(recorded(&store), ["x", "y"]);
    let next = egress
        .attach("n", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(
        next.interface(),
        Some("jsv1"),
        "block 0 stays out of use while its record stays"
    );
}

/// Asserts that leaving an attachment removes nothing, now or when it is dropped, and that the
/// wiring and its record are the next process's: the log does not grow, with the forwards of a
/// run that has ports, the record is still in the store, and the host stays held by the egress
/// that is still alive. Catches a hand-over that takes down the wiring of a run that goes on.
#[test]
fn a_left_attachment_removes_nothing_and_keeps_its_record() {
    let dir = scratch("leave");
    let store = store_in(&dir);
    let fake = Fake::new();
    let egress = started_with(&fake, forwarding("allow-list"), &store);
    show(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let wired = egress
        .attach("r", PID, JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .expect("attach");
    assert_eq!(wired.interface(), Some("jsv0"));
    let before = fake.log().len();
    wired.leave();
    assert_eq!(fake.log().len(), before, "left, not removed");
    assert_eq!(recorded(&store), ["r"]);
}

/// Asserts that the crun runtime's port takes over, settles and leaves through the host side
/// itself: through `EgressPort` an adoption claims the run's record and runs nothing, a run with
/// no record is refused with the host side's exact text, the settle removes what no run took
/// over, and `Wired::leave` of the adopted wiring runs nothing and keeps its record. Catches a
/// port that adopts or settles by anything but the host side, or a leave that takes the
/// wiring down.
#[test]
fn the_crun_runtimes_port_takes_over_settles_and_leaves_through_the_host_side() {
    use crate::crun::{EgressPort, Wired};
    let (fake, _dir, store, egress) = left_one(allow_list(), JobNetwork::AllowList, &[]);
    let port: Arc<dyn EgressPort> = crate::crun::port_of(&egress);
    let before = fake.log().len();
    let wired = port
        .adopt("r", JobNetwork::AllowList, &[])
        .expect("adopted");
    let refused = port
        .adopt("nobody", JobNetwork::AllowList, &[])
        .map(drop)
        .expect_err("no record");
    assert_eq!(
        refused.to_string(),
        cannot("nobody", "the egress store holds no record of it")
    );
    port.settle().expect("settled");
    Wired::leave(wired);
    assert_eq!(fake.log().len(), before, "nothing was run");
    assert_eq!(recorded(&store), ["r"], "the record is the next process's");
}
