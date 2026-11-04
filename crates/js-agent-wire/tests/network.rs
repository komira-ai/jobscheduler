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
