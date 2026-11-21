//! Taking over the wiring an earlier process left, against the scripted machine: what a start
//! finds and keeps, the one transaction that puts it back, the claim a run makes on its own
//! wiring, and the settle that removes what no run claimed.

use std::net::Ipv4Addr;

use serde_json::{Value, json};

use super::*;
use crate::egress::forward::fixture::{filter_rule, inbound, nat_rule};

/// The text of both tables as an earlier process left them: `jobifs` and `openifs` with the
/// given members, and the forwards `forwards` (host port, interface, job address, container
/// port), each acceptance with the handle `100 + n` and its DNAT rule `200 + n`.
fn tables(
    jobifs: &[&str],
    openifs: &[&str],
    forwards: &[(u16, &str, &str, u16)],
) -> (String, String) {
    let (mut filters, mut nats) = (Vec::new(), Vec::new());
    for (n, &(host, iface, job, container)) in (0u64..).zip(forwards) {
        filters.push(filter_rule(host, iface, job, container, 100 + n));
        nats.push(nat_rule(host, job, container, 200 + n));
    }
    let mut inet = vec![
        json!({"metainfo": {"version": "1.0.9"}}),
        json!({"table": {"family": "inet", "name": "js_crun_egress", "handle": 1}}),
        json!({"set": {"name": "jobifs", "type": "ifname", "handle": 2, "elem": jobifs}}),
        json!({"set": {"name": "openifs", "type": "ifname", "handle": 3, "elem": openifs}}),
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

/// Makes the machine hold what an earlier process left: the interfaces `names` (the
/// interface index of each is 10 plus its place in the list), and `tables`.
fn leave(
    fake: &Fake,
    names: &[&str],
    jobifs: &[&str],
    openifs: &[&str],
    forwards: &[(u16, &str, &str, u16)],
) {
    let (inet, nat) = tables(jobifs, openifs, forwards);
    fake.answer("link show", Ok(links(names)));
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
}

/// Makes the `eth0` of the container read as the job end of the pair whose host end has the
/// interface index `peer`, with the addresses `(family, address, prefix length)`.
fn job_end(fake: &Fake, peer: u64, addresses: &[(&str, &str, u64)]) {
    let info: Vec<Value> = addresses
        .iter()
        .map(|(family, local, len)| json!({"family": family, "local": local, "prefixlen": len}))
        .collect();
    let end = json!([{"ifname": "eth0", "link_index": peer, "addr_info": info}]);
    fake.answer("addr show dev eth0", Ok(end.to_string()));
}

/// A configuration with forward ports `9000-9100` and `8080`.
fn forwarding(mode: &str) -> Config {
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

fn maps(pairs: &[(u16, u16)]) -> Vec<PortMap> {
    pairs
        .iter()
        .map(|&(host, container)| PortMap { host, container })
        .collect()
}

fn forward_of(host: u16, last: u8, container: u16, iface: &str) -> Forward {
    Forward {
        host,
        job: Ipv4Addr::new(198, 18, 0, last),
        container,
        iface: iface.to_owned(),
    }
}

/// The text of the refusal of an adoption of container `PID`.
fn cannot(why: &str) -> String {
    format!("egress: the wiring of container {PID} cannot be taken over: {why}")
}

/// The machine of an earlier process that left `jsv0` (interface index 10, in `jobifs`, with
/// `forwards`) and whose container reads as its job end, started.
fn left_one(config: Config, forwards: &[(u16, &str, &str, u16)]) -> (Arc<Fake>, Egress) {
    let fake = Fake::new();
    leave(&fake, &["jsv0"], &["jsv0"], &[], forwards);
    job_end(&fake, 10, &[("inet", "198.18.0.2", 30)]);
    let egress = started(&fake, config);
    (fake, egress)
}

/// A way to spoil the machine for an adoption.
type Spoil = Box<dyn Fn(&Fake)>;

/// What an adoption that `setup` spoils is refused with, and that the refusal took nothing:
/// the block `jsv0` is still the settle's to remove.
fn refusal(
    config: Config,
    forwards: &[(u16, &str, &str, u16)],
    setup: &dyn Fn(&Fake),
    (level, ports): (JobNetwork, &[(u16, u16)]),
) -> String {
    let (fake, egress) = left_one(config, forwards);
    setup(&fake);
    let error = egress
        .adopt("r", PID, level, &maps(ports))
        .expect_err("refused");
    let before = fake.log().len();
    egress.settle().expect("settled");
    assert!(
        fake.log()[before..].contains(&"ip link del jsv0".to_owned()),
        "{:?}",
        fake.log()
    );
    error.to_string()
}

/// Asserts the start's one load, in full: the ruleset, then each kept member back in its set
/// (jsv0 and jsv3 in `jobifs`; jsv1, in both sets, and jsv2, in `openifs` that an allow-list
/// does not have, in none), then the whole forwards of the interfaces still there (a member
/// and a forward of `jsv9`, which is not on the host, are dropped), sorted; and that the read
/// back accepts it. Catches a start that loses a wired run's rules, keeps those of a run that
/// is gone, or widens a member's access.
#[test]
fn the_start_loads_the_ruleset_and_what_it_keeps_in_one_transaction() {
    let fake = Fake::new();
    let kept = [
        (9000, "jsv3", "198.18.0.14", 90),
        (8080, "jsv0", "198.18.0.2", 80),
    ];
    leave(&fake, &["jsv0", "jsv1", "jsv2", "jsv3"], &[], &[], &kept);
    let stale = [&kept[..], &[(7000, "jsv9", "198.18.0.38", 70)]].concat();
    let (inet, nat) = tables(&["jsv0", "jsv1", "jsv3", "jsv9"], &["jsv1", "jsv2"], &stale);
    fake.answer_once("list table inet", Ok(inet));
    fake.answer_once("list table ip", Ok(nat));
    let egress = started(&fake, allow_list());
    let expected = format!(
        "{}add element inet js_crun_egress jobifs {{ \"jsv0\" }}\n\
         add element inet js_crun_egress jobifs {{ \"jsv3\" }}\n{}",
        ruleset::render(&allow_list()),
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
        "no run holds them yet"
    );
}

/// Asserts that a table that is gone keeps nothing of it: with the filter table gone at the
/// start's listing (`nft`'s "No such file or directory"), and with only the NAT table gone, the
/// interface is kept with no member and no forward, the load is the ruleset alone, and the
/// settle deletes the host end and nothing else. Catches a start that fails on a host where an
/// earlier process's tables are not there, or that keeps members of tables it cannot read.
#[test]
fn a_table_that_is_gone_keeps_no_member_and_no_forward() {
    for gone in ["list table inet", "list table ip"] {
        let fake = Fake::new();
        fake.answer("link show", Ok(links(&["jsv0"])));
        let (inet, _) = tables(&["jsv0"], &[], &[(8080, "jsv0", "198.18.0.2", 80)]);
        if gone.ends_with("ip") {
            fake.answer_once("list table inet", Ok(inet));
        }
        fake.answer_once(
            gone,
            Err("Error: No such file or directory; did you mean table?".to_owned()),
        );
        let egress = started(&fake, allow_list());
        assert_eq!(loaded(&fake), ruleset::render(&allow_list()), "{gone}");
        let before = fake.log().len();
        egress.settle().expect("settled");
        assert_eq!(fake.log()[before..], ["ip link del jsv0"], "{gone}");
    }
}

/// Asserts that a forward that does not read back as kept fails the start: a kept forward
/// the tables lack after the load, a forward's lone DNAT rule, and a forward's acceptance
/// behind the `inbound` refusal are each `Rules`, and the start holds nothing then. Catches a
/// start that goes on with rules it did not load.
#[test]
fn forwards_that_do_not_read_back_as_kept_fail_the_start() {
    let kept = [(8080, "jsv0", "198.18.0.2", 80)];
    let fake = Fake::new();
    let (inet, nat) = tables(&["jsv0"], &[], &kept);
    fake.answer("link show", Ok(links(&["jsv0"])));
    fake.answer_once("list table inet", Ok(inet));
    fake.answer_once("list table ip", Ok(nat));
    assert_eq!(
        Egress::start_on(allow_list(), fake.clone()).map(drop),
        Err(EgressError::Rules),
        "the forward is not there after the load"
    );

    let fake = Fake::new();
    let (inet, _) = tables(&[], &[], &[]);
    let (_, nat) = tables(&[], &[], &kept);
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
    assert_eq!(
        Egress::start_on(allow_list(), fake.clone()).map(drop),
        Err(EgressError::Rules),
        "a half of a forward is not a forward"
    );

    let fake = Fake::new();
    let mut late: Value = serde_json::from_str(&tables(&[], &[], &[]).0).expect("json");
    let items = late["nftables"].as_array_mut().expect("items");
    items.push(inbound(7));
    items.push(filter_rule(8080, "jsv0", "198.18.0.2", 80, 8));
    fake.answer("list table inet", Ok(late.to_string()));
    assert_eq!(
        Egress::start_on(allow_list(), fake.clone()).map(drop),
        Err(EgressError::Rules),
        "an acceptance behind the inbound refusal accepts nothing"
    );
}

/// Asserts the reading of the interfaces: one with no interface index is an error that names
/// it (`jsv0 has no ifindex`), a listing that is not JSON names the listing, and the reading
/// of the old tables names itself when `nft` fails for a reason other than the table being
/// gone, when it cannot be started, and when either table is not JSON. Catches a start that
/// guesses what it cannot read.
#[test]
fn what_cannot_be_read_of_the_old_wiring_fails_the_start_with_its_name() {
    let fake = Fake::new();
    fake.answer("link show", Ok(r#"[{"ifname":"jsv0"}]"#.to_owned()));
    let error = Egress::start_on(allow_list(), fake.clone()).map(drop);
    assert_eq!(
        error.expect_err("no ifindex").to_string(),
        "egress: listing the interfaces: jsv0 has no ifindex"
    );

    let fake = Fake::new();
    fake.answer("link show", Ok("{}".to_owned()));
    let error = Egress::start_on(allow_list(), fake.clone()).map(drop);
    let text = error.expect_err("not a list").to_string();
    assert!(
        text.starts_with("egress: listing the interfaces: "),
        "{text}"
    );

    let nft = program("nft");
    let args = |table: &str| format!("[\"-j\", \"list\", \"table\", {table}]");
    let fake = Fake::new();
    fake.answer(
        "list table inet",
        Err("Error: Operation not permitted".to_owned()),
    );
    let error = Egress::start_on(allow_list(), fake.clone()).map(drop);
    assert_eq!(
        error.expect_err("refused").to_string(),
        format!(
            "egress: reading the firewall an earlier process left: {nft} {}: Error: Operation not permitted",
            args("\"inet\", \"js_crun_egress\"")
        )
    );

    let fake = Fake::new();
    fake.not_run("list table ip");
    let error = Egress::start_on(allow_list(), fake.clone()).map(drop);
    let text = error.expect_err("not run").to_string();
    assert!(
        text.starts_with("egress: reading the firewall an earlier process left: ")
            && text.contains("not run: "),
        "{text}"
    );

    for table in ["list table inet", "list table ip"] {
        let fake = Fake::new();
        fake.answer(table, Ok("nope".to_owned()));
        let error = Egress::start_on(allow_list(), fake.clone()).map(drop);
        let text = error.expect_err("not JSON").to_string();
        assert!(
            text.starts_with("egress: reading the firewall an earlier process left: "),
            "{table}: {text}"
        );
    }
    let fake = Fake::new();
    fake.answer("list table ip", Ok(r#"{"other":1}"#.to_owned()));
    let error = Egress::start_on(allow_list(), fake.clone()).map(drop);
    assert_eq!(
        error.expect_err("not nft output").to_string(),
        "egress: reading the firewall an earlier process left: nft output has no nftables list"
    );
}

/// Asserts the claim of an allow-list run on its own wiring: the container's `eth0` is read
/// once through `nsenter` (and an IPv6 address beside the one /30 address is no matter), no
/// other program runs, the block is the run's and not given to a new attach, a settle finds
/// nothing of it, and its release removes the member and the host end and frees the block.
/// Catches an adoption that changes the wiring it takes, or leaves the block free to be
/// given twice.
#[test]
fn adopting_an_allow_list_run_reads_its_end_and_changes_nothing() {
    let (fake, egress) = left_one(allow_list(), &[]);
    job_end(
        &fake,
        10,
        &[("inet6", "fe80::1", 64), ("inet", "198.18.0.2", 30)],
    );
    let before = fake.log().len();
    let adopted = egress
        .adopt("r", PID, JobNetwork::AllowList, &[])
        .expect("adopted");
    assert_eq!(adopted.interface(), Some("jsv0"));
    assert_eq!(
        fake.log()[before..],
        [format!(
            "nsenter --net={NETFD} -- {} -j addr show dev eth0",
            program("ip")
        )]
    );
    let after = fake.log().len();
    egress.settle().expect("settled");
    assert_eq!(fake.log().len(), after, "nothing of it is left to remove");
    let other = egress
        .attach("other", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(other.interface(), Some("jsv1"), "block 0 is the run's");
    let released = fake.log().len();
    adopted.release().expect("release");
    assert_eq!(fake.log()[released..], release_calls(0)[..]);
    let again = egress
        .attach("again", PID, JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(again.interface(), Some("jsv0"), "block 0 is free again");
    drop(other);
}

/// Asserts the claim of a run with ports: its forwards are the run's (the check passes with
/// them, as it does for a run attached in this process), and its release removes their rules
/// (found by comment, deleted by handle in one transaction), then the member and the host end.
/// Catches an adoption that leaves its forwards out of the record of the runs, so the check
/// refuses them, or a release that leaves their rules.
#[test]
fn adopting_a_run_with_ports_takes_its_forwards_and_releases_them() {
    let kept = [
        (8080, "jsv0", "198.18.0.2", 80),
        (9001, "jsv0", "198.18.0.2", 90),
    ];
    let (fake, egress) = left_one(forwarding("allow-list"), &kept);
    assert_eq!(egress.check(), Err(EgressError::Rules));
    let adopted = egress
        .adopt(
            "r",
            PID,
            JobNetwork::AllowList,
            &maps(&[(9001, 90), (8080, 80)]),
        )
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
}

/// Asserts the claim of an `open` run: the host end must be in `openifs`, and its release
/// removes it from there. Catches an `open` run taken over into the other set's rules.
#[test]
fn adopting_an_open_run_needs_its_member_in_the_open_set() {
    let fake = Fake::new();
    leave(&fake, &["jsv0"], &[], &["jsv0"], &[]);
    job_end(&fake, 10, &[("inet", "198.18.0.2", 30)]);
    let egress = started(&fake, open_config());
    let adopted = egress
        .adopt("r", PID, JobNetwork::Open, &[])
        .expect("adopted");
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
/// is refused before anything is read: a `none` job gets an attachment with no interface and
/// no call is made, under every ceiling; an `open` job under `allow-list` is `AboveCeiling`;
/// mode none refuses an `allow-list` job the same way and settles to nothing. Catches an
/// adoption that wires or reads a run that has no wiring.
#[test]
fn a_none_job_has_nothing_to_take_over_and_a_level_above_the_ceiling_is_refused() {
    for config in [allow_list(), open_config(), config_with("none", None)] {
        let fake = Fake::new();
        let egress = started(&fake, config);
        let before = fake.calls();
        let attachment = egress
            .adopt("r", PID, JobNetwork::None, &[])
            .expect("nothing to take over");
        assert_eq!(attachment.interface(), None);
        drop(attachment);
        egress.settle().expect("nothing to settle");
        assert_eq!(fake.calls(), before);
    }
    let fake = Fake::new();
    let egress = started(&fake, allow_list());
    let before = fake.log().len();
    let above = egress.adopt("r", PID, JobNetwork::Open, &[]).map(drop);
    assert_eq!(
        above,
        Err(EgressError::AboveCeiling {
            asked: JobNetwork::Open,
            ceiling: JobNetwork::AllowList
        })
    );
    let fake_none = Fake::new();
    let none = started(&fake_none, config_with("none", None));
    let above = none.adopt("r", PID, JobNetwork::AllowList, &[]).map(drop);
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

/// Asserts each refusal of an adoption with its exact text, and that each leaves the kept
/// block for the settle: a host port outside the forward ports, a container that is gone, the
/// host's own namespace, `nsenter` failing, an `eth0` that is not JSON, one with no link
/// index, no address list, no address, two addresses, an address that is not /30 or not an
/// address, an address outside the pool or not a job end, a block no start kept, another
/// interface index, a member in another set or in none, and forwards that are not the run's
/// ports. Catches an adoption that takes a wiring that is not the run's.
#[test]
fn an_adoption_that_does_not_fit_is_refused_and_leaves_the_block_to_the_settle() {
    let same = (JobNetwork::AllowList, &[][..]);
    let nothing = |_: &Fake| {};
    let outside = refusal(
        forwarding("allow-list"),
        &[],
        &nothing,
        (JobNetwork::AllowList, &[(8080, 80), (9101, 80)]),
    );
    assert_eq!(
        outside,
        cannot("run r: host port 9101 is not among the ports this agent forwards")
    );
    let none_forwarded = refusal(
        allow_list(),
        &[],
        &nothing,
        (JobNetwork::AllowList, &[(80, 80)]),
    );
    assert_eq!(
        none_forwarded,
        cannot("run r: host port 80 is not among the ports this agent forwards")
    );

    let gone = refusal(
        allow_list(),
        &[],
        &|fake| {
            fake.state().identities.remove(NET);
        },
        same,
    );
    assert_eq!(
        gone,
        format!("egress: container {PID}: /proc/{PID}/ns/net: entity not found")
    );
    let own = refusal(
        allow_list(),
        &[],
        &|fake| fake.identity(NET, vec![(1, 1)]),
        same,
    );
    assert_eq!(
        own,
        format!("egress: container {PID}: it shares the host's network namespace")
    );

    let (ip, nsenter) = (program("ip"), program("nsenter"));
    let failed = refusal(
        allow_list(),
        &[],
        &|fake| {
            fake.answer(
                "addr show dev eth0",
                Err("Cannot open network namespace".to_owned()),
            )
        },
        same,
    );
    let args = format!(
        "[\"--net={NETFD}\", \"--\", \"{ip}\", \"-j\", \"addr\", \"show\", \"dev\", \"eth0\"]"
    );
    assert_eq!(
        failed,
        cannot(&format!(
            "egress: reading the job end: {nsenter} {args}: Cannot open network namespace"
        ))
    );
    let not_json = refusal(
        allow_list(),
        &[],
        &|fake| fake.answer("addr show dev eth0", Ok("nope".to_owned())),
        same,
    );
    assert!(
        not_json.starts_with(&cannot("reading the job end: ")),
        "{not_json}"
    );

    let not_a_pair = cannot("its eth0 is not the end of a pair with one /30 address");
    let ends: [Spoil; 8] = [
        Box::new(|fake| fake.answer("addr show dev eth0", Ok("[]".to_owned()))),
        Box::new(|fake| {
            let end = json!([{"ifname": "eth0", "addr_info": [
                {"family": "inet", "local": "198.18.0.2", "prefixlen": 30}]}]);
            fake.answer("addr show dev eth0", Ok(end.to_string()));
        }),
        Box::new(|fake| {
            let end = json!([{"ifname": "eth0", "link_index": 10}]);
            fake.answer("addr show dev eth0", Ok(end.to_string()));
        }),
        Box::new(|fake| job_end(fake, 10, &[])),
        Box::new(|fake| {
            job_end(
                fake,
                10,
                &[("inet", "198.18.0.2", 30), ("inet", "198.18.0.3", 30)],
            );
        }),
        Box::new(|fake| job_end(fake, 10, &[("inet", "198.18.0.2", 24)])),
        Box::new(|fake| job_end(fake, 10, &[("inet", "not an address", 30)])),
        Box::new(|fake| {
            let end = json!([{"link_index": 10, "addr_info": [
                {"family": "inet", "local": "198.18.0.2"}]}]);
            fake.answer("addr show dev eth0", Ok(end.to_string()));
        }),
    ];
    for (n, setup) in ends.iter().enumerate() {
        assert_eq!(
            refusal(allow_list(), &[], setup, same),
            not_a_pair,
            "end {n}"
        );
    }

    let outside_pool = refusal(
        allow_list(),
        &[],
        &|fake| job_end(fake, 10, &[("inet", "10.0.0.2", 30)]),
        same,
    );
    assert_eq!(
        outside_pool,
        cannot("run r: its address 10.0.0.2 is not the job end of a block of the pool")
    );
    let not_a_job_end = refusal(
        allow_list(),
        &[],
        &|fake| job_end(fake, 10, &[("inet", "198.18.0.1", 30)]),
        same,
    );
    assert_eq!(
        not_a_job_end,
        cannot("run r: its address 198.18.0.1 is not the job end of a block of the pool")
    );
    let unkept = refusal(
        allow_list(),
        &[],
        &|fake| job_end(fake, 10, &[("inet", "198.18.0.6", 30)]),
        same,
    );
    assert_eq!(unkept, cannot("run r: no jsv1 was left on the host"));
    let index = refusal(
        allow_list(),
        &[],
        &|fake| job_end(fake, 99, &[("inet", "198.18.0.2", 30)]),
        same,
    );
    assert_eq!(index, cannot("run r: its eth0 is not the job end of jsv0"));

    let set = cannot("run r: the set of its level does not hold jsv0");
    let wrong_set = refusal(open_config(), &[], &nothing, (JobNetwork::Open, &[]));
    assert_eq!(wrong_set, set);

    let kept = [(8080, "jsv0", "198.18.0.2", 80)];
    let forwards = cannot("run r: its declared ports are not the forwards of jsv0");
    for ports in [&[][..], &[(8080, 81)], &[(8080, 80), (9000, 90)]] {
        let wrong = refusal(
            forwarding("allow-list"),
            &kept,
            &nothing,
            (JobNetwork::AllowList, ports),
        );
        assert_eq!(wrong, forwards, "{ports:?}");
    }
    let unplanned = refusal(
        forwarding("allow-list"),
        &[],
        &nothing,
        (JobNetwork::AllowList, &[(8080, 80)]),
    );
    assert_eq!(unplanned, forwards);
}

/// Asserts that a member in no set is not a level's wiring: a host end the old tables held in
/// no set is refused for an `allow-list` run with the set's text. Catches an adoption that
/// reads "no set" as "any set".
#[test]
fn a_host_end_in_no_set_is_not_adopted() {
    let fake = Fake::new();
    leave(&fake, &["jsv0"], &[], &[], &[]);
    job_end(&fake, 10, &[("inet", "198.18.0.2", 30)]);
    let egress = started(&fake, allow_list());
    let error = egress.adopt("r", PID, JobNetwork::AllowList, &[]).map(drop);
    assert_eq!(
        error.expect_err("refused").to_string(),
        cannot("run r: the set of its level does not hold jsv0")
    );
}

/// Asserts that a host port another run of this agent forwards refuses the adoption with
/// `PortTaken` and its text, and that the block stays for the settle. Catches an adoption that
/// forwards one host port to two runs.
#[test]
fn a_host_port_another_run_forwards_refuses_the_adoption() {
    let (fake, egress) = left_one(
        forwarding("allow-list"),
        &[(8080, "jsv0", "198.18.0.2", 80)],
    );
    let (inet, nat) = tables(&[], &[], &[(8080, "jsv1", "198.18.0.6", 80)]);
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
    let other = egress
        .attach("other", PID, JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .expect("attach");
    assert_eq!(other.interface(), Some("jsv1"));
    let error = egress
        .adopt("r", PID, JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .map(drop);
    let error = error.expect_err("taken");
    assert_eq!(error, EgressError::PortTaken { port: 8080 });
    assert_eq!(
        error.to_string(),
        "egress: host port 8080 is forwarded to another run already"
    );
    let before = fake.log().len();
    drop(egress);
    assert_eq!(fake.log().len(), before);
    drop(other);
}

/// Asserts what a settle removes and in what order: for each block no run took over its
/// forwards' rules (listed, then deleted by handle), its member (only a block that has one) and
/// its host end, in block order, then each stray job end; the blocks are free afterwards, a
/// second settle makes no call, and the check, which refused the kept forwards, passes once
/// they are gone. Catches a settle that leaves wiring behind or removes more than was left.
#[test]
fn settle_removes_what_no_run_took_over_once() {
    let fake = Fake::new();
    leave(
        &fake,
        &["jsv0", "jsv1", "jsp4"],
        &["jsv0", "jsv1"],
        &[],
        &[(8080, "jsv0", "198.18.0.2", 80)],
    );
    let egress = started(&fake, forwarding("allow-list"));
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
            "ip link del jsp4",
        ]
    );
    let (inet, nat) = tables(&[], &[], &[]);
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
    egress.check().expect("no forward is left");
    let after = fake.log().len();
    egress.settle().expect("a second settle");
    assert_eq!(fake.log().len(), after, "nothing is left to do");
    let attached: Vec<Attachment> = (0..3)
        .map(|n| {
            egress
                .attach(&format!("r{n}"), PID, JobNetwork::AllowList, &[])
                .expect("attach")
        })
        .collect();
    let names: Vec<_> = attached.iter().map(Attachment::interface).collect();
    assert_eq!(names, [Some("jsv0"), Some("jsv1"), Some("jsv2")]);
}

/// Asserts that a removal that fails keeps its block out of use and does not stop the rest: with
/// the deletion of `jsv0` refused and that of the job end `jsp4` refused, `jsv1` and `jsp6` are
/// still removed, the first error is the one reported (the veth of `jsv0`), and blocks 0 and 4
/// are not given to a new run while 1, 2, 3, 5 and 6 are. Catches a settle that gives out a
/// name a firewall set may still hold, or stops at its first failure.
#[test]
fn a_settle_that_cannot_remove_a_block_keeps_it_out_of_use_and_reports_the_first_error() {
    let fake = Fake::new();
    leave(&fake, &["jsv0", "jsv1", "jsp4", "jsp6"], &[], &[], &[]);
    let egress = started(&fake, allow_list());
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
            "ip link del jsv0",
            "ip link del jsv1",
            "ip link del jsp4",
            "ip link del jsp6",
        ]
    );
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

/// Asserts that a stray job end alone is deleted with the text of what was being done:
/// `jsp4` failing is "deleting a leftover veth" and is reported when it is the only failure.
/// Catches a settle that hides the failure of a job end left alone.
#[test]
fn a_stray_job_end_that_cannot_be_deleted_is_reported() {
    let fake = Fake::new();
    leave(&fake, &["jsp4"], &[], &[], &[]);
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

/// Asserts that leaving an attachment removes nothing, now or when it is dropped, and that the
/// wiring is the next process's: the log does not grow, with the forwards of a run that has
/// ports, the host stays held by the egress
/// that is still alive. Catches a hand-over
/// that takes down the wiring of a run that goes on.
#[test]
fn a_left_attachment_removes_nothing_now_or_at_drop() {
    let fake = Fake::new();
    let egress = started(&fake, forwarding("allow-list"));
    let (inet, nat) = tables(&[], &[], &[(8080, "jsv0", "198.18.0.2", 80)]);
    fake.answer("list table inet", Ok(inet));
    fake.answer("list table ip", Ok(nat));
    let wired = egress
        .attach("r", PID, JobNetwork::AllowList, &maps(&[(8080, 80)]))
        .expect("attach");
    assert_eq!(wired.interface(), Some("jsv0"));
    let before = fake.log().len();
    wired.leave();
    assert_eq!(fake.log().len(), before, "left, not removed");
}
