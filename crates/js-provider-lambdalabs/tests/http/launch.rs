//! Launch and terminate over HTTP: what is sent, what the answers mean, and the provider
//! on top of the real client.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, ElasticProvider, GpuClass, HostName, Offer,
    PriceUnit, Reason, Refusal, RefusalCode, Released, SchedulerId,
};
use js_provider_lambdalabs::{
    ApiError, ApiKey, ElasticConfig, ErrorCode, HttpApi, InstanceId, LambdaApi, LambdaProvider,
    LaunchRequest, ManualClock, OptionTable, Tag,
};
use js_types::ExecMode;
use serde_json::{Value, json};

use crate::reads::client;
use crate::support::{PATIENT, Reply, Scratch, Server, fixture};

const LAUNCH: &str = "POST /api/v1/instance-operations/launch";
const TERMINATE: &str = "POST /api/v1/instance-operations/terminate";
const LIST: &str = "GET /api/v1/instances";
const ONE: &str = "GET /api/v1/instances/c0ffee00c0ffee00c0ffee00c0ffee01";

fn request() -> LaunchRequest {
    LaunchRequest {
        region: "region-a".to_owned(),
        instance_type: "gpu_1x_sample".to_owned(),
        ssh_key: "fixture-key".to_owned(),
        name: "js-0123456789ab-000000000001".to_owned(),
        tags: vec![Tag {
            key: "jobscheduler:pool".to_owned(),
            value: "fixture".to_owned(),
        }],
    }
}

fn body(server: &Server, index: usize) -> Value {
    serde_json::from_slice(&server.seen()[index].body).unwrap()
}

#[test]
fn a_launch_posts_the_documented_body_with_one_ssh_key_and_no_quantity() {
    let server = Server::plain();
    server.on(LAUNCH, Reply::json(200, &fixture("launch.json")));
    let ids = client(&server, "key-one").launch(&request()).unwrap();
    assert_eq!(ids[0].as_str(), "c0ffee00c0ffee00c0ffee00c0ffee01");
    let seen = &server.seen()[0];
    assert_eq!(seen.header("authorization"), Some("Bearer key-one"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(
        body(&server, 0),
        json!({
            "region_name": "region-a",
            "instance_type_name": "gpu_1x_sample",
            "ssh_key_names": ["fixture-key"],
            "name": "js-0123456789ab-000000000001",
            "tags": [{"key": "jobscheduler:pool", "value": "fixture"}],
        })
    );
}

#[test]
fn a_terminate_posts_one_id_and_reads_the_ids_the_answer_lists() {
    let server = Server::plain();
    server.on(TERMINATE, Reply::json(200, &fixture("terminate.json")));
    server.on(TERMINATE, Reply::json(200, &fixture("terminate_none.json")));
    let api = client(&server, "key-one");
    let id = InstanceId::new("c0ffee00c0ffee00c0ffee00c0ffee01").unwrap();
    assert_eq!(api.terminate(&id).unwrap(), std::slice::from_ref(&id));
    // A success that lists nothing is an empty list, not an error and not a confirmation.
    assert_eq!(api.terminate(&id).unwrap(), []);
    assert_eq!(
        body(&server, 0),
        json!({"instance_ids": ["c0ffee00c0ffee00c0ffee00c0ffee01"]})
    );
}

#[test]
fn a_terminate_of_a_missing_instance_is_the_documented_404() {
    let server = Server::plain();
    server.on(
        TERMINATE,
        Reply::json(404, &fixture("error_object_does_not_exist.json")),
    );
    let id = InstanceId::new("c0ffee00c0ffee00c0ffee00c0ffee01").unwrap();
    assert!(client(&server, "k").terminate(&id).unwrap_err().is_absent());
}

#[test]
fn launch_and_terminate_answers_that_are_not_the_documented_shape_are_malformed() {
    let id = InstanceId::new("c0ffee00c0ffee00c0ffee00c0ffee01").unwrap();
    let launches = [
        r#"{"data": {"instance_ids": ["has space"]}}"#,
        r#"{"data": {"instance_ids": "x"}}"#,
        r#"{"data": {}}"#,
        r#"{"data": []}"#,
        "not json",
    ];
    for text in launches {
        let server = Server::plain();
        server.on(LAUNCH, Reply::json(200, text));
        let result = client(&server, "k").launch(&request());
        assert!(matches!(result, Err(ApiError::Malformed(_))), "{text}");
    }
    let terminates = [
        r#"{"data": {"terminated_instances": [{"id": "has space"}]}}"#,
        r#"{"data": {"terminated_instances": [{}]}}"#,
        r#"{"data": 5}"#,
        "not json",
    ];
    for text in terminates {
        let server = Server::plain();
        server.on(TERMINATE, Reply::json(200, text));
        let result = client(&server, "k").terminate(&id);
        assert!(matches!(result, Err(ApiError::Malformed(_))), "{text}");
    }
}

#[test]
fn a_launch_error_keeps_its_code_and_request_id_and_drops_the_text() {
    let server = Server::plain();
    server.on(
        LAUNCH,
        Reply::json(400, &fixture("error_insufficient_capacity.json")),
    );
    let err = client(&server, "k").launch(&request()).unwrap_err();
    assert_eq!(
        err,
        ApiError::Api {
            status: 400,
            code: ErrorCode::InsufficientCapacity,
            request_id: Some("req-capacity".to_owned()),
        }
    );
    assert!(!format!("{err:?}").contains("CANARY"));
}

/// A provider on the real client against `server`, live, with room for `max_machines` machines.
fn provider(
    server: &Server,
    max_machines: usize,
    limit: Duration,
) -> (LambdaProvider, Arc<ManualClock>) {
    let scratch = Scratch::new("launch");
    let key = ApiKey::from_file(&scratch.key_file("key-one")).unwrap();
    let api = HttpApi::connect(&server.url("http"), &key, None, limit).unwrap();
    let clock = Arc::new(ManualClock::new());
    let table = OptionTable::parse(
        "[[option]]\ninstance_type = \"gpu_1x_sample\"\ngpu_class = \"nvidia-sample\"\ngpus = 1\n",
    )
    .unwrap();
    let config = ElasticConfig::new(
        SchedulerId::new("sched-one").unwrap(),
        "fixture-key",
        std::num::NonZeroUsize::new(max_machines).unwrap(),
    )
    .unwrap()
    .enable()
    .live();
    (
        LambdaProvider::new(Arc::new(api), clock.clone(), table, config),
        clock,
    )
}

fn ask() -> AcquireRequest {
    let acquire = AcquireId::new("acq-1").unwrap();
    AcquireRequest {
        name: HostName::derive(&SchedulerId::new("sched-one").unwrap(), &acquire),
        acquire,
        pool: "pool-a".to_owned(),
        offer: Offer {
            id: "gpu_1x_sample".to_owned(),
            zone: Some("region-a".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            disk_bytes: 100 << 30,
            gpus: 1,
            gpu_class: Some(GpuClass::parse("nvidia-sample").unwrap()),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
        },
        labels: Vec::new(),
        payload: BootPayload::new(String::new()),
    }
}

fn empty_list(server: &Server) {
    server.on(LIST, Reply::json(200, &fixture("instances_empty.json")));
}

#[test]
fn the_provider_lists_then_launches_with_the_ownership_tags_and_name() {
    let server = Server::plain();
    empty_list(&server);
    server.on(LAUNCH, Reply::json(200, &fixture("launch.json")));
    let (provider, _) = provider(&server, 2, PATIENT);
    let Acquired::Accepted { held, .. } = provider.acquire(&ask()) else {
        panic!("not accepted");
    };
    assert_eq!(held.id.as_str(), "c0ffee00c0ffee00c0ffee00c0ffee01");
    let methods: Vec<String> = server
        .seen()
        .iter()
        .map(|s| format!("{} {}", s.method, s.target))
        .collect();
    assert_eq!(methods, [LIST, LAUNCH].map(str::to_owned).to_vec());
    let sent = body(&server, 1);
    assert_eq!(sent["instance_type_name"], "gpu_1x_sample");
    assert!(sent["name"].as_str().unwrap().starts_with("js-"));
    let tags: Vec<(String, String)> = sent["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["key"].as_str().unwrap().to_owned(),
                t["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        tags,
        [
            ("jobscheduler:scheduler", "sched-one"),
            ("jobscheduler:acquire", "acq-1"),
            ("jobscheduler:pool", "pool-a"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
}

#[test]
fn statuses_and_codes_of_a_launch_become_the_provider_answers() {
    let refused = |code, reason| Acquired::Refused(Refusal { code, reason });
    let cases = [
        (
            400,
            "error_insufficient_capacity.json",
            refused(
                RefusalCode::Capacity,
                Reason::Api(ErrorCode::InsufficientCapacity.to_string()),
            ),
        ),
        (
            400,
            "error_quota_exceeded.json",
            refused(
                RefusalCode::Quota,
                Reason::Api(ErrorCode::QuotaExceeded.to_string()),
            ),
        ),
        (
            401,
            "error_invalid_api_key.json",
            refused(
                RefusalCode::Denied,
                Reason::Api(ErrorCode::InvalidApiKey.to_string()),
            ),
        ),
        (
            403,
            "error_account_inactive.json",
            refused(
                RefusalCode::Denied,
                Reason::Api(ErrorCode::AccountInactive.to_string()),
            ),
        ),
        (429, "error_rate_limited.json", Acquired::Unknown),
        (500, "error_internal.json", Acquired::Unknown),
        (503, "error_provider_unavailable.json", Acquired::Unknown),
    ];
    for (status, file, want) in cases {
        let server = Server::plain();
        empty_list(&server);
        server.on(LAUNCH, Reply::json(status, &fixture(file)));
        let (provider, _) = provider(&server, 2, PATIENT);
        assert_eq!(provider.acquire(&ask()), want, "{status} {file}");
        // One launch request, never a second try inside the call.
        let launches = server.seen().iter().filter(|s| s.method == "POST").count();
        assert_eq!(launches, 1, "{status} {file}");
    }
}

#[test]
fn a_launch_that_never_answers_is_unknown_at_the_limit() {
    let server = Server::plain();
    empty_list(&server);
    server.on(LAUNCH, Reply::Hang);
    let (provider, _) = provider(&server, 2, Duration::from_millis(300));
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(provider.acquire(&ask()));
    });
    let answer = receive
        .recv_timeout(Duration::from_secs(20))
        .expect("the call outlived the client's limit");
    assert_eq!(answer, Acquired::Unknown);
}

#[test]
fn a_list_that_cannot_be_read_stops_the_launch() {
    let server = Server::plain();
    server.on(LIST, Reply::json(500, &fixture("error_internal.json")));
    server.on(LAUNCH, Reply::json(200, &fixture("launch.json")));
    let (provider, _) = provider(&server, 2, PATIENT);
    assert_eq!(provider.acquire(&ask()), Acquired::Unknown);
    assert!(server.seen().iter().all(|s| s.method == "GET"));
}

#[test]
fn a_list_with_a_page_token_is_not_taken_for_everything() {
    let server = Server::plain();
    server.on(
        LIST,
        Reply::json(200, r#"{"data": [], "page_token": "more"}"#),
    );
    server.on(LAUNCH, Reply::json(200, &fixture("launch.json")));
    let (provider, _) = provider(&server, 2, PATIENT);
    assert_eq!(provider.acquire(&ask()), Acquired::Unknown);
    assert!(server.seen().iter().all(|s| s.method == "GET"));
}

#[test]
fn a_release_over_http_reads_terminates_one_id_and_reads_back() {
    let server = Server::plain();
    // An instance that is ours: the name and tags the provider would have given.
    let (provider, clock) = provider(&server, 2, PATIENT);
    empty_list(&server);
    server.on(LAUNCH, Reply::json(200, &fixture("launch.json")));
    let Acquired::Accepted { held, .. } = provider.acquire(&ask()) else {
        panic!("not accepted");
    };
    let sent = body(&server, 1);
    let instance = |status: &str| {
        let mut one: Value = serde_json::from_str(&fixture("instance.json")).unwrap();
        one["data"]["id"] = json!(held.id.as_str());
        one["data"]["status"] = json!(status);
        one["data"]["name"] = sent["name"].clone();
        one["data"]["tags"] = sent["tags"].clone();
        one.to_string()
    };
    server.on(ONE, Reply::json(200, &instance("active")));
    server.on(ONE, Reply::json(200, &instance("terminating")));
    server.on(TERMINATE, Reply::json(200, &fixture("terminate.json")));
    assert_eq!(
        provider.release(&held.id),
        Released::Terminating(js_elastic::HostStatus::Terminating)
    );
    let seen: Vec<String> = server
        .seen()
        .iter()
        .skip(2)
        .map(|s| format!("{} {}", s.method, s.target))
        .collect();
    assert_eq!(seen, [ONE, TERMINATE, ONE].map(str::to_owned));
    assert_eq!(
        body(&server, 3),
        json!({"instance_ids": ["c0ffee00c0ffee00c0ffee00c0ffee01"]})
    );
    // The documented 404 on the next read, after a list has worked, is gone.
    server.on(
        ONE,
        Reply::json(404, &fixture("error_object_does_not_exist.json")),
    );
    clock.advance(Duration::from_secs(1));
    assert_eq!(provider.release(&held.id), Released::Gone);
}
