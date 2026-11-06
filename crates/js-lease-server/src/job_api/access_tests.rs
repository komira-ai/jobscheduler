//! The roles of the job service's clients over the transports, in one process: what a
//! `client` and an `operator` may call (every method, on its own job, another client's and
//! one recorded with no client), and the one log line of every call, with who called, what it
//! named and how it ended. No socket is bound and nothing sleeps.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use js_agent_wire::Phase as AgentPhase;
use js_jobs::{ClientName, NewJob, Retention};
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, CordonHostRequest, CordonHostResponse, GetJobRequest,
    GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultRequest, GetResultResponse,
    GetUsageRequest, GetUsageResponse, ListHostsRequest, ListHostsResponse, ListJobsRequest,
    ListJobsResponse, ListPoolsRequest, ListPoolsResponse, ListProvidersRequest,
    ListProvidersResponse, SubmitJobRequest, SubmitJobResponse, UncordonHostRequest,
    UncordonHostResponse,
};
use js_registry::{HostState, HostView};
use js_state::ManualClock;
use js_types::{HostId, MemoryHeadroom, Resources, Shape};
use serde_json::json;
use tonic::{Code, Status};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Metadata, Subscriber};

use super::fleet::{self, Hosts, Pools, Refused};
use super::read_tests::json_of;
use super::tests::TOKEN;
use super::writes_rig::{Fixed, Writes, id as job_id, wire};
use super::{JobApi, submit};
use crate::adapt::SharedClock;
use crate::agent_api::job_rig::{OneClock, START_MS, tables_on};
use crate::config::{Clients, Entry, LiveClients, Role, parse_token};
use crate::hosts::{HostList, LeasesOn};
use crate::status::PoolsView;

/// The token of `ci`, a client.
const CI: &str = "ci-token-0123456789-abcdefghijklmnopqrst";

/// The token of `kbf`, a client.
const KBF: &str = "kbf-token-0123456789-abcdefghijklmnopqrs";

/// The token of `ops`, an operator.
const OPS: &str = "ops-token-0123456789-abcdefghijklmnopqrs";

/// The clients `default` (operator, [`TOKEN`]), `ci` and `kbf` (clients) and `ops`
/// (operator).
fn clients() -> LiveClients {
    let token = |text: &str| parse_token(text.as_bytes()).expect("a valid token");
    let mut clients = Clients::only(&token(TOKEN), "--token-file");
    for (name, role, text) in [
        ("ci", Role::Client, CI),
        ("kbf", Role::Client, KBF),
        ("ops", Role::Operator, OPS),
    ] {
        let entry = Entry {
            name: ClientName::parse(name).expect("a client name"),
            role,
            place: format!("--client-token {name}"),
            file: std::path::PathBuf::new(),
        };
        assert_eq!(clients.add(entry, &token(text)), Ok(()));
    }
    LiveClients::new(clients)
}

/// The registered host `h1`, serving, as the registry views it.
fn h1() -> HostView {
    let shape = Shape::new(
        Resources::new(1000, 1 << 20, 0, None).expect("resources"),
        BTreeMap::new(),
    );
    HostView {
        id: HostId::new("h1").expect("a host id"),
        state: HostState::Live,
        phase: AgentPhase::Serving,
        lease: None,
        leases: Vec::new(),
        shape,
        memory: MemoryHeadroom::full(1 << 20),
        claim: None,
        labels: BTreeMap::new(),
        agent_version: None,
        epoch: 1,
        heard_since_hello: true,
        releasing: false,
        taint: None,
        mark: None,
        reserved_for: None,
        drain_deadline: None,
        cordon_until: None,
        load: None,
    }
}

/// Hosts that list `h1` and acknowledge every cordon and uncordon.
struct AnyHost;

#[tonic::async_trait]
impl Hosts for AnyHost {
    async fn hosts(&self) -> (HostList, LeasesOn) {
        (HostList::of(Vec::new(), vec![h1()], None), LeasesOn::new())
    }

    async fn cordon(&self, _: &HostId, _: Duration) -> Result<bool, Refused> {
        Ok(true)
    }

    async fn uncordon(&self, _: &HostId) -> Result<bool, Refused> {
        Ok(true)
    }
}

/// No pools.
struct NoPools;

impl Pools for NoPools {
    fn pools(&self) -> PoolsView {
        PoolsView {
            pools: Vec::new(),
            warnings: Vec::new(),
            logs: None,
        }
    }
}

/// The service over fresh tables, with a served submit that admits every job and a served
/// fleet group, behind the bearer layer of [`clients`].
async fn served() -> Writes {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(clock));
    let submit = submit::Ctx::served(super::writes_rig::CEILING, Fixed::new(true));
    let fleet = fleet::Ctx::served(Arc::new(AnyHost), Arc::new(NoPools), None);
    let api = JobApi::new(Arc::new(jobs), shared)
        .with_submit(submit)
        .with_fleet(fleet);
    Writes::over(api, clients()).await
}

/// The code and message of an answer, `(OK, "")` for a response.
fn status<T>(answer: Result<T, Status>) -> (Code, String) {
    match answer {
        Ok(_) => (Code::Ok, String::new()),
        Err(status) => (status.code(), status.message().to_owned()),
    }
}

/// The calls of the matrix, each made with a token over gRPC.
#[derive(Debug, Clone, Copy)]
enum Call {
    Submit(&'static str),
    Get(&'static str),
    Result(&'static str),
    Logs(&'static str),
    List,
    Cancel(&'static str),
    ListHosts,
    Cordon,
    Uncordon,
    ListPools,
    ListProviders,
    GetUsage,
}

/// The code and message `call` is answered with `token`.
async fn answer(rig: &Writes, token: &str, call: Call) -> (Code, String) {
    let job = |id: &str| id.to_owned();
    match call {
        Call::Submit(id) => {
            let request = SubmitJobRequest {
                spec: Some(wire(id)),
            };
            status(
                rig.grpc_as::<_, SubmitJobResponse>(token, "SubmitJob", request)
                    .await,
            )
        }
        Call::Get(id) => {
            let request = GetJobRequest { job_id: job(id) };
            status(
                rig.grpc_as::<_, GetJobResponse>(token, "GetJob", request)
                    .await,
            )
        }
        Call::Result(id) => {
            let request = GetResultRequest { job_id: job(id) };
            status(
                rig.grpc_as::<_, GetResultResponse>(token, "GetResult", request)
                    .await,
            )
        }
        Call::Logs(id) => {
            let request = GetLogsRequest { job_id: job(id) };
            status(
                rig.grpc_as::<_, GetLogsResponse>(token, "GetLogs", request)
                    .await,
            )
        }
        Call::List => {
            let request = ListJobsRequest::default();
            status(
                rig.grpc_as::<_, ListJobsResponse>(token, "ListJobs", request)
                    .await,
            )
        }
        Call::Cancel(id) => {
            let request = CancelJobRequest { job_id: job(id) };
            status(
                rig.grpc_as::<_, CancelJobResponse>(token, "CancelJob", request)
                    .await,
            )
        }
        Call::ListHosts => status(
            rig.grpc_as::<_, ListHostsResponse>(token, "ListHosts", ListHostsRequest {})
                .await,
        ),
        Call::Cordon => {
            let request = CordonHostRequest {
                host_id: "h1".to_owned(),
                cordon_secs: None,
            };
            status(
                rig.grpc_as::<_, CordonHostResponse>(token, "CordonHost", request)
                    .await,
            )
        }
        Call::Uncordon => {
            let request = UncordonHostRequest {
                host_id: "h1".to_owned(),
            };
            status(
                rig.grpc_as::<_, UncordonHostResponse>(token, "UncordonHost", request)
                    .await,
            )
        }
        Call::ListPools => status(
            rig.grpc_as::<_, ListPoolsResponse>(token, "ListPools", ListPoolsRequest {})
                .await,
        ),
        Call::ListProviders => status(
            rig.grpc_as::<_, ListProvidersResponse>(
                token,
                "ListProviders",
                ListProvidersRequest {},
            )
            .await,
        ),
        Call::GetUsage => {
            let request = GetUsageRequest::default();
            status(
                rig.grpc_as::<_, GetUsageResponse>(token, "GetUsage", request)
                    .await,
            )
        }
    }
}

/// Records the queued job `id` of `client` (`None`: recorded before clients were named).
fn recorded(rig: &Writes, id: &str, client: Option<&str>) {
    let new = NewJob {
        id: Some(job_id(id)),
        spec: crate::agent_api::job_rig::spec(),
        priority: 0,
        client: client.map(|name| ClientName::parse(name).expect("a client name")),
    };
    rig.jobs.insert_queued(new).expect("recorded");
}

/// The ids `ListJobs` answers `token`, with the filter `client`.
async fn listed(rig: &Writes, token: &str, client: Option<&str>) -> Vec<String> {
    let request = ListJobsRequest {
        client: client.map(str::to_owned),
        ..ListJobsRequest::default()
    };
    let page: ListJobsResponse = rig
        .grpc_as(token, "ListJobs", request)
        .await
        .expect("a listing");
    page.jobs.into_iter().map(|job| job.id).collect()
}

/// Whether the cancel of the job `id` was asked for.
fn cancel_asked(rig: &Writes, id: &str) -> bool {
    let job = rig.jobs.get_job(&job_id(id)).expect("read");
    job.expect("the job is recorded").cancel_requested
}

/// Asserts the exact answer of every call to the client `ci` and to the operator `ops`: both
/// submit; each reads, lists and cancels its own jobs; `ci` is answered `unknown-job` for the
/// job of `kbf` and the job with no client (`GetJob`, `GetResult`, `GetLogs`, `CancelJob`),
/// lists only its own jobs (also when it asks for `kbf`'s), and is refused every fleet call
/// `PERMISSION_DENIED` `operator-only`; `ops` reads, lists and cancels every job and makes
/// every fleet call. A refused cancel asks for nothing. Catches a client that cancels or reads
/// another client's job, a listing that shows a client other clients' jobs, a client that
/// cordons a host, an operator held to its own jobs, and a denial that still commits.
#[tokio::test]
async fn each_role_is_answered_as_the_role_table_says() {
    let rig = served().await;
    recorded(&rig, "kbf-job", Some("kbf"));
    recorded(&rig, "kbf-two", Some("kbf"));
    recorded(&rig, "none-job", None);
    let ok = (Code::Ok, String::new());
    let unknown = (Code::NotFound, "unknown-job".to_owned());
    let not_finished = (Code::FailedPrecondition, "not-finished".to_owned());
    let operator_only = (Code::PermissionDenied, "operator-only".to_owned());
    let fleet = [
        Call::ListHosts,
        Call::Cordon,
        Call::Uncordon,
        Call::ListPools,
        Call::ListProviders,
        Call::GetUsage,
    ];
    let client_rows = [
        (Call::Submit("ci-job"), ok.clone()),
        (Call::Get("ci-job"), ok.clone()),
        (Call::Result("ci-job"), not_finished.clone()),
        (Call::Logs("ci-job"), ok.clone()),
        (Call::List, ok.clone()),
        (Call::Get("kbf-job"), unknown.clone()),
        (Call::Result("kbf-job"), unknown.clone()),
        (Call::Logs("kbf-job"), unknown.clone()),
        (Call::Get("none-job"), unknown.clone()),
        (Call::Cancel("kbf-job"), unknown.clone()),
        (Call::Cancel("none-job"), unknown.clone()),
        (Call::Cancel("ci-job"), ok.clone()),
    ];
    for (call, wants) in client_rows {
        assert_eq!(answer(&rig, CI, call).await, wants, "client {call:?}");
    }
    for call in fleet {
        assert_eq!(
            answer(&rig, CI, call).await,
            operator_only,
            "client {call:?}"
        );
    }
    assert!(
        !cancel_asked(&rig, "kbf-job"),
        "a client's refused cancel committed"
    );
    assert!(
        !cancel_asked(&rig, "none-job"),
        "a client's refused cancel committed"
    );
    assert_eq!(listed(&rig, CI, None).await, ["ci-job"]);
    assert_eq!(listed(&rig, CI, Some("kbf")).await, Vec::<String>::new());

    let operator_rows = [
        (Call::Submit("ops-job"), ok.clone()),
        (Call::Get("ops-job"), ok.clone()),
        (Call::Get("kbf-job"), ok.clone()),
        (Call::Get("none-job"), ok.clone()),
        (Call::Result("kbf-job"), not_finished.clone()),
        (Call::Logs("kbf-job"), ok.clone()),
        (Call::List, ok.clone()),
        (Call::Cancel("ops-job"), ok.clone()),
        (Call::Cancel("kbf-job"), ok.clone()),
        (Call::Cancel("none-job"), ok.clone()),
    ];
    for (call, wants) in operator_rows {
        assert_eq!(answer(&rig, OPS, call).await, wants, "operator {call:?}");
    }
    for call in fleet {
        assert_eq!(answer(&rig, OPS, call).await, ok, "operator {call:?}");
    }
    assert!(
        job_ended(&rig, "kbf-job"),
        "an operator's cancel of a queued job ends it"
    );
    assert!(
        job_ended(&rig, "ci-job"),
        "a client's cancel of its own queued job ends it"
    );
    let every = ["ci-job", "kbf-job", "kbf-two", "none-job", "ops-job"];
    assert_eq!(listed(&rig, OPS, None).await, every);
    assert_eq!(listed(&rig, OPS, Some("kbf")).await, ["kbf-job", "kbf-two"]);
    assert_eq!(
        listed(&rig, TOKEN, None).await,
        every,
        "default is an operator"
    );
}

/// Whether the job `id` has ended.
fn job_ended(rig: &Writes, id: &str) -> bool {
    let job = rig.jobs.get_job(&job_id(id)).expect("read");
    job.expect("the job is recorded").state.is_ended()
}

/// Asserts that over the gateway a client's fleet call is 403 `operator-only` and its cancel
/// of another client's job is 404 `unknown-job`, the same body as a job id with no row, while
/// an operator's cordon is 200. Catches a denial the gateway answers 500, and a hidden job a
/// client can tell from a missing one by its answer.
#[tokio::test]
async fn the_gateway_answers_a_denial_403_and_another_clients_job_404() {
    let rig = served().await;
    recorded(&rig, "kbf-job", Some("kbf"));
    let cordon = rig
        .http_as(CI, "POST", "/v1/hosts/h1:cordon", Some(&json!({})))
        .await;
    assert_eq!(cordon.0, StatusCode::FORBIDDEN);
    assert_eq!(json_of(&cordon.1), json!({ "error": "operator-only" }));
    let hidden = rig
        .http_as(CI, "POST", "/v1/jobs/kbf-job:cancel", Some(&json!({})))
        .await;
    let missing = rig
        .http_as(CI, "POST", "/v1/jobs/no-such-job:cancel", Some(&json!({})))
        .await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden.1, missing.1, "a hidden job reads as a missing one");
    let by_operator = rig
        .http_as(OPS, "POST", "/v1/hosts/h1:cordon", Some(&json!({})))
        .await;
    assert_eq!(by_operator.0, StatusCode::OK);
}

/// One event a subscriber collected: its level, its target and its fields, each as text (a
/// string as it is, any other value by its `Debug`), the message under `message`.
#[derive(Debug, Clone)]
pub(super) struct Event {
    pub(super) level: Level,
    pub(super) target: String,
    pub(super) fields: BTreeMap<String, String>,
}

/// A subscriber that keeps every event, for the thread it is the default of.
#[derive(Clone, Default)]
pub(super) struct Events(Arc<Mutex<Vec<Event>>>);

impl Events {
    /// A new collector, the default of this thread until the guard is dropped. A global
    /// subscriber that discards everything is installed first, once per process: with only
    /// thread-scoped subscribers, a callsite another thread reached while no subscriber was
    /// in scope can be cached as off, and this thread's events then lost, depending on the
    /// order the tests run in; with a global subscriber in place every callsite stays on.
    pub(super) fn scoped() -> (Events, tracing::subscriber::DefaultGuard) {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_max_level(Level::TRACE)
                .with_writer(std::io::sink)
                .try_init();
        });
        let events = Events::default();
        let guard = tracing::subscriber::set_default(events.clone());
        (events, guard)
    }

    /// Every event kept since the last call, which forgets them.
    pub(super) fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// The info events of this crate among `events`.
pub(super) fn info(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|event| event.level == Level::INFO)
        .filter(|event| event.target.starts_with("js_lease_server"))
        .collect()
}

/// Writes each field of an event into a map.
struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl Subscriber for Events {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().push(Event {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            fields,
        });
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// The fields every call's line must carry, as `(name, value)`.
fn line(
    call: &str,
    client: &str,
    role: &str,
    target: &str,
    outcome: &str,
    code: &str,
) -> BTreeMap<String, String> {
    let job = match call {
        "CordonHost" | "UncordonHost" => "",
        _ => target,
    };
    [
        ("message", "job service call"),
        ("call", call),
        ("client", client),
        ("role", role),
        ("job", job),
        ("target", target),
        ("outcome", outcome),
        ("code", code),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect()
}

/// Asserts, with a subscriber that collects every event, that each state-changing call
/// (`SubmitJob`, `CancelJob`, `CordonHost`, `UncordonHost`) writes exactly one info event of
/// this crate, whose fields are exactly the method, the client, its role, the target (the
/// job, or the host), the outcome and the code; that a client's cordon and its cancel of
/// another client's job each log `denied` (the cancel although its answer is `unknown-job`),
/// and a cancel of a job with no row `not-found`; and that no field of any event holds a
/// token, at any level. Catches a call with no line or two, a line that misses who called or what it
/// named, a denial logged as a success or as a missing job, and a token in a log.
#[tokio::test]
async fn each_state_change_logs_one_line_with_who_what_and_how_it_ended() {
    let (events, _scope) = Events::scoped();
    let rig = served().await;
    recorded(&rig, "kbf-job", Some("kbf"));
    let mut seen = events.take();
    let cases = [
        (
            CI,
            Call::Submit("ci-job"),
            line("SubmitJob", "ci", "client", "ci-job", "ok", "Ok"),
        ),
        (
            CI,
            Call::Cancel("ci-job"),
            line("CancelJob", "ci", "client", "ci-job", "ok", "Ok"),
        ),
        (
            CI,
            Call::Cancel("kbf-job"),
            line("CancelJob", "ci", "client", "kbf-job", "denied", "NotFound"),
        ),
        (
            CI,
            Call::Cancel("gone-job"),
            line(
                "CancelJob",
                "ci",
                "client",
                "gone-job",
                "not-found",
                "NotFound",
            ),
        ),
        (
            CI,
            Call::Cordon,
            line(
                "CordonHost",
                "ci",
                "client",
                "h1",
                "denied",
                "PermissionDenied",
            ),
        ),
        (
            CI,
            Call::Uncordon,
            line(
                "UncordonHost",
                "ci",
                "client",
                "h1",
                "denied",
                "PermissionDenied",
            ),
        ),
        (
            OPS,
            Call::Cordon,
            line("CordonHost", "ops", "operator", "h1", "ok", "Ok"),
        ),
        (
            OPS,
            Call::Uncordon,
            line("UncordonHost", "ops", "operator", "h1", "ok", "Ok"),
        ),
        (
            OPS,
            Call::Cancel("kbf-job"),
            line("CancelJob", "ops", "operator", "kbf-job", "ok", "Ok"),
        ),
        (
            OPS,
            Call::Submit("ci-job"),
            line("SubmitJob", "ops", "operator", "ci-job", "ok", "Ok"),
        ),
    ];
    for (token, call, wants) in cases {
        answer(&rig, token, call).await;
        let taken = events.take();
        let lines = info(&taken);
        assert_eq!(lines.len(), 1, "{call:?}: {lines:#?}");
        assert_eq!(lines[0].fields, wants, "{call:?}");
        seen.extend(taken);
    }
    for event in &seen {
        for value in event.fields.values() {
            for token in [CI, OPS, KBF, TOKEN] {
                assert!(!value.contains(token), "{event:?}");
            }
        }
    }
}

/// The service of [`served`] with a listing that reads windows of two rows and at most two
/// rows a call, so a page is cut at the bound after one window.
async fn served_in_small_windows() -> Writes {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(clock));
    let two = std::num::NonZeroUsize::new(2).expect("two");
    let read = super::read::Ctx {
        scan_window: two,
        scan_bound: two,
        ..super::read::Ctx::default()
    };
    let api = JobApi::new(Arc::new(jobs), shared).with_read(read);
    Writes::over(api, clients()).await
}

/// Every page of `ListJobs` for `token`, following the tokens to the end: each page's ids
/// and the token it answered.
async fn pages(rig: &Writes, token: &str) -> Vec<(Vec<String>, String)> {
    let mut pages = Vec::new();
    let mut at = String::new();
    loop {
        let request = ListJobsRequest {
            page_token: at.clone(),
            page_size: 10,
            ..ListJobsRequest::default()
        };
        let page: ListJobsResponse = rig
            .grpc_as(token, "ListJobs", request)
            .await
            .expect("a listing");
        let ids = page.jobs.into_iter().map(|job| job.id).collect();
        at = page.next_page_token.clone();
        pages.push((ids, page.next_page_token));
        if at.is_empty() {
            return pages;
        }
    }
}

/// Asserts, over a table where another client's jobs sit between and after a client's own,
/// and pages cut at a bound of two rows, that every page token a client is answered names
/// one of its own jobs (never the last row read, another client's), that a page cut at the
/// bound with none of its jobs reads on to its next job, and that following the tokens lists
/// exactly its jobs; while an operator's page cut at the bound still carries the last row
/// read. Catches a client told another client's job id through a page token, and a client's
/// listing that ends early or repeats a job.
#[tokio::test]
async fn a_clients_page_token_never_names_another_clients_job() {
    let rig = served_in_small_windows().await;
    for (id, client) in [
        ("a-ci", "ci"),
        ("b-kbf", "kbf"),
        ("c-kbf", "kbf"),
        ("d-kbf", "kbf"),
        ("e-ci", "ci"),
    ] {
        recorded(&rig, id, Some(client));
    }
    let by_ci = pages(&rig, CI).await;
    let tokens: Vec<&str> = by_ci.iter().map(|(_, token)| token.as_str()).collect();
    assert_eq!(tokens, ["job/a-ci", ""], "{by_ci:?}");
    let ids: Vec<String> = by_ci.into_iter().flat_map(|(ids, _)| ids).collect();
    assert_eq!(ids, ["a-ci", "e-ci"]);
    let by_ops = pages(&rig, OPS).await;
    assert_eq!(
        by_ops[0],
        (
            vec!["a-ci".to_owned(), "b-kbf".to_owned()],
            "job/b-kbf".to_owned()
        )
    );
}

/// Asserts that every method called on the service directly, with no caller (a request no
/// bearer layer admitted), is `UNAUTHENTICATED` with an empty message, the fleet calls and
/// the job calls alike, and that nothing is recorded. Catches a call that runs as nobody, or
/// as an operator, when the bearer layer is not in front of the service.
#[tokio::test]
async fn a_call_with_no_caller_is_unauthenticated() {
    use js_proto::v1::job_service_server::JobService;
    use tonic::Request;
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let jobs = Arc::new(jobs);
    let shared = SharedClock::new(OneClock(clock));
    let submit = submit::Ctx::served(super::writes_rig::CEILING, Fixed::new(true));
    let fleet = fleet::Ctx::served(Arc::new(AnyHost), Arc::new(NoPools), None);
    let api = JobApi::new(Arc::clone(&jobs), shared)
        .with_submit(submit)
        .with_fleet(fleet);
    let job = || "job-x".to_owned();
    let refused = |answer: (Code, String)| answer == (Code::Unauthenticated, String::new());
    let submit = SubmitJobRequest {
        spec: Some(wire("job-x")),
    };
    let answers = [
        status(api.submit_job(Request::new(submit)).await),
        status(
            api.get_job(Request::new(GetJobRequest { job_id: job() }))
                .await,
        ),
        status(
            api.list_jobs(Request::new(ListJobsRequest::default()))
                .await,
        ),
        status(
            api.cancel_job(Request::new(CancelJobRequest { job_id: job() }))
                .await,
        ),
        status(
            api.get_result(Request::new(GetResultRequest { job_id: job() }))
                .await,
        ),
        status(
            api.get_logs(Request::new(GetLogsRequest { job_id: job() }))
                .await,
        ),
        status(api.list_hosts(Request::new(ListHostsRequest {})).await),
        status(
            api.cordon_host(Request::new(CordonHostRequest {
                host_id: "h1".to_owned(),
                cordon_secs: None,
            }))
            .await,
        ),
        status(
            api.uncordon_host(Request::new(UncordonHostRequest {
                host_id: "h1".to_owned(),
            }))
            .await,
        ),
        status(api.list_pools(Request::new(ListPoolsRequest {})).await),
        status(
            api.list_providers(Request::new(ListProvidersRequest {}))
                .await,
        ),
        status(
            api.get_usage(Request::new(GetUsageRequest::default()))
                .await,
        ),
    ];
    for (at, answer) in answers.into_iter().enumerate() {
        assert!(refused(answer.clone()), "method {at}: {answer:?}");
    }
    assert_eq!(
        jobs.get_job(&job_id("job-x")).unwrap(),
        None,
        "a job was recorded"
    );
}
