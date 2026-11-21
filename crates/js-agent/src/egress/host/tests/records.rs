//! The runs' records in the egress store against the scripted machine: written when a run is
//! wired, removed once its wiring is down and kept while any of it may be there. The records
//! are real files in a scratch directory of each test; the helpers here are the takeover tests'
//! too.

use js_testkit::dir::TestDir;
use serde_json::json;

use super::*;
use crate::egress::forward::fixture::{filter_rule, nat_rule};

/// A scratch directory of the test's own, removed when it is dropped.
pub(in crate::egress::host) fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("js-egress-{name}"))
}

/// The store of the scratch directory `dir`.
pub(in crate::egress::host) fn store_in(dir: &TestDir) -> Store {
    Store::open(dir).expect("a store")
}

/// An egress started with the store `store`, which the runs it wires are recorded in.
pub(in crate::egress::host) fn started_with(
    fake: &Arc<Fake>,
    config: Config,
    store: &Store,
) -> Egress {
    Egress::start_on(config, Some(store.clone()), fake.clone()).expect("starts")
}

/// A configuration with forward ports `9000-9100` and `8080`.
pub(in crate::egress::host) fn forwarding(mode: &str) -> Config {
    let (ip, nft, nsenter) = (program("ip"), program("nft"), program("nsenter"));
    let allow = vec!["192.0.2.10:9300".to_owned()];
    let ports = vec!["9000-9100".to_owned(), "8080".to_owned()];
    Config::from_flags(&Flags {
        mode: Some(mode),
        allow: &allow,
        forward_ports: &ports,
        ip: Some(&ip),
        nft: Some(&nft),
        nsenter: Some(&nsenter),
        ..Flags::default()
    })
    .expect("a sound configuration")
}

/// The declared ports `host -> container`.
pub(in crate::egress::host) fn maps(pairs: &[(u16, u16)]) -> Vec<PortMap> {
    pairs
        .iter()
        .map(|&(host, container)| PortMap { host, container })
        .collect()
}

/// Writes the record of `run` into `store`: its block, level and ports.
pub(in crate::egress::host) fn put(
    store: &Store,
    run: &str,
    (index, level): (u32, JobNetwork),
    ports: &[(u16, u16)],
) {
    let record = Record {
        index,
        level,
        ports: maps(ports),
    };
    store.write(run, &record).expect("a record");
}

/// The names of the runs the store holds a record of.
pub(in crate::egress::host) fn recorded(store: &Store) -> Vec<String> {
    let all = store.records().expect("listed");
    all.into_iter().map(|(run, _)| run).collect()
}

/// The text of both tables as an earlier process left them: the forwards `forwards` (host
/// port, interface, job address, container port), each acceptance with the handle `100 + n`
/// and its DNAT rule `200 + n`.
pub(in crate::egress::host) fn tables(forwards: &[(u16, &str, &str, u16)]) -> (String, String) {
    let (mut filters, mut nats) = (Vec::new(), Vec::new());
    for (n, &(host, iface, job, container)) in (0u64..).zip(forwards) {
        filters.push(filter_rule(host, iface, job, container, 100 + n));
        nats.push(nat_rule(host, job, container, 200 + n));
    }
    let mut inet = vec![
        json!({"metainfo": {"version": "1.0.9"}}),
        json!({"table": {"family": "inet", "name": "js_crun_egress", "handle": 1}}),
        json!({"set": {"name": "jobifs", "type": "ifname", "handle": 2}}),
        json!({"set": {"name": "openifs", "type": "ifname", "handle": 3}}),
    ];
    inet.extend(filters);
    inet.push(
        json!({"rule": {"chain": "forward", "handle": 3, "comment": "reply",
        "expr": [{"counter": {"packets": 0, "bytes": 0}}, {"accept": null}]}}),
    );
    let mut nat = vec![
        json!({"metainfo": {"version": "1.0.9"}}),
        json!({"table": {"family": "ip", "name": "js_crun_egress_nat", "handle": 1}}),
    ];
    nat.extend(nats);
    (
        json!({ "nftables": inet }).to_string(),
        json!({ "nftables": nat }).to_string(),
    )
}

/// Makes both tables read as holding the forwards `forwards` (see [`tables`]).
pub(in crate::egress::host) fn show(fake: &Fake, forwards: &[(u16, &str, &str, u16)]) {
    let (inet, nat) = tables(forwards);
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
}

/// Asserts the record an attach writes, byte for byte: the block, the level's name and the
/// ports in the order the job declared them, in the file named after the run; an `open` run's
/// says `open`; a `none` run, and a run of an agent with no store, have none. Catches a record
/// that names another block or level than the wiring it stands for, so that the next process
/// puts the wrong rules back.
#[test]
fn an_attach_writes_the_record_of_its_wiring() {
    let dir = scratch("attach-record");
    let store = store_in(&dir);
    let fake = Fake::new();
    let egress = started_with(&fake, forwarding("open"), &store);
    show(&fake, &[(8080, "jsv0", "198.18.0.2", 80)]);
    let first = egress
        .attach("js-x.one", PID, JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .expect("attach");
    let second = egress
        .attach("two", PID, JobNetwork::Open, &[])
        .expect("attach");
    let none = egress
        .attach("three", PID, JobNetwork::None, &[])
        .expect("nothing to wire");
    let text = |run: &str| std::fs::read_to_string(store.dir().join(format!("{run}.json")));
    assert_eq!(
        text("js-x.one").expect("a record"),
        r#"{"version":1,"index":0,"level":"allow-list","ports":[{"host":8080,"container":80}]}"#
    );
    assert_eq!(
        text("two").expect("a record"),
        r#"{"version":1,"index":1,"level":"open","ports":[]}"#
    );
    assert_eq!(recorded(&store), ["js-x.one", "two"]);
    drop((first, second, none));
    assert_eq!(recorded(&store), Vec::<String>::new());
}

/// Asserts that a run whose name no record can have (`bad/run`) is refused before anything is
/// wired, with the exact text and no call at all. Catches a run wired with no record, which the
/// next process could neither take over nor clean up.
#[test]
fn a_run_no_record_can_name_is_refused_before_it_is_wired() {
    let dir = scratch("attach-unnamed");
    let store = store_in(&dir);
    let fake = Fake::new();
    let egress = started_with(&fake, allow_list(), &store);
    let before = fake.log().len();
    let error = egress
        .attach("bad/run", PID, JobNetwork::AllowList, &[])
        .map(drop)
        .expect_err("no record");
    assert_eq!(
        error.to_string(),
        "egress: recording the egress of run bad/run: \"bad/run\" is not a name a record can have"
    );
    assert_eq!(fake.log().len(), before, "nothing was run");
    let next = egress
        .attach("fine", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(next.interface(), Some("jsv0"), "no block was taken");
}

/// Asserts that a record that cannot be written takes the run's wiring down: the write fails
/// (a directory stands where its temporary file goes), the attach is an error with the exact
/// text, the calls are the wiring and then its removal, no record is left, and the block is the
/// next run's. Catches a run that goes on wired with no record, which the next process could
/// not take over or clean up.
#[test]
fn a_record_that_cannot_be_written_takes_the_wiring_down() {
    let dir = scratch("attach-fails");
    let store = store_in(&dir);
    std::fs::create_dir(store.dir().join(".stuck.json.tmp")).expect("a directory");
    let fake = Fake::new();
    let egress = started_with(&fake, allow_list(), &store);
    let before = fake.log().len();
    let error = egress
        .attach("stuck", PID, JobNetwork::AllowList, &[])
        .map(drop)
        .expect_err("no record");
    assert_eq!(
        error.to_string(),
        "egress: recording the egress of run stuck: Is a directory (os error 21)"
    );
    let mut calls = attach_calls();
    calls.extend(release_calls(0));
    assert_eq!(fake.log()[before..], calls[..]);
    assert_eq!(recorded(&store), Vec::<String>::new());
    let next = egress
        .attach("fine", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(next.interface(), Some("jsv0"), "the block is free");
}

/// Asserts the order of a release: the record goes only after the wiring is down. A release
/// whose veth cannot be deleted is an error and leaves the record (and the block out of use)
/// for the next start to put back and settle; one that succeeds removes it; and a record that
/// cannot be removed is the release's error, with its block kept out of use (the record still
/// names it, and a later start would put it back for whichever run held the block then).
/// Catches a release that forgets a run whose wiring is still on the host, and one that gives
/// back a block a record still names.
#[test]
fn a_release_removes_the_record_only_after_the_wiring_is_down() {
    let dir = scratch("release-record");
    let store = store_in(&dir);
    let fake = Fake::new();
    let egress = started_with(&fake, allow_list(), &store);
    let stuck = egress
        .attach("stuck", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    fake.answer("link del jsv0", Err("Operation not permitted".to_owned()));
    let error = stuck.release().expect_err("the veth stays");
    assert!(
        error.to_string().starts_with("egress: deleting the veth: "),
        "{error}"
    );
    assert_eq!(recorded(&store), ["stuck"], "the wiring may be there");

    let done = egress
        .attach("done", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(done.interface(), Some("jsv1"), "block 0 stays out of use");
    assert_eq!(recorded(&store), ["done", "stuck"]);
    done.release().expect("release");
    assert_eq!(recorded(&store), ["stuck"], "removed with the wiring");

    let held = egress
        .attach("held", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    let path = store.dir().join("held.json");
    std::fs::remove_file(&path).expect("removed");
    std::fs::create_dir_all(path.join("inside")).expect("a directory");
    let error = held.release().expect_err("the record stays");
    assert_eq!(
        error.to_string(),
        "egress: removing the egress record of held: Is a directory (os error 21)"
    );
    let again = egress
        .attach("again", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(
        again.interface(),
        Some("jsv2"),
        "block 1 stays out of use while its record stays"
    );
}
