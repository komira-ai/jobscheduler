//! The network of a container job on the wire: the `network` level and the `ports` of a
//! start, which are additive (a start from before them still parses, and a start that asks
//! for nothing is the text it was), and the ceiling an agent reports in its hello.

mod common;

use std::collections::BTreeMap;

use common::{broken, expect, hello, job};
use js_agent_wire::limits::{
    MAX_EGRESS_PORTS_CAPABILITY_VALUE_LEN, MAX_PORTS, capability_value_max,
};
use js_agent_wire::refusal;
use js_agent_wire::{
    Command, EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY, HeartbeatReply, Hello, Isolation, JobSpec,
    Limits, Mount, NativeSpec, NetworkCeiling, PortRange, Rule,
};
use js_types::{JobNetwork, PortMap};
use serde::Deserialize;
use serde_json::json;

/// A start as the release before the fields wrote it, saved literally: no `network`, no
/// `ports`.
const PREVIOUS_START: &str = concat!(
    r#"{"type":"start","run_id":"run-1","lease_id":"1-1","#,
    r#""image":"ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
    r#""args":["make","all"],"#,
    r#""limits":{"cpu_millis":2000,"memory_bytes":1073741824,"pids":512,"wall_ms":60000},"#,
    r#""gpu_indices":[0],"mounts":[{"root_name":"cache","rel":"inputs/a","ro":true}],"#,
    r#""isolation":"host","labels":{"team":"infra"},"heartbeat_seq":7,"valid_for_ms":10000}"#
);

/// The start body of the release before the fields, copied: what an agent of that release
/// decodes a start into. Like every body of the wire it skips fields it does not know.
#[derive(Debug, Deserialize)]
#[allow(
    dead_code,
    reason = "decoded to prove the previous shape reads the text"
)]
struct PreviousJobSpec {
    run_id: String,
    lease_id: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    native: Option<NativeSpec>,
    #[serde(default)]
    args: Vec<String>,
    limits: Limits,
    #[serde(default)]
    gpu_indices: Vec<u32>,
    #[serde(default)]
    mounts: Vec<Mount>,
    #[serde(default)]
    files: Vec<js_agent_wire::RunFile>,
    #[serde(default)]
    isolation: Isolation,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    heartbeat_seq: u64,
    #[serde(default)]
    valid_for_ms: u64,
    #[serde(default)]
    bundle: Option<String>,
}

/// The command of the release before the fields, copied, with its `start` tag.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousCommand {
    Start(Box<PreviousJobSpec>),
}

fn port(host: u16, container: u16) -> PortMap {
    PortMap { host, container }
}

/// Proves the fields are additive both ways (rule B8). A start saved from the previous
/// release (no `network`, no `ports`) decodes as `none` with no port and encodes back to the
/// same bytes. A new start with `none` and no port carries neither key, so the previous
/// release decodes it; a new start with `open` and a port decodes in the previous release's
/// shape too, the new fields skipped. Catches a field that is always written (an older
/// agent's text would change) or required (every start of an older server would be
/// unreadable).
#[test]
fn previous_wire_start_without_network_still_parses() {
    let decoded: Command = serde_json::from_str(PREVIOUS_START).unwrap();
    let Command::Start(spec) = &decoded else {
        panic!("not a start: {decoded:?}");
    };
    assert_eq!(spec.network, JobNetwork::None);
    assert!(spec.ports.is_empty());
    assert_eq!(**spec, job());
    assert_eq!(serde_json::to_string(&decoded).unwrap(), PREVIOUS_START);

    let none = serde_json::to_string(&Command::Start(Box::new(job()))).unwrap();
    assert!(!none.contains("\"network\""), "{none}");
    assert!(!none.contains("\"ports\""), "{none}");
    let PreviousCommand::Start(old) = serde_json::from_str(&none).unwrap();
    assert_eq!(old.run_id, "run-1");

    let mut open = job();
    open.network = JobNetwork::Open;
    open.ports = vec![port(8080, 80)];
    let text = serde_json::to_string(&Command::Start(Box::new(open.clone()))).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["network"], json!("open"));
    assert_eq!(value["ports"], json!([{"host": 8080, "container": 80}]));
    let PreviousCommand::Start(old) = serde_json::from_str(&text).unwrap();
    assert_eq!((old.run_id.as_str(), old.limits.wall_ms), ("run-1", 60_000));
    let back: Command = serde_json::from_str(&text).unwrap();
    assert_eq!(back, Command::Start(Box::new(open)));
    let allow = serde_json::to_value(JobNetwork::AllowList).unwrap();
    assert_eq!(allow, json!("allow-list"));
}

/// Proves a level this version does not know is a decode error and not a level: the start
/// alone does not decode, and inside a reply it reads as an unknown command while the stop
/// beside it arrives. Catches a level that fails open (read as `none` or `open`), which would
/// run a job at a level it did not name.
#[test]
fn an_unknown_network_level_fails_to_decode() {
    let mut body = serde_json::to_value(job()).unwrap();
    body["network"] = json!("wide");
    let err = serde_json::from_value::<JobSpec>(body.clone()).unwrap_err();
    assert!(err.is_data(), "{err}");
    let mut start = json!({"type": "start"});
    start
        .as_object_mut()
        .unwrap()
        .extend(body.as_object().unwrap().clone());
    let stop = json!({"type": "stop", "run_id": "r"});
    let reply: HeartbeatReply = serde_json::from_value(json!({"commands": [start, stop]})).unwrap();
    assert_eq!(
        reply.commands,
        vec![Command::Unknown, Command::Stop { run_id: "r".into() }]
    );
    for known in ["none", "allow-list", "open"] {
        body["network"] = json!(known);
        let spec: JobSpec = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(spec.network.name(), known);
    }
}

fn check_ports(ports: Vec<PortMap>) -> js_agent_wire::ValidationError {
    broken(&job(), |j| j.ports = ports, JobSpec::validate)
}

/// A job with no image and a native body, valid as it is.
fn native_job() -> JobSpec {
    let mut spec = job();
    spec.image = String::new();
    spec.args = Vec::new();
    spec.gpu_indices = Vec::new();
    spec.mounts = Vec::new();
    spec.isolation = Isolation::Contained;
    spec.native = Some(NativeSpec {
        program: "/usr/bin/clang".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: "src".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: js_agent_wire::NetworkLevel::Off,
        ports: Vec::new(),
    });
    spec
}

/// Proves every bound of the declared ports, each refused `invalid-job` naming the field: a
/// zero host or container port names its place (`ports[0].host`, `ports[1].container`), a host
/// port given twice names the later one (`ports[2].host`), and more than 16 is `ports`; 16, a
/// port below 1024 and one container port behind two host ports are valid. A native job that
/// sets `network` or declares a port is refused naming that field. Catches a port of zero (no
/// forward can be made for it), two jobs' worth of one host port in one start, an unbounded
/// list, and a native job whose container fields would be silently ignored.
#[test]
fn ports_are_validated_naming_the_field() {
    let err = check_ports(vec![port(0, 80)]);
    expect(err, "ports[0].host", Rule::OutOfRange, refusal::INVALID_JOB);
    let err = check_ports(vec![port(8080, 80), port(8081, 0)]);
    expect(
        err,
        "ports[1].container",
        Rule::OutOfRange,
        refusal::INVALID_JOB,
    );
    let err = check_ports(vec![port(8080, 80), port(8081, 81), port(8080, 82)]);
    expect(err, "ports[2].host", Rule::Duplicate, refusal::INVALID_JOB);
    let many: Vec<PortMap> = (1..=17).map(|n| port(8000 + n, 80)).collect();
    let err = check_ports(many);
    expect(err, "ports", Rule::TooMany, refusal::INVALID_JOB);
    let err = check_ports(vec![port(65535, 65535), port(15, 0)]);
    expect(
        err,
        "ports[1].container",
        Rule::OutOfRange,
        refusal::INVALID_JOB,
    );

    let mut spec = job();
    spec.ports = (1..=16).map(|n| port(8000 + n, 80)).collect();
    assert_eq!(spec.ports.len(), MAX_PORTS);
    assert_eq!(spec.validate(), Ok(()));
    spec.ports = vec![port(80, 8080), port(443, 8080), port(1, 65535)];
    assert_eq!(spec.validate(), Ok(()));
    spec.network = JobNetwork::Open;
    assert_eq!(spec.validate(), Ok(()));

    let native = native_job();
    assert_eq!(native.validate(), Ok(()));
    let err = broken(
        &native,
        |j| j.network = JobNetwork::AllowList,
        JobSpec::validate,
    );
    expect(err, "network", Rule::Inconsistent, refusal::INVALID_JOB);
    let err = broken(
        &native,
        |j| j.ports = vec![port(8080, 80)],
        JobSpec::validate,
    );
    expect(err, "ports", Rule::Inconsistent, refusal::INVALID_JOB);
}

fn caps(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn ceiling(level: JobNetwork, ports: &[&str]) -> NetworkCeiling {
    NetworkCeiling {
        level,
        ports: ports
            .iter()
            .map(|text| PortRange::parse(text).unwrap())
            .collect(),
    }
}

/// Proves the ceiling an agent writes into its hello is the one a server reads back: `open`
/// and `none` are written under `egress`, a list of ports under `egress-ports` as
/// `80,8000-8010` (and no key without a port), an absent key reads as `none` with no port,
/// and a hello with the keys and one without them (an older agent's) both validate. Catches
/// a key that is missing or spelled otherwise, a list that does not round-trip, and a
/// server that refuses the hello of an agent from before the keys.
#[test]
fn the_ceiling_is_advertised_in_hello() {
    let mut written = BTreeMap::new();
    ceiling(JobNetwork::Open, &[]).insert_into(&mut written);
    assert_eq!(written, caps(&[("egress", "open")]));
    let mut written = BTreeMap::new();
    NetworkCeiling::default().insert_into(&mut written);
    assert_eq!(written, caps(&[("egress", "none")]));
    assert_eq!(
        NetworkCeiling::from_capabilities(&written),
        Ok(NetworkCeiling::default())
    );

    let ported = ceiling(JobNetwork::AllowList, &["80", "8000-8010"]);
    let mut written = BTreeMap::new();
    ported.insert_into(&mut written);
    assert_eq!(
        written,
        caps(&[("egress", "allow-list"), ("egress-ports", "80,8000-8010")])
    );
    assert_eq!(
        NetworkCeiling::from_capabilities(&written),
        Ok(ported.clone())
    );
    assert_eq!(ported.ports_text(), "80,8000-8010");
    assert!(ported.forwards(80) && ported.forwards(8000) && ported.forwards(8010));
    assert!(!ported.forwards(81) && !ported.forwards(8011) && !ported.forwards(7999));
    assert!(ported.allows(JobNetwork::None) && ported.allows(JobNetwork::AllowList));
    assert!(!ported.allows(JobNetwork::Open));
    assert_eq!(
        NetworkCeiling::from_capabilities(&caps(&[("os", "linux")])),
        Ok(NetworkCeiling::default())
    );
    assert_eq!(
        (EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY),
        ("egress", "egress-ports")
    );

    let old = hello();
    assert!(!old.capabilities.contains_key(EGRESS_CAPABILITY));
    assert_eq!(old.validate(), Ok(()));
    let mut new = hello();
    ported.insert_into(&mut new.capabilities);
    assert_eq!(new.validate(), Ok(()));
    let widest: Vec<String> = (0..MAX_PORTS)
        .map(|n| format!("{}-65535", 65000 + n))
        .collect();
    let mut widest_hello = hello();
    widest_hello
        .capabilities
        .insert("egress".into(), "open".into());
    widest_hello
        .capabilities
        .insert("egress-ports".into(), widest.join(","));
    assert_eq!(widest_hello.validate(), Ok(()));
}

fn check_hello(pairs: &[(&str, &str)]) -> js_agent_wire::ValidationError {
    broken(
        &hello(),
        |h: &mut Hello| {
            for (key, value) in pairs {
                h.capabilities.insert((*key).into(), (*value).into());
            }
        },
        Hello::validate,
    )
}

/// Proves a hello whose ceiling cannot be read is refused `invalid-hello` naming the key: a
/// level this version does not know, a port list that is not `N` and `LO-HI` items from 1 to
/// 65535, one of more than 16 items, and ports with the level `none`. The longest list the
/// limit allows fits the value cap of its key, which is longer than the other capabilities'.
/// Catches a server that reads a ceiling it cannot parse as some other ceiling, and a cap
/// that refuses the longest list an agent may send.
#[test]
fn a_ceiling_that_cannot_be_read_is_refused_naming_the_key() {
    let err = check_hello(&[("egress", "wide")]);
    expect(
        err,
        "capabilities.egress",
        Rule::BadChars,
        refusal::INVALID_HELLO,
    );
    for bad in [
        "0", "65536", "80-70", "+80", " 80", "80-", "-80", "1-2-3", "80,,81", "x",
    ] {
        let err = check_hello(&[("egress", "open"), ("egress-ports", bad)]);
        expect(
            err,
            "capabilities.egress-ports",
            Rule::BadChars,
            refusal::INVALID_HELLO,
        );
    }
    let many: Vec<String> = (1..=17).map(|n| n.to_string()).collect();
    let err = check_hello(&[("egress", "open"), ("egress-ports", &many.join(","))]);
    expect(
        err,
        "capabilities.egress-ports",
        Rule::TooMany,
        refusal::INVALID_HELLO,
    );
    for level in [None, Some("none")] {
        let mut pairs = vec![("egress-ports", "80")];
        pairs.extend(level.map(|level| ("egress", level)));
        let err = check_hello(&pairs);
        expect(
            err,
            "capabilities.egress-ports",
            Rule::Inconsistent,
            refusal::INVALID_HELLO,
        );
    }
    assert_eq!(MAX_EGRESS_PORTS_CAPABILITY_VALUE_LEN, 16 * 11 + 15);
    assert_eq!(
        capability_value_max("egress-ports"),
        MAX_EGRESS_PORTS_CAPABILITY_VALUE_LEN
    );
    let longest = vec!["65535-65535"; MAX_PORTS].join(",");
    assert_eq!(longest.len(), MAX_EGRESS_PORTS_CAPABILITY_VALUE_LEN);
}

/// Proves a range reads `N` and `LO-HI` from 1 to 65535 and prints as it reads, holds both of
/// its ends, and that `new` refuses zero and a range that runs backwards. Catches a range that
/// drops its last port, and a port zero or a reversed range that would match nothing or
/// everything.
#[test]
fn a_port_range_reads_and_prints_n_and_lo_hi() {
    let one = PortRange::parse("80").unwrap();
    assert_eq!(
        (one.lo(), one.hi(), one.to_string()),
        (80, 80, "80".to_owned())
    );
    let span = PortRange::parse("8000-8010").unwrap();
    assert_eq!((span.lo(), span.hi()), (8000, 8010));
    assert_eq!(span.to_string(), "8000-8010");
    assert!(span.contains(8000) && span.contains(8010) && !span.contains(8011));
    assert_eq!(PortRange::parse("1-65535"), PortRange::new(1, 65535));
    assert_eq!(PortRange::new(0, 1), None);
    assert_eq!(PortRange::new(2, 1), None);
    assert_eq!(PortRange::parse("00080"), PortRange::new(80, 80));
    assert_eq!(PortRange::parse("000080"), None);
}

/// Proves a ceiling's value that the generic text rules refuse is still named by its key:
/// an empty value, one with a control character and one a byte over the key's cap are each
/// `invalid-hello` naming `capabilities.egress` or `capabilities.egress-ports`, with the rule
/// that failed, while another capability keeps the field `capabilities`. Catches a refusal a
/// server cannot tell from any other malformed capability.
#[test]
fn a_ceiling_value_the_text_rules_refuse_names_its_key() {
    for (key, field) in [
        ("egress", "capabilities.egress"),
        ("egress-ports", "capabilities.egress-ports"),
    ] {
        let over = "1".repeat(capability_value_max(key) + 1);
        for (value, rule) in [
            ("", Rule::Empty),
            ("open\u{7}", Rule::BadChars),
            (over.as_str(), Rule::TooLong),
        ] {
            let err = check_hello(&[("egress", "open"), (key, value)]);
            expect(err, field, rule, refusal::INVALID_HELLO);
        }
    }
    let err = check_hello(&[("toolchain", "")]);
    expect(err, "capabilities", Rule::Empty, refusal::INVALID_HELLO);
}

/// Proves writing a ceiling replaces the one written before it and leaves the rest alone: a
/// ported ceiling followed by one with no port, and by the default, leaves no
/// `egress-ports`, and the map reads back as the second ceiling; the other capabilities stay.
/// Catches a stale port list that keeps an allowance the agent no longer has, or that makes
/// the agent's own hello unreadable (ports beside `none`).
#[test]
fn writing_a_ceiling_replaces_the_one_before_it() {
    let ported = ceiling(JobNetwork::Open, &["80"]);
    for next in [ceiling(JobNetwork::Open, &[]), NetworkCeiling::default()] {
        let mut capabilities = caps(&[("os", "linux")]);
        ported.insert_into(&mut capabilities);
        assert!(capabilities.contains_key("egress-ports"));
        next.insert_into(&mut capabilities);
        let mut want = caps(&[("os", "linux")]);
        want.insert("egress".into(), next.level.name().into());
        assert_eq!(capabilities, want);
        assert_eq!(NetworkCeiling::from_capabilities(&capabilities), Ok(next));
    }
}

/// The capability rules of the release before the network ceiling, copied: every value but
/// the bundle list at most 128 bytes, the bundle list at most `16 * 72 - 1`.
fn previous_receiver_accepts(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities.iter().all(|(key, value)| {
        let max = match key.as_str() {
            "bundle" => 16 * 72 - 1,
            _ => 128,
        };
        !value.is_empty() && value.len() <= max
    })
}

/// Proves a hello carrying the ceiling stays readable by a server from before it: a port
/// list that fits the previous value cap is written and that server's rules accept it; the
/// longest list this version reads (16 ranges, 191 bytes) is left out, the hello reads as no
/// port here and passes the previous rules too. Catches a hello that an older server refuses,
/// which would cost the host its registration.
#[test]
fn a_hello_with_the_ceiling_passes_the_previous_receivers_bounds() {
    let short = ceiling(JobNetwork::Open, &["80", "8000-8010"]);
    let mut written = caps(&[("os", "linux")]);
    short.insert_into(&mut written);
    assert_eq!(written["egress-ports"], "80,8000-8010");
    assert!(previous_receiver_accepts(&written));

    let widest: Vec<String> = (0..MAX_PORTS)
        .map(|n| format!("{}-65535", 65000 + n))
        .collect();
    let names: Vec<&str> = widest.iter().map(String::as_str).collect();
    let long = ceiling(JobNetwork::Open, &names);
    assert_eq!(
        long.ports_text().len(),
        MAX_EGRESS_PORTS_CAPABILITY_VALUE_LEN
    );
    let mut written = caps(&[("os", "linux")]);
    long.insert_into(&mut written);
    assert!(!written.contains_key("egress-ports"), "{written:?}");
    assert!(previous_receiver_accepts(&written));
    let read = NetworkCeiling::from_capabilities(&written).expect("readable");
    assert_eq!((read.level, read.ports.len()), (JobNetwork::Open, 0));
    let mut hello = hello();
    hello.capabilities = written;
    assert_eq!(hello.validate(), Ok(()));
}

/// The ports of `native_job()`'s native body set to `ports`, checked by `JobSpec::validate`.
fn check_native_ports(ports: Vec<PortMap>) -> js_agent_wire::ValidationError {
    broken(
        &native_job(),
        |j| j.native.as_mut().unwrap().ports = ports,
        JobSpec::validate,
    )
}

/// Proves that the ports of a native body are held to every bound of a container job's, with
/// the same rule and code, named under `native.ports`: a zero host or container port, a host
/// port given twice and a seventeenth port are refused `invalid-job`; sixteen, a port below
/// 1024 and one container port behind two host ports are valid. A body with no ports encodes
/// without the key, so the wire form of every native start from before the field is
/// unchanged, and a body with one encodes it as a container job's `ports` are. Catches a
/// native body whose ports go unchecked (an unbounded list or port 0 reaching an agent), a
/// second set of bounds that drifts from the container job's, and an empty `ports` that
/// changes the text an older agent reads.
#[test]
fn ports_on_the_native_body_validate_like_the_container_job() {
    let err = check_native_ports(vec![port(0, 80)]);
    expect(
        err,
        "native.ports[0].host",
        Rule::OutOfRange,
        refusal::INVALID_JOB,
    );
    let err = check_native_ports(vec![port(8080, 80), port(8081, 0)]);
    expect(
        err,
        "native.ports[1].container",
        Rule::OutOfRange,
        refusal::INVALID_JOB,
    );
    let err = check_native_ports(vec![port(8080, 80), port(8081, 81), port(8080, 82)]);
    expect(
        err,
        "native.ports[2].host",
        Rule::Duplicate,
        refusal::INVALID_JOB,
    );
    let many: Vec<PortMap> = (1..=17).map(|n| port(8000 + n, 80)).collect();
    let err = check_native_ports(many.clone());
    expect(err, "native.ports", Rule::TooMany, refusal::INVALID_JOB);
    let container = check_ports(many);
    assert_eq!(
        (container.rule(), container.code()),
        (Rule::TooMany, refusal::INVALID_JOB),
        "the container job's bound is the same"
    );

    let mut spec = native_job();
    let body = spec.native.as_mut().unwrap();
    body.ports = (1..=16).map(|n| port(8000 + n, 80)).collect();
    assert_eq!(spec.validate(), Ok(()));
    let body = spec.native.as_mut().unwrap();
    body.ports = vec![port(80, 8080), port(443, 8080), port(1, 65535)];
    assert_eq!(spec.validate(), Ok(()));
    let value = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        value["native"]["ports"],
        json!([
            {"host": 80, "container": 8080},
            {"host": 443, "container": 8080},
            {"host": 1, "container": 65535}
        ])
    );
    let round: JobSpec = serde_json::from_value(value).unwrap();
    assert_eq!(round, spec);

    let bare = serde_json::to_value(native_job()).unwrap();
    assert!(bare["native"].get("ports").is_none(), "{bare}");
}
