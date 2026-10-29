//! The egress check: before a contained job is created on a named network the agent reads
//! back the network (`network inspect`), `br_netfilter` and the nftables table
//! `inet js_egress` (`nft -j list table`), and refuses the job unless they are what
//! `--container-egress-allow` requires. The runner is the scripted command line; the JSON
//! fixtures below are written to the shape of nft 1.0.9's `-j list table` for the ruleset
//! `deploy/js-agent/egress-network.sh` prints (from nft's JSON documentation: they have
//! not been compared with a real host's output).

use std::path::Path;

use js_agent::cli_runtime::{CliError, CliRuntime, EgressError, Settings, SettingsError};
use js_agent::lifecycle::{Config, Runtime, Lifecycle, NoChecks};
use js_agent_wire::Command;
use serde_json::{Value, json};

use super::scripted::Scripted;
use super::{
    BIN, HOST, in_another_pid_namespace, made_at, platform, profile_dir, sees_the_engines_pid, spec,
};
use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{config_in, job, names, reply};
use crate::support::Scratch;

const PODMAN: &str = "podman version 4.9.3\n";

/// The interface and subnet of the fixture network.
const IFACE: &str = "jsbr0";
const SUBNET: &str = "192.0.2.0";

fn allow() -> Vec<String> {
    ["198.51.100.10:443", "198.51.100.20:8443"]
        .map(str::to_owned)
        .to_vec()
}

/// `br_netfilter` active, and no other file of `/proc`.
fn bridge_on(path: &str) -> Option<String> {
    (path == "/proc/sys/net/bridge/bridge-nf-call-iptables").then(|| "1\n".to_owned())
}

/// `br_netfilter` not loaded: the file is not there.
fn bridge_absent(_path: &str) -> Option<String> {
    None
}

/// `br_netfilter` loaded and off.
fn bridge_off(path: &str) -> Option<String> {
    (path == "/proc/sys/net/bridge/bridge-nf-call-iptables").then(|| "0\n".to_owned())
}

fn settings(allow: &[String]) -> Settings {
    Settings::new(Path::new(BIN), Some("jobs"), None)
        .unwrap()
        .with_egress(allow, Some(BIN))
        .unwrap()
}

fn runtime_with(
    world: &Scripted,
    set: Settings,
    proc: fn(&str) -> Option<String>,
) -> Result<CliRuntime, CliError> {
    CliRuntime::open(
        set,
        HOST,
        &platform("linux"),
        made_at,
        Box::new(world.clone()),
    )
    .map(|rt| {
        rt.with_profile_dir(profile_dir())
            .with_pid_seen(sees_the_engines_pid)
            .with_proc_files(proc, in_another_pid_namespace)
    })
}

/// A Podman world whose network and table are the good fixtures.
fn world() -> Scripted {
    let world = Scripted::new(PODMAN);
    world.say("network", &network().to_string());
    world.say("-j", &listing(&forward(), &input(), -10));
    world
}

fn runtime(world: &Scripted) -> CliRuntime {
    runtime_with(world, settings(&allow()), bridge_on).unwrap()
}

/// `podman network inspect` of a good egress network.
fn network() -> Value {
    json!([{
        "name": "jobs", "id": "0123", "driver": "bridge", "network_interface": IFACE,
        "created": "2026-10-29T00:00:00Z",
        "subnets": [{"subnet": format!("{SUBNET}/24"), "gateway": "192.0.2.1"}],
        "ipv6_enabled": false, "internal": false, "dns_enabled": false,
        "ipam_options": {"driver": "host-local"}
    }])
}

fn iif() -> Value {
    json!({"match": {"op": "==", "left": {"meta": {"key": "iifname"}}, "right": IFACE}})
}

fn payload(proto: &str, field: &str, right: Value) -> Value {
    json!({"match": {"op": "==",
        "left": {"payload": {"protocol": proto, "field": field}}, "right": right}})
}

fn prefix(addr: &str, len: u8) -> Value {
    json!({"prefix": {"addr": addr, "len": len}})
}

fn counter() -> Value {
    json!({"counter": {"packets": 0, "bytes": 0}})
}

fn rule(chain: &str, expr: Vec<Value>) -> Value {
    json!({"rule": {"family": "inet", "table": "js_egress", "chain": chain, "handle": 1, "expr": expr}})
}

fn drop_v6(chain: &str) -> Value {
    let v6 = json!({"match": {"op": "==", "left": {"meta": {"key": "nfproto"}}, "right": "ipv6"}});
    rule(chain, vec![iif(), v6, counter(), json!({"drop": null})])
}

fn drop_metadata() -> Value {
    let to = payload("ip", "daddr", prefix("169.254.0.0", 16));
    rule("forward", vec![iif(), to, counter(), json!({"drop": null})])
}

fn established(chain: &str) -> Value {
    let state = json!({"match": {"op": "in", "left": {"ct": {"key": "state"}},
        "right": ["established", "related"]}});
    rule(chain, vec![iif(), state, json!({"accept": null})])
}

fn accept(chain: &str, ip: &str, port: u16) -> Value {
    let new = json!({"match": {"op": "in", "left": {"ct": {"key": "state"}}, "right": "new"}});
    rule(
        chain,
        vec![
            iif(),
            payload("ip", "saddr", prefix(SUBNET, 24)),
            payload("ip", "daddr", json!(ip)),
            payload("tcp", "dport", json!(port)),
            new,
            json!({"accept": null}),
        ],
    )
}

fn drop_rest(chain: &str) -> Value {
    rule(chain, vec![iif(), counter(), json!({"drop": null})])
}

/// The rules the script writes into `forward` for the two endpoints of [`allow`].
fn forward() -> Vec<Value> {
    vec![
        drop_v6("forward"),
        drop_metadata(),
        established("forward"),
        accept("forward", "198.51.100.10", 443),
        accept("forward", "198.51.100.20", 8443),
        drop_rest("forward"),
    ]
}

/// The rules the script writes into `input`.
fn input() -> Vec<Value> {
    vec![
        drop_v6("input"),
        established("input"),
        accept("input", "198.51.100.10", 443),
        accept("input", "198.51.100.20", 8443),
        drop_rest("input"),
    ]
}

/// `nft -j list table inet js_egress` for these rules.
fn listing(forward: &[Value], input: &[Value], prio: i64) -> String {
    let chain = |name: &str, hook: &str| {
        json!({"chain": {"family": "inet", "table": "js_egress", "name": name, "handle": 1,
            "type": "filter", "hook": hook, "prio": prio, "policy": "accept"}})
    };
    let mut items = vec![
        json!({"metainfo": {"version": "1.0.9", "release_name": "Old Doc Yak #3",
            "json_schema_version": 1}}),
        json!({"table": {"family": "inet", "name": "js_egress", "handle": 1}}),
        chain("forward", "forward"),
    ];
    items.extend(forward.iter().cloned());
    items.push(chain("input", "input"));
    items.extend(input.iter().cloned());
    json!({"nftables": items}).to_string()
}

/// Asserts that the egress check passes when the network is a bridge with DNS off and no
/// IPv6, `br_netfilter` is on, and the table holds exactly the rules for the endpoints
/// (counters ignored; the endpoints in any order). It catches a check that refuses what
/// the script installs, which would stop every job.
#[test]
fn a_matching_table_passes() {
    let world = world();
    let rt = runtime(&world);
    rt.egress_enforced().unwrap();
    // The endpoints in the other order are the same set.
    let mut swapped = forward();
    swapped.swap(3, 4);
    world.say("-j", &listing(&swapped, &input(), -10));
    rt.egress_enforced().unwrap();
    assert_eq!(world.calls_of("network").len(), 2);
    assert_eq!(world.calls_of("-j").len(), 2);
    assert_eq!(
        world.calls_of("-j")[0],
        ["-j", "list", "table", "inet", "js_egress"]
    );
}

/// Asserts that each way the table can be wrong refuses with the typed error naming the
/// chain: an extra endpoint, a missing endpoint, an endpoint with another port, a missing
/// final drop (forward and input), a missing IPv6 drop, a missing metadata drop, a missing
/// established accept, a wrong subnet, a wrong interface, a wrong priority, a chain that
/// is not on its hook, and an expression the check does not know. It catches a rule that
/// opens more than the operator named, and a table that does not drop what is left.
#[test]
fn a_table_that_differs_refuses() {
    let wrong_subnet = {
        let mut rules = forward();
        rules[3] = rule(
            "forward",
            vec![
                iif(),
                payload("ip", "saddr", prefix("203.0.113.0", 24)),
                payload("ip", "daddr", json!("198.51.100.10")),
                payload("tcp", "dport", json!(443)),
                json!({"match": {"op": "in", "left": {"ct": {"key": "state"}}, "right": "new"}}),
                json!({"accept": null}),
            ],
        );
        rules
    };
    let udp = {
        let mut rules = input();
        rules[2] = rule(
            "input",
            vec![
                iif(),
                payload("udp", "dport", json!(53)),
                json!({"accept": null}),
            ],
        );
        rules
    };
    let unknown = {
        let mut rules = forward();
        rules.insert(3, rule("forward", vec![iif(), json!({"masquerade": null})]));
        rules
    };
    let other_iface = {
        let mut rules = forward();
        rules[5] = rule(
            "forward",
            vec![
                json!({"match": {"op": "==", "left": {"meta": {"key": "iifname"}}, "right": "other0"}}),
                json!({"drop": null}),
            ],
        );
        rules
    };
    let cut = |mut rules: Vec<Value>, at: usize| {
        rules.remove(at);
        rules
    };
    let extra = {
        let mut rules = forward();
        rules.insert(5, accept("forward", "198.51.100.99", 443));
        rules
    };
    let other_port = {
        let mut rules = forward();
        rules[3] = accept("forward", "198.51.100.10", 444);
        rules
    };
    let accept_last = {
        let mut rules = input();
        rules.push(accept("input", "198.51.100.30", 22));
        rules
    };
    // Rules in the place of the established accept that nft would never print for the
    // script's ruleset: each must read as "not the expected rule", not as a pass.
    let odd = |expr: Vec<Value>| {
        let mut rules = forward();
        rules[2] = rule("forward", expr);
        rules
    };
    let number_state = odd(vec![
        iif(),
        json!({"match": {"op": "in", "left": {"ct": {"key": "state"}}, "right": 5}}),
        json!({"accept": null}),
    ]);
    let other_meta = odd(vec![
        iif(),
        json!({"match": {"op": "==", "left": {"meta": {"key": "l4proto"}}, "right": "tcp"}}),
        json!({"accept": null}),
    ]);
    let set_daddr = odd(vec![
        iif(),
        payload("ip", "daddr", json!(["198.51.100.10"])),
        json!({"accept": null}),
    ]);
    let ct_mark = odd(vec![
        iif(),
        json!({"match": {"op": "in", "left": {"ct": {"key": "mark"}}, "right": "new"}}),
        json!({"accept": null}),
    ]);
    let reordered = {
        let mut rules = forward();
        rules.swap(1, 2);
        rules
    };
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "order swapped",
            listing(&reordered, &input(), -10),
            "forward",
        ),
        (
            "ct state as a number",
            listing(&number_state, &input(), -10),
            "forward",
        ),
        (
            "another meta key",
            listing(&other_meta, &input(), -10),
            "forward",
        ),
        (
            "daddr as a list",
            listing(&set_daddr, &input(), -10),
            "forward",
        ),
        (
            "another ct key",
            listing(&ct_mark, &input(), -10),
            "forward",
        ),
        ("extra endpoint", listing(&extra, &input(), -10), "forward"),
        (
            "another port",
            listing(&other_port, &input(), -10),
            "forward",
        ),
        (
            "endpoint missing",
            listing(&cut(forward(), 3), &input(), -10),
            "forward",
        ),
        (
            "input endpoint missing",
            listing(&forward(), &cut(input(), 2), -10),
            "input",
        ),
        (
            "no final drop forward",
            listing(&cut(forward(), 5), &input(), -10),
            "forward",
        ),
        (
            "no final drop input",
            listing(&forward(), &cut(input(), 4), -10),
            "input",
        ),
        (
            "accept after the drop",
            listing(&forward(), &accept_last, -10),
            "input",
        ),
        (
            "no ipv6 drop forward",
            listing(&cut(forward(), 0), &input(), -10),
            "forward",
        ),
        (
            "no ipv6 drop input",
            listing(&forward(), &cut(input(), 0), -10),
            "input",
        ),
        (
            "no metadata drop",
            listing(&cut(forward(), 1), &input(), -10),
            "forward",
        ),
        (
            "no established accept",
            listing(&cut(forward(), 2), &input(), -10),
            "forward",
        ),
        (
            "wrong subnet",
            listing(&wrong_subnet, &input(), -10),
            "forward",
        ),
        (
            "wrong interface",
            listing(&other_iface, &input(), -10),
            "forward",
        ),
        ("udp accept", listing(&forward(), &udp, -10), "input"),
        (
            "unknown expression",
            listing(&unknown, &input(), -10),
            "forward",
        ),
        (
            "wrong priority",
            listing(&forward(), &input(), 0),
            "forward",
        ),
        ("no rules", listing(&[], &[], -10), "forward"),
    ];
    let world = world();
    let rt = runtime(&world);
    for (what, listed, chain) in cases {
        world.say("-j", &listed);
        match rt.egress_enforced() {
            Err(CliError::Egress(EgressError::Chain { chain: got })) => {
                assert_eq!(got, chain, "{what}")
            }
            other => panic!("{what}: {other:?}"),
        }
    }
}

/// Asserts that the input chain must accept every allowed endpoint, the same set as the
/// forward chain: a job reaches the store front on its OWN node (one of the allowed
/// addresses is the host's) through the input hook, not forward, so a table whose input
/// chain lacks that endpoint is refused even though forward accepts it, and the table that
/// accepts every endpoint in both chains passes. The endpoint at 198.51.100.10:443 stands
/// for the node's own front. It catches an input chain that only opens the host to the
/// established half, which would leave every job unable to reach its own node's store.
#[test]
fn the_input_chain_accepts_every_endpoint_so_a_job_reaches_its_own_nodes_front() {
    let world = world();
    let rt = runtime(&world);
    let own_front = 2;
    assert_eq!(input()[own_front], accept("input", "198.51.100.10", 443));
    let mut without_own_front = input();
    without_own_front.remove(own_front);
    world.say("-j", &listing(&forward(), &without_own_front, -10));
    match rt.egress_enforced() {
        Err(CliError::Egress(EgressError::Chain { chain })) => assert_eq!(chain, "input"),
        other => panic!("an input chain without the node's own front passed: {other:?}"),
    }
    world.say("-j", &listing(&forward(), &input(), -10));
    rt.egress_enforced().unwrap();
}

/// Asserts the table-level failures: a chain that is not on its hook (a regular chain
/// is never evaluated), a table that is not in the listing, a rule in a chain the agent
/// does not know, output that is not nft's JSON, and nft failing (which is what a missing
/// table does). None of the errors carries what nft printed. It catches a hook-less
/// table that looks right and does nothing, and a failure of nft read as a pass.
#[test]
fn a_missing_table_or_a_failing_nft_refuses() {
    let world = world();
    let rt = runtime(&world);

    let mut value: Value = serde_json::from_str(&listing(&forward(), &input(), -10)).unwrap();
    value["nftables"][2]["chain"]["hook"] = json!("prerouting");
    world.say("-j", &value.to_string());
    assert_eq!(
        rt.egress_enforced().unwrap_err(),
        CliError::Egress(EgressError::Chain { chain: "forward" })
    );
    let mut value: Value = serde_json::from_str(&listing(&forward(), &input(), -10)).unwrap();
    value["nftables"][2]["chain"]["type"] = json!("nat");
    world.say("-j", &value.to_string());
    assert!(rt.egress_enforced().is_err());

    world.say(
        "-j",
        &json!({"nftables": [{"metainfo": {"version": "1.0.9"}}]}).to_string(),
    );
    assert_eq!(
        rt.egress_enforced().unwrap_err(),
        CliError::Egress(EgressError::NoTable)
    );
    // A table of the same name in another family, and another name in the right one, are
    // not ours: the rules of another table prove nothing.
    for (family, name) in [("ip", "js_egress"), ("inet", "other")] {
        let table = json!({"table": {"family": family, "name": name, "handle": 1}});
        world.say("-j", &json!({"nftables": [table]}).to_string());
        assert_eq!(
            rt.egress_enforced().unwrap_err(),
            CliError::Egress(EgressError::NoTable),
            "{family} {name}"
        );
    }

    let mut stray = forward();
    stray.push(rule("elsewhere", vec![json!({"accept": null})]));
    world.say("-j", &listing(&stray, &input(), -10));
    assert_eq!(
        rt.egress_enforced().unwrap_err(),
        CliError::Egress(EgressError::Extra)
    );

    world.say("-j", "SECRET-NFT-OUTPUT not json");
    let said = rt.egress_enforced().unwrap_err();
    assert_eq!(said, CliError::Egress(EgressError::Unreadable));
    assert!(!said.to_string().contains("SECRET"), "{said}");
    world.say("-j", "{\"nftables\": 3}");
    assert!(rt.egress_enforced().is_err());

    // nft exits with status 1 (no such table, or no permission): never a pass.
    let world = self::world();
    let rt = runtime(&world);
    world.fail("-j", 1);
    let said = rt.egress_enforced().unwrap_err();
    assert!(
        matches!(said, CliError::Egress(EgressError::Nft(_))),
        "{said:?}"
    );
    assert!(said.to_string().contains("exit status: 1"), "{said}");
    rt.egress_enforced().unwrap();
}

/// Asserts the network rules: it must be a bridge, with DNS off, IPv6 off, no IPv6 subnet,
/// exactly one IPv4 subnet, and a usable interface name, each field present (a missing
/// field is a refusal, not a default); a network the runtime cannot inspect refuses with
/// the network's name; and an endpoint inside the network's own subnet is refused. It
/// catches a macvlan or DNS-enabled network passing under rules that fit only a bridge.
#[test]
fn a_network_that_is_not_an_isolated_bridge_refuses() {
    let world = world();
    let rt = runtime(&world);
    let change = |edit: &dyn Fn(&mut Value)| {
        let mut net = network();
        edit(&mut net[0]);
        world.say("network", &net.to_string());
    };
    type Edit = Box<dyn Fn(&mut Value)>;
    let edits: Vec<(&str, Edit)> = vec![
        ("macvlan", Box::new(|n| n["driver"] = json!("macvlan"))),
        ("dns", Box::new(|n| n["dns_enabled"] = json!(true))),
        ("ipv6", Box::new(|n| n["ipv6_enabled"] = json!(true))),
        (
            "no dns field",
            Box::new(|n| {
                n.as_object_mut().unwrap().remove("dns_enabled");
            }),
        ),
        (
            "no ipv6 field",
            Box::new(|n| {
                n.as_object_mut().unwrap().remove("ipv6_enabled");
            }),
        ),
        (
            "no driver",
            Box::new(|n| {
                n.as_object_mut().unwrap().remove("driver");
            }),
        ),
        (
            "no interface",
            Box::new(|n| {
                n.as_object_mut().unwrap().remove("network_interface");
            }),
        ),
        (
            "bad interface",
            Box::new(|n| n["network_interface"] = json!("a b")),
        ),
        (
            "long interface",
            Box::new(|n| n["network_interface"] = json!("0123456789abcdef")),
        ),
        (
            "v6 subnet",
            Box::new(|n| n["subnets"] = json!([{"subnet": "2001:db8::/64"}])),
        ),
        (
            "two subnets",
            Box::new(|n| {
                n["subnets"] = json!([{"subnet": "192.0.2.0/24"}, {"subnet": "2001:db8::/64"}]);
            }),
        ),
        ("no subnet", Box::new(|n| n["subnets"] = json!([]))),
    ];
    for (what, edit) in &edits {
        change(edit.as_ref());
        match rt.egress_enforced() {
            Err(CliError::Egress(EgressError::NetworkShape { name })) => {
                assert_eq!(name, "jobs", "{what}")
            }
            other => panic!("{what}: {other:?}"),
        }
    }
    world.say("network", "not json");
    assert!(matches!(
        rt.egress_enforced(),
        Err(CliError::Egress(EgressError::NetworkShape { .. }))
    ));
    world.say("network", &json!([network()[0], network()[0]]).to_string());
    assert!(rt.egress_enforced().is_err(), "two networks");

    let world = self::world();
    let rt = runtime(&world);
    world.fail("network", 1);
    assert!(matches!(
        rt.egress_enforced(),
        Err(CliError::Egress(EgressError::Network { name, .. })) if name == "jobs"
    ));
    assert!(
        world.calls_of("-j").is_empty(),
        "nft is not asked for a network that fails"
    );

    // An endpoint in the subnet (or the gateway): the job could reach a neighbour.
    for inside in ["192.0.2.50:80", "192.0.2.1:80", "192.0.2.0/25:80"] {
        let rt = runtime_with(&self::world(), settings(&[inside.to_owned()]), bridge_on).unwrap();
        assert_eq!(
            rt.egress_enforced().unwrap_err(),
            CliError::Egress(EgressError::AllowInSubnet),
            "{inside}"
        );
    }
}

/// Asserts that `br_netfilter` must be active: bridge-nf-call-iptables absent or 0 refuses,
/// because without it traffic between two jobs on the bridge never reaches the hooks.
/// It catches a firewall that looks right while the bridge bypasses it.
#[test]
fn br_netfilter_must_be_active() {
    for proc in [bridge_off, bridge_absent] {
        let world = world();
        let rt = runtime_with(&world, settings(&allow()), proc).unwrap();
        assert_eq!(
            rt.egress_enforced().unwrap_err(),
            CliError::Egress(EgressError::BrNetfilter)
        );
        assert!(world.calls_of("-j").is_empty());
    }
}

/// Asserts that Docker with egress flags is refused when the runtime opens (the check
/// reads netavark's network and an nftables table, which Docker does not use), and that
/// the default network is never checked: no call is made. It catches a Docker host that
/// believes a check protects it.
#[test]
fn docker_is_refused_and_the_default_network_is_not_checked() {
    let docker = Scripted::new(super::scripted::DOCKER);
    let opened = runtime_with(&docker, settings(&allow()), bridge_on);
    assert_eq!(opened.err(), Some(CliError::EgressDocker));

    let world = Scripted::new(PODMAN);
    let plain = Settings::new(Path::new(BIN), None, None).unwrap();
    let rt = runtime_with(&world, plain, bridge_off).unwrap();
    rt.egress_enforced().unwrap();
    assert!(world.calls_of("network").is_empty());
    assert!(world.calls_of("-j").is_empty());
    assert_eq!(
        Settings::new(Path::new(BIN), Some("jobs"), None)
            .unwrap()
            .with_egress(&[], None),
        Err(SettingsError::EgressMissing)
    );
}

/// A lifecycle over a Podman runtime with the egress check.
fn lifecycle(world: &Scripted, dir: &Scratch) -> Lifecycle {
    let config: Config = config_in(dir, false);
    let (life, _) = Lifecycle::adopt(
        config,
        Box::new(runtime(world)),
        Box::new(NoChecks),
        Box::new(ManualClock::default()),
    )
    .unwrap();
    life
}

fn start(life: &mut Lifecycle, run_id: &str) -> Vec<String> {
    names(&life.apply(&reply(vec![Command::Start(Box::new(job(run_id)))])))
}

/// The subcommands of the calls that matter, in order: the two checks and the create.
fn sequence(world: &Scripted) -> Vec<String> {
    world
        .calls()
        .into_iter()
        .map(|args| args[0].clone())
        .filter(|sub| matches!(sub.as_str(), "network" | "-j" | "create"))
        .collect()
}

/// Asserts the check runs before EVERY create and not only the first (the network and the
/// table, then the create, for each of two starts); that a start whose table has been
/// flushed since the first is refused as `egress-unchecked` with nothing created; and that
/// the next start passes again once the table is back. It catches a check made once at
/// start, which a flushed ruleset would then outlive.
#[test]
fn the_check_runs_before_every_create_and_a_failure_refuses_the_start() {
    let dir = Scratch::new("cli-egress-life");
    let world = world();
    let mut life = lifecycle(&world, &dir);
    assert_eq!(start(&mut life, "r1"), ["started r1"]);
    assert_eq!(start(&mut life, "r2"), ["started r2"]);
    assert_eq!(
        sequence(&world),
        ["network", "-j", "create", "network", "-j", "create"]
    );

    world.fail("-j", 1);
    assert_eq!(start(&mut life, "r3"), ["refused r3 egress-unchecked"]);
    assert_eq!(world.calls_of("create").len(), 2, "nothing was created");
    assert_eq!(world.ids().len(), 2);
    let last = life.results().last().unwrap().clone();
    assert_eq!(last.run_id, "r3");
    assert_eq!(last.detail.as_deref(), Some("egress-unchecked"));

    assert_eq!(start(&mut life, "r4"), ["started r4"]);
    assert_eq!(world.calls_of("create").len(), 3);
}

/// Asserts that the created container of a named network is given the sysctl that removes
/// IPv6 (`--sysctl net.ipv6.conf.all.disable_ipv6=1`) and that the default network is not
/// (there is no interface to configure). It catches a job that keeps the link-local IPv6
/// address of its veth.
#[test]
fn a_named_network_disables_ipv6_in_the_container() {
    let world = world();
    let mut rt = runtime(&world);
    let id = rt.create_and_start(&spec("r1")).unwrap();
    let args = world.create_of(&id.0);
    let at = args.iter().position(|a| a == "--sysctl").expect("--sysctl");
    assert_eq!(args[at + 1], "net.ipv6.conf.all.disable_ipv6=1");

    let plain = Scripted::new(PODMAN);
    let set = Settings::new(Path::new(BIN), None, None).unwrap();
    let mut rt = runtime_with(&plain, set, bridge_on).unwrap();
    let id = rt.create_and_start(&spec("r1")).unwrap();
    assert!(!plain.create_of(&id.0).iter().any(|a| a == "--sysctl"));
}

/// Asserts that a host-isolation start is not subject to the egress check: the call log
/// holds no `network inspect` and no `nft` for it (it has no container network; the
/// runtime refuses it on its own). It catches the check moved above the isolation test,
/// where a host job would depend on a firewall it does not use.
#[test]
fn a_host_job_skips_the_egress_check() {
    let dir = Scratch::new("cli-egress-host");
    let world = world();
    let (mut life, _) = Lifecycle::adopt(
        config_in(&dir, true),
        Box::new(runtime(&world)),
        Box::new(NoChecks),
        Box::new(ManualClock::default()),
    )
    .unwrap();
    let mut host = job("h1");
    host.isolation = js_agent_wire::Isolation::Host;
    life.apply(&reply(vec![Command::Start(Box::new(host))]));
    assert!(
        sequence(&world).iter().all(|sub| sub == "create"),
        "{:?}",
        sequence(&world)
    );
    assert!(world.calls_of("network").is_empty());
    assert!(world.calls_of("-j").is_empty());
}
