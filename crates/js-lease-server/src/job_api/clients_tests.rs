//! The clients of the job service over both transports, in one process: each client's token
//! names the client that submits a job, a token of no client (one removed before a restart
//! among them) reaches no handler, job ids stay one namespace across clients, a listing keeps
//! one client's jobs, and no token is printed in a configuration, a startup refusal or a
//! request's log line. No socket is bound and nothing sleeps.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use js_jobs::{ClientName, Retention};
use js_proto::v1::{
    self, GetJobRequest, GetJobResponse, ListJobsRequest, ListJobsResponse, SubmitJobRequest,
    SubmitJobResponse,
};
use js_state::ManualClock;
use serde_json::json;
use tonic::Code;

use super::read_tests::{from_json, json_of};
use super::tests::{Recording, TOKEN, call, connect, methods, send_all, value};
use super::writes_rig::{Fixed, Said, Via, Writes, id as job_id, said, to_json, wire};
use super::{gateway_over, grpc_over};
use crate::agent_api::job_rig::{OneClock, START_MS, tables_on};
use crate::clock::SharedClock;
use crate::config::clients_rig::{ClientsRig, DEFAULT_TOKEN, JOIN};
use crate::config::{Cli, Clients, Config, Entry, LiveClients, Role, parse_token};

/// The token of the client `ci`.
const CI: &str = "ci-token-0123456789-abcdefghijklmnopqrst";

/// The token of the client `ops`.
const OPS: &str = "ops-token-0123456789-abcdefghijklmnopqrs";

/// A token no client has.
const STRANGER: &str = "stranger-token-0123456789-abcdefghijklmn";

/// The clients `default` (the token [`TOKEN`]), `ci` and `ops`.
fn three() -> LiveClients {
    let token = |text: &str| parse_token(text.as_bytes()).expect("a valid token");
    let mut clients = Clients::only(&token(TOKEN), "--token-file");
    for (name, text) in [("ci", CI), ("ops", OPS)] {
        let entry = Entry {
            name: ClientName::parse(name).expect("a client name"),
            role: Role::Client,
            place: format!("--client-token {name}"),
            file: std::path::PathBuf::new(),
        };
        assert_eq!(clients.add(entry, &token(text)), Ok(()));
    }
    LiveClients::new(clients)
}

/// The service over fresh tables with a served submit that admits every job, behind the
/// bearer layer of `clients`.
async fn served(clients: LiveClients) -> Writes {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(clock));
    Writes::with_clients(Arc::new(jobs), shared, Fixed::new(true), clients).await
}

/// `SubmitJob` of `spec` with `token` over `via`.
async fn submit_as(rig: &Writes, token: &str, via: Via, spec: &v1::JobSpec) -> Said {
    let request = SubmitJobRequest {
        spec: Some(spec.clone()),
    };
    match via {
        Via::Grpc => rig
            .grpc_as::<_, SubmitJobResponse>(token, "SubmitJob", request)
            .await
            .map(|answer| answer.job.expect("an answer carries the job"))
            .map_err(|status| said(&status)),
        Via::Json => {
            let body = to_json("SubmitJobRequest", &request);
            let (status, body) = rig.http_as(token, "POST", "/v1/jobs", Some(&body)).await;
            match status {
                StatusCode::CREATED => {
                    let name = "jobscheduler.v1.SubmitJobResponse";
                    let answer: SubmitJobResponse = from_json(name, &body);
                    Ok(answer.job.expect("an answer carries the job"))
                }
                status => Err((status, json_of(&body))),
            }
        }
    }
}

/// `GetJob` of `id` with `token` over `via`: the job.
async fn read_as(rig: &Writes, token: &str, via: Via, id: &str) -> v1::Job {
    match via {
        Via::Grpc => {
            let request = GetJobRequest {
                job_id: id.to_owned(),
            };
            let answer: GetJobResponse = rig
                .grpc_as(token, "GetJob", request)
                .await
                .expect("the job is read");
            answer.job.expect("an answer carries the job")
        }
        Via::Json => {
            let uri = format!("/v1/jobs/{id}");
            let (status, body) = rig.http_as(token, "GET", &uri, None).await;
            assert_eq!(status, StatusCode::OK);
            let answer: GetJobResponse = from_json("jobscheduler.v1.GetJobResponse", &body);
            answer.job.expect("an answer carries the job")
        }
    }
}

/// The job `id` as a client submits it, labelled with `label` (a value the request carries,
/// which must not decide the client).
fn labelled(id: &str, label: &str) -> v1::JobSpec {
    v1::JobSpec {
        labels: [("client".to_owned(), label.to_owned())].into(),
        ..wire(id)
    }
}

/// Asserts that with three clients, a `SubmitJob` with each client's token succeeds over gRPC
/// and over the gateway and records that client's name, answered on the job, and that the
/// job read back with the client's own token and with the operator `default`'s, over either
/// transport, shows it; each job is
/// labelled with another client's name, which the record does not take. Catches a name not
/// attached by the bearer layer (the record would have none), a name taken from the request
/// rather than the token, and a transport that loses the name on the way to the call.
#[tokio::test]
async fn each_client_token_is_accepted_and_names_its_client() {
    let rig = served(three()).await;
    let clients = [(TOKEN, "default"), (CI, "ci"), (OPS, "ops")];
    for (at, (token, name)) in clients.iter().enumerate() {
        let other = clients[(at + 1) % clients.len()].1;
        for via in Via::BOTH {
            let id = format!("{name}-{via:?}").to_lowercase();
            let job = submit_as(&rig, token, via, &labelled(&id, other))
                .await
                .expect("the client's submit is recorded");
            assert_eq!(job.client.as_deref(), Some(*name), "{id}");
            let stored = rig.jobs.get_job(&job_id(&id)).unwrap();
            let stored = stored.expect("the job is recorded").client;
            assert_eq!(stored.as_ref().map(ClientName::as_str), Some(*name));
            for (reader, via) in [token, &TOKEN].into_iter().zip(Via::BOTH) {
                let read = read_as(&rig, reader, via, &id).await;
                assert_eq!(read.client.as_deref(), Some(*name), "{id} over {via:?}");
            }
        }
    }
}

/// Asserts, over the clients a configuration gives, that a token of no client, and the token
/// of an entry that was removed from the flags before a restart, are refused by every method
/// over gRPC (`UNAUTHENTICATED`, empty message) and every route of the gateway (401, empty
/// body), and that none of those requests reaches the recording service; while before the
/// restart the removed client's token reached it, and after it the kept client's still does.
/// Catches a revocation that does not take effect at a restart, a set that admits a token it
/// was not given, and a refusal that runs a handler.
#[tokio::test]
async fn an_unknown_or_removed_token_is_refused_with_no_handler_run() {
    let rig = ClientsRig::new("revoked");
    let both = rig.client_flags(&[("ci", CI), ("ops", OPS)]);
    let before = rig.config_of_flags(&both).expect("the flags start").clients;
    let recording = Recording::new();
    let channel = connect(grpc_over(recording.clone(), before)).await;
    let answer = call(&channel, "GetJob", Some(&format!("Bearer {OPS}"))).await;
    assert_ne!(
        answer.map_err(|status| status.code()),
        Err(Code::Unauthenticated)
    );
    assert_eq!(
        recording.calls(),
        1,
        "the client's token reached the service"
    );

    let kept = rig.client_flags(&[("ci", CI)]);
    let after = rig
        .config_of_flags(&kept)
        .expect("the restart starts")
        .clients;
    let recording = Recording::new();
    let channel = connect(grpc_over(recording.clone(), after.clone())).await;
    let router = gateway_over(recording.clone(), after);
    for token in [OPS, STRANGER] {
        let authorization = format!("Bearer {token}");
        for method in methods() {
            let status = call(&channel, &method, Some(&authorization))
                .await
                .expect_err("a removed or unknown token is refused");
            assert_eq!(
                (status.code(), status.message()),
                (Code::Unauthenticated, ""),
                "{method} with {token}"
            );
        }
        for answer in send_all(&router, Some(value(&authorization))).await {
            assert_eq!(answer, (StatusCode::UNAUTHORIZED, Bytes::new()), "{token}");
        }
    }
    assert_eq!(
        recording.calls(),
        0,
        "a refused request reached the service"
    );
    let answer = call(&channel, "GetJob", Some(&format!("Bearer {CI}"))).await;
    assert_ne!(
        answer.map_err(|status| status.code()),
        Err(Code::Unauthenticated)
    );
    assert_eq!(
        recording.calls(),
        1,
        "the kept client's token reached the service"
    );
}

/// Asserts that job ids are one namespace across clients: client A (`ci`) submits the job
/// `x`; client B (`ops`, of the role `client`) submitting the same job is `job-id-conflict`
/// (409), over gRPC and over the gateway, so it never reads A's job; the operator `default`
/// submitting the same job gets the same answer (the job, with A as its client); any other
/// job under `x` is `job-id-conflict` (409, `ALREADY_EXISTS` over gRPC) whoever sends it; and
/// the stored job still names A. Catches ids scoped per client (B would make a second job), a
/// client that reads another's job through a retry, and a retry that overwrites the stored
/// client.
#[tokio::test]
async fn a_retry_of_another_clients_id_conflicts_for_a_client_and_answers_an_operator() {
    let rig = served(three()).await;
    let first = submit_as(&rig, CI, Via::Grpc, &wire("x"))
        .await
        .expect("the first submit is recorded");
    assert_eq!(first.client.as_deref(), Some("ci"));
    for via in Via::BOTH {
        let refused = submit_as(&rig, OPS, via, &wire("x"))
            .await
            .expect_err("another client's id");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{via:?}");
        assert_eq!(refused.1["error"], json!("job-id-conflict"), "{via:?}");
        let again = submit_as(&rig, TOKEN, via, &wire("x")).await;
        assert_eq!(again, Ok(first.clone()), "an operator's retry over {via:?}");
    }
    let other = v1::JobSpec {
        args: vec!["--slow".to_owned()],
        ..wire("x")
    };
    for (token, via) in [(OPS, Via::Grpc), (OPS, Via::Json), (TOKEN, Via::Json)] {
        let refused = submit_as(&rig, token, via, &other)
            .await
            .expect_err("a conflict");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{via:?}");
        assert_eq!(refused.1["error"], json!("job-id-conflict"), "{via:?}");
    }
    let request = SubmitJobRequest {
        spec: Some(other.clone()),
    };
    let status = rig
        .grpc_as::<_, SubmitJobResponse>(OPS, "SubmitJob", request)
        .await
        .expect_err("a conflict");
    assert_eq!(status.code(), Code::AlreadyExists);
    let stored = rig.jobs.get_job(&job_id("x")).unwrap();
    let stored = stored.expect("the job is recorded").client;
    assert_eq!(stored.as_ref().map(ClientName::as_str), Some("ci"));
    assert_eq!(read_as(&rig, TOKEN, Via::Json, "x").await, first);
}

/// The ids of the jobs `ListJobs` answers with the filter `client` (none: no filter), over
/// gRPC and over the gateway, which must agree.
async fn listed(rig: &Writes, client: Option<&str>) -> Vec<String> {
    let request = ListJobsRequest {
        client: client.map(str::to_owned),
        ..ListJobsRequest::default()
    };
    let answer: ListJobsResponse = rig.grpc_as(TOKEN, "ListJobs", request).await.unwrap();
    let uri = client.map_or("/v1/jobs".to_owned(), |client| {
        format!("/v1/jobs?client={client}")
    });
    let (status, body) = rig.http_as(TOKEN, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let json: ListJobsResponse = from_json("jobscheduler.v1.ListJobsResponse", &body);
    assert_eq!(json, answer, "the transports disagree for {client:?}");
    answer.jobs.into_iter().map(|job| job.id).collect()
}

/// Asserts that `ListJobs` with a client keeps exactly the jobs that client submitted, that a
/// client with no job (or no such client) lists none, and that without the filter every
/// client's job is listed, a job recorded with no client among them (it matches no client).
/// Catches a filter that is ignored, one that compares another field, and a job with no
/// client that every filter keeps.
#[tokio::test]
async fn a_listing_by_client_keeps_that_clients_jobs() {
    let rig = served(three()).await;
    for (token, id) in [(CI, "a-ci"), (OPS, "b-ops"), (CI, "c-ci")] {
        submit_as(&rig, token, Via::Grpc, &wire(id))
            .await
            .expect("recorded");
    }
    let unnamed = js_jobs::NewJob {
        id: Some(job_id("d-none")),
        spec: crate::agent_api::job_rig::spec(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(unnamed).expect("recorded");
    assert_eq!(listed(&rig, Some("ci")).await, ["a-ci", "c-ci"]);
    assert_eq!(listed(&rig, Some("ops")).await, ["b-ops"]);
    assert_eq!(listed(&rig, Some("default")).await, Vec::<String>::new());
    assert_eq!(listed(&rig, Some("nobody")).await, Vec::<String>::new());
    let every = ["a-ci", "b-ops", "c-ci", "d-none"];
    assert_eq!(listed(&rig, None).await, every);
}

/// A log written to a buffer, for the thread that made it.
struct Captured {
    text: Arc<Mutex<Vec<u8>>>,
    _scope: tracing::subscriber::DefaultGuard,
}

/// The writer of [`Captured`].
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// Captures what this thread logs until it is dropped. The process-wide sink is installed
    /// first, in case another thread, with no subscriber of its own, reached a line first.
    fn start() -> Captured {
        let text = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&text);
        crate::api::rig::process_sink();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Buffer(Arc::clone(&sink)))
            .finish();
        let scope = tracing::subscriber::set_default(subscriber);
        Captured {
            text,
            _scope: scope,
        }
    }

    /// The lines logged so far.
    fn lines(&self) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned();
        text.lines().map(str::to_owned).collect()
    }
}

/// The token of the client `marker`, looked for in everything the server prints.
const MARKER: &str = "marker-token-0123456789-abcdefghijklmnop";

/// Asserts that the token of the client `marker` is in no printed configuration (the
/// command line's `Debug`, the clients' `Debug`), in no refusal of a start that holds it (a
/// second client with the same token, the token pasted as an entry of `--client-token`, the
/// token in the join token file, the same token in two `[[client]]` tables), and in no log
/// line of the job service's calls, made with it over both transports and with a token one
/// byte away; and that each call's line says the method, the client and the job. Catches a
/// token printed by `Debug`, quoted in a refusal, or logged with a request.
#[tokio::test]
async fn the_token_never_appears_in_debug_errors_or_logs() {
    let rig = ClientsRig::new("marker");
    let marker = rig.client_flags(&[("marker", MARKER)]);
    let cli = Cli::try_parse_from(rig.flags(&marker)).expect("the flags parse");
    let mut printed = vec![format!("{cli:?}")];
    let config = Config::from_cli(cli).expect("the flags start");
    printed.push(format!("{:?}", config.clients));
    assert_eq!(
        printed[1],
        r#"Clients([("default", "operator"), ("marker", "client")])"#
    );

    let pasted: Vec<OsString> = vec!["--client-token".into(), MARKER.into()];
    let twice = rig.client_flags(&[("marker", MARKER), ("copy", MARKER)]);
    let joined = rig.client_flags(&[("marker", JOIN)]);
    for (extra, wants) in [
        (pasted, "--client-token: entry 1: "),
        (
            twice,
            "--client-token: copy: holds the same token as --client-token marker",
        ),
        (joined, "the two must differ"),
    ] {
        let said = rig
            .config_of_flags(&extra)
            .err()
            .expect("refused")
            .to_string();
        assert!(said.contains(wants), "{wants}: {said}");
        printed.push(said);
    }
    let tables = rig.client_tables(&[("marker", MARKER), ("copy", MARKER)]);
    let text = rig.file("job_listen = \"127.0.0.1:0\"", &tables);
    let said = rig
        .config_of_file(&text)
        .err()
        .expect("refused")
        .to_string();
    let wants = "client \"copy\": token_file: holds the same token as [[client]] \"marker\"";
    assert!(said.starts_with(wants), "{said}");
    printed.push(said);
    for text in &printed {
        assert!(!text.contains(MARKER) && !text.contains(JOIN), "{text}");
        assert!(!text.contains(DEFAULT_TOKEN), "{text}");
    }

    let captured = Captured::start();
    let writes = served(config.clients).await;
    for via in Via::BOTH {
        let id = format!("logged-{via:?}").to_lowercase();
        submit_as(&writes, MARKER, via, &wire(&id))
            .await
            .expect("recorded");
        read_as(&writes, MARKER, via, &id).await;
    }
    let near = format!("{}X", &MARKER[..MARKER.len() - 1]);
    let request = SubmitJobRequest {
        spec: Some(wire("never")),
    };
    let body = to_json("SubmitJobRequest", &request);
    let refused = writes.http_as(&near, "POST", "/v1/jobs", Some(&body)).await;
    assert_eq!(refused, (StatusCode::UNAUTHORIZED, Vec::new()));
    let lines = captured.lines();
    let calls: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("job service call"))
        .collect();
    assert_eq!(calls.len(), 4, "{lines:#?}");
    for (line, (call, job)) in calls.iter().zip([
        ("SubmitJob", "logged-grpc"),
        ("GetJob", "logged-grpc"),
        ("SubmitJob", "logged-json"),
        ("GetJob", "logged-json"),
    ]) {
        for field in [call, "marker", job, "Ok"] {
            assert!(line.contains(field), "{field} in {line}");
        }
    }
    for line in &lines {
        assert!(!line.contains(MARKER) && !line.contains(&near), "{line}");
        assert!(
            !line.contains("never"),
            "a refused request was logged: {line}"
        );
    }
}
