//! The host listing as `GET /v1/hosts` writes it and as `ListHosts` tells it, over a fake
//! listing with a host of every kind, and the messages the listing is made of.

use super::*;

fn small() -> Shape {
    Shape::new(
        Resources::new(1000, 1 << 20, 0, None).unwrap(),
        BTreeMap::from([("os".to_owned(), "linux".to_owned())]),
    )
}

/// A registered host `id` in `state`, with `mark`, as the registry views it.
fn view(id: &str, state: HostState, mark: Option<HostMark>) -> HostView {
    HostView {
        id: HostId::new(id).unwrap(),
        state,
        phase: AgentPhase::Serving,
        lease: None,
        leases: Vec::new(),
        shape: small(),
        memory: MemoryHeadroom::full(1 << 20),
        claim: None,
        by_hello: false,
        labels: BTreeMap::from([("rack".to_owned(), "r2".to_owned())]),
        agent_version: Some("0.1.0".to_owned()),
        epoch: 1,
        heard_since_hello: true,
        releasing: false,
        taint: None,
        mark,
        reserved_for: None,
        drain_deadline: None,
        cordon_until: None,
        load: None,
    }
}

/// The hosts of every kind a listing shows: registered ones live (one of them with GPUs and
/// scratch disk), lost, awaiting, tainted, reserved, held, draining, releasing and cordoned.
fn every_kind() -> FakeHosts {
    let tainted = HostView {
        taint: Some(Taint {
            reason: "dirty-scratch".to_owned(),
            run_id: Some("run-1-1".to_owned()),
            check: None,
        }),
        ..view("r-tainted", HostState::Live, None)
    };
    let cordoned = HostView {
        cordon_until: Some(Timestamp::from_millis(T0 + 60_000)),
        ..view("r-cordoned", HostState::Live, None)
    };
    let gpu = Resources::new(64_000, 1 << 30, 8, Some("nvidia-a100".to_owned()))
        .unwrap()
        .with_disk_bytes(5 << 30);
    FakeHosts {
        views: vec![
            gpu_host(gpu),
            view("r-live", HostState::Live, None),
            view("r-lost", HostState::Lost, None),
            view("r-awaiting", HostState::Awaiting, None),
            tainted,
            view("r-reserved", HostState::Live, Some(HostMark::Reserved)),
            view("r-held", HostState::Live, Some(HostMark::Held)),
            view("r-draining", HostState::Live, Some(HostMark::Draining)),
            view("r-releasing", HostState::Live, Some(HostMark::Releasing)),
            cordoned,
        ],
        ..FakeHosts::new()
    }
}

/// A live registered host `s-1` booking `gpu`, with no capability, and all of its 1 GiB of
/// memory.
fn gpu_host(gpu: Resources) -> HostView {
    HostView {
        shape: Shape::new(gpu, BTreeMap::new()),
        memory: MemoryHeadroom::full(1 << 30),
        ..view("s-1", HostState::Live, None)
    }
}

/// The host that the body of `GET /v1/hosts` writes as `entry`, as `ListHosts` should tell
/// it, read from the JSON alone, with nothing booked.
fn told(entry: &Value) -> wire::Host {
    let text = |value: &Value| value.as_str().unwrap().to_owned();
    let map = |value: &Value| -> BTreeMap<String, String> {
        let pairs = value.as_object().unwrap().iter();
        pairs
            .map(|(key, value)| (key.clone(), text(value)))
            .collect()
    };
    assert_eq!(entry["source"], "registered");
    let source = wire::HostSource::Registered;
    let states = [
        ("live", wire::HostState::Live),
        ("lost", wire::HostState::Lost),
        ("awaiting", wire::HostState::Awaiting),
    ];
    let state = states
        .into_iter()
        .find(|(name, _)| entry["state"] == *name)
        .map(|(_, state)| state)
        .unwrap();
    let mark = match entry.get("mark").and_then(Value::as_str) {
        Some("reserved") => wire::HostMark::Reserved,
        Some("held") => wire::HostMark::Held,
        Some("draining") => wire::HostMark::Draining,
        Some("releasing") => wire::HostMark::Releasing,
        _ => wire::HostMark::Unspecified,
    };
    let shape = &entry["shape"];
    let memory = &entry["memory"];
    let number = |value: &Value| value.as_u64().unwrap();
    wire::Host {
        id: text(&entry["id"]),
        source: i32::from(source),
        state: i32::from(state),
        shape: Some(wire::Shape {
            cpu_millis: number(&shape["cpu_millis"]),
            memory_bytes: number(&shape["memory_bytes"]),
            gpus: u32::try_from(number(&shape["gpus"])).unwrap(),
            gpu_class: shape.get("gpu_class").map(text).unwrap_or_default(),
            disk_bytes: shape.get("disk_bytes").map(number),
        }),
        memory: Some(wire::Memory {
            configured_bytes: number(&memory["configured_bytes"]),
            schedulable_percent: u32::try_from(number(&memory["schedulable_percent"])).unwrap(),
            schedulable_bytes: number(&memory["bookable_bytes"]),
        }),
        capabilities: map(&entry["capabilities"]),
        labels: map(&entry["labels"]),
        booked: Vec::new(),
        mark: i32::from(mark),
        cordon_expires_at_ms: entry
            .get("cordon")
            .map(|cordon| number(&cordon["expires_at_ms"])),
        taint_reason: entry
            .get("taint")
            .map(|taint| text(&taint["reason"]))
            .unwrap_or_default(),
        agent_version: entry.get("agent_version").map(text).unwrap_or_default(),
    }
}

/// Asserts that for one listing with a host of every kind, each host of `ListHosts` is the
/// host the body of `GET /v1/hosts` writes for it, field by field, in the same order; and
/// that the body of a registered host with every optional field, and of one with GPUs and
/// scratch disk, is the text the route has always written. It catches the two views built by two functions that
/// drift, and a change of the body that the typed fields of the entries could make.
#[tokio::test]
async fn the_hosts_json_and_grpc_agree() {
    let hosts = every_kind();
    let list = hosts.hosts().await;
    let body = serde_json::to_value(&list).unwrap();
    let api = api_over(Arc::new(hosts), Arc::new(FakePools(no_pools)), None);
    let listed = list_hosts(&api, Request::new(ListHostsRequest {})).await;
    let listed = listed.unwrap().into_inner().hosts;
    let written: Vec<wire::Host> = body["hosts"].as_array().unwrap().iter().map(told).collect();
    assert_eq!(listed.len(), 10);
    assert_eq!(listed, written);
    let states: BTreeSet<i32> = listed.iter().map(|host| host.state).collect();
    let marks: BTreeSet<i32> = listed.iter().map(|host| host.mark).collect();
    assert_eq!((states.len(), marks.len()), (3, 5));

    let gpu = Resources::new(64_000, 1 << 30, 8, Some("nvidia-a100".to_owned()))
        .unwrap()
        .with_disk_bytes(5 << 30);
    let mut pinned = every_kind();
    pinned.views = vec![
        HostView {
            taint: Some(Taint {
                reason: "dirty-scratch".to_owned(),
                run_id: None,
                check: None,
            }),
            cordon_until: Some(Timestamp::from_millis(T0 + 60_000)),
            ..view("r-1", HostState::Lost, Some(HostMark::Draining))
        },
        gpu_host(gpu),
    ];
    let list = pinned.hosts().await;
    let body = serde_json::to_string(&list).unwrap();
    let expected = concat!(
        r#"{"hosts":[{"id":"r-1","source":"registered","state":"lost","lost":true,"#,
        r#""phase":"serving","shape":{"cpu_millis":1000,"memory_bytes":1048576,"gpus":0},"#,
        r#""memory":{"configured_bytes":1048576,"schedulable_percent":100,"#,
        r#""bookable_bytes":1048576},"capabilities":{"os":"linux"},"labels":{"rack":"r2"},"#,
        r#""agent_version":"0.1.0","releasing":false,"taint":{"reason":"dirty-scratch"},"#,
        r#""mark":"draining","cordon":{"expires_at_ms":1800000060000}},"#,
        r#"{"id":"s-1","source":"registered","state":"live","lost":false,"#,
        r#""phase":"serving","shape":{"cpu_millis":64000,"#,
        r#""memory_bytes":1073741824,"gpus":8,"gpu_class":"nvidia-a100","#,
        r#""disk_bytes":5368709120},"memory":{"configured_bytes":1073741824,"#,
        r#""schedulable_percent":100,"bookable_bytes":1073741824},"capabilities":{},"#,
        r#""labels":{"rack":"r2"},"agent_version":"0.1.0","releasing":false}]}"#,
    );
    assert_eq!(body, expected);
}

/// Asserts that a cordon whose host is gone from the listing by the time it is read back (a
/// host forgotten in between) is `NOT_FOUND` `unknown-host`, as is a refusal of the fake.
#[tokio::test]
async fn a_cordon_whose_host_is_gone_is_not_found() {
    let api = api_over(
        Arc::new(FakeHosts::new()),
        Arc::new(FakePools(no_pools)),
        None,
    );
    let unknown = (Code::NotFound, UNKNOWN_HOST.to_owned());
    assert_eq!(refused(cordon(&api, "gone", Some(60)).await), unknown);
    assert_eq!(refused(uncordon(&api, "gone").await), unknown);
    let mut refusing = FakeHosts::new();
    refusing.answer = Err(Refused::UnknownHost);
    let api = api_over(Arc::new(refusing), Arc::new(FakePools(no_pools)), None);
    assert_eq!(refused(cordon(&api, "gone", Some(60)).await), unknown);
}

/// The message types `ListHostsResponse` is made of, with the enums they use: its own
/// message and every type a field of one of them names, transitively.
fn host_list_types() -> Vec<(String, Vec<String>)> {
    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).unwrap();
    let files = set
        .file
        .iter()
        .filter(|file| file.package() == "jobscheduler.v1");
    let mut messages = BTreeMap::new();
    let mut enums: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        for message in &file.message_type {
            let fields = message.field.iter().map(|field| field.name().to_owned());
            let types = message
                .field
                .iter()
                .map(|field| field.type_name().to_owned());
            let entry = (fields.collect::<Vec<_>>(), types.collect::<Vec<_>>());
            messages.insert(format!(".jobscheduler.v1.{}", message.name()), entry);
        }
        for named in &file.enum_type {
            let values = named.value.iter().map(|value| value.name().to_owned());
            enums.insert(
                format!(".jobscheduler.v1.{}", named.name()),
                values.collect(),
            );
        }
    }
    let mut seen: Vec<(String, Vec<String>)> = Vec::new();
    let mut next = vec![".jobscheduler.v1.ListHostsResponse".to_owned()];
    while let Some(name) = next.pop() {
        if let Some((fields, types)) = messages.get(&name) {
            next.extend(types.iter().filter(|name| !name.is_empty()).cloned());
            seen.push((name, fields.clone()));
        } else if let Some(values) = enums.get(&name) {
            seen.push((name, values.clone()));
        }
    }
    seen
}

/// The words of a name in any of the spellings the descriptors use (`HostBooking`,
/// `host_ports`, `HOST_MARK_HELD`, `.jobscheduler.v1.Host`), in lower case.
fn words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut last_lower = false;
    for c in name.chars() {
        let starts = c.is_ascii_uppercase() & last_lower;
        let parts = !c.is_ascii_alphanumeric();
        if starts | parts | words.is_empty() {
            words.push(String::new());
        }
        if !parts {
            words.last_mut().unwrap().push(c.to_ascii_lowercase());
        }
        last_lower = c.is_ascii_lowercase() | c.is_ascii_digit();
    }
    words.retain(|word| !word.is_empty());
    words
}

/// Asserts that no message or enum the host list is made of, and none of their fields or
/// values, is named for an allocation, a lease or a renewal (the bookings are
/// `HostBooking`s under `booked`), and that the walk reaches the booking. A mark named
/// `RELEASING` is a host being handed back, not a lease. It catches an allocation message
/// leaking into the client's surface.
#[test]
fn host_list_never_shows_an_allocation_as_a_job_concept() {
    assert_eq!(words("HostBooking"), ["host", "booking"]);
    assert_eq!(words("HOST_MARK_HELD"), ["host", "mark", "held"]);
    assert_eq!(
        words(".jobscheduler.v1.LeaseInfo"),
        ["jobscheduler", "v1", "lease", "info"]
    );
    let types = host_list_types();
    let names: Vec<&str> = types.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&".jobscheduler.v1.HostBooking"), "{names:?}");
    assert!(names.contains(&".jobscheduler.v1.HostMark"), "{names:?}");
    for (name, members) in &types {
        for member in std::iter::once(name).chain(members) {
            for word in words(member) {
                for banned in ["allocation", "lease", "renew"] {
                    assert!(!word.starts_with(banned), "{name}: {member}");
                }
            }
        }
    }
}
