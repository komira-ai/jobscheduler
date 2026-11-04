//! The job client, `js_client::jobs::JobClient`, against the in-memory job server of
//! `fake_jobs`: the rules it keeps from the lease client (each against `JobClient::connect`,
//! with the whole message), the bearer on every call, how a failure is classified by
//! whether the request was written, and a retried request on the wire.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_client::jobs::{Dialer, JobClient, JobError};
use js_proto::v1::{
    CancelJobRequest, CordonHostRequest, GetJobRequest, GetLogsRequest, GetResultRequest,
    GetUsageRequest, JobSpec, ListHostsRequest, ListJobsRequest, ListPoolsRequest,
    ListProvidersRequest, Needs, SubmitJobRequest, UncordonHostRequest,
};
use prost::Message as _;
use tonic::Code;

use crate::fake_jobs::{Answer, Fake, refusing, tls_server};
use crate::support::Scratch;

/// The limit of every call; nothing in these tests waits for it.
const LIMIT: Duration = Duration::from_secs(20);

/// The token of the tests.
const TOKEN: &str = "job-token-0123456789";

/// A loopback origin over plain http; the in-memory dialer never dials it.
const PLAIN: &str = "http://localhost:7070";

/// The owner-only token file of a test.
fn token_file(dir: &Scratch) -> PathBuf {
    dir.write("token", format!("{TOKEN}\n").as_bytes(), 0o600)
}

/// A client of `server` through `dialer`, with the test's token file.
async fn client(
    dir: &Scratch,
    server: &str,
    ca: Option<&Path>,
    dialer: Dialer,
) -> Result<JobClient, JobError> {
    JobClient::connect_via(server, &token_file(dir), ca, LIMIT, dialer).await
}

/// What a refused connect said.
fn refused(connected: Result<JobClient, JobError>) -> String {
    connected
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

/// A get of job `j-1`.
fn get() -> GetJobRequest {
    GetJobRequest {
        job_id: "j-1".to_owned(),
    }
}

/// Asserts that the same `UNAVAILABLE` is `NotSent` when the connection failed before the
/// request was written (the dialer refuses) and `MayHaveActed` when the server answered it
/// after reading the request. This is what proves the classification reads the record of
/// the write, not the status code alone.
#[tokio::test]
async fn unavailable_before_the_write_is_not_sent_and_after_it_may_have_acted() {
    let dir = Scratch::new("jobs_unavailable");
    let mut before = client(&dir, PLAIN, None, refusing()).await.unwrap();
    let error = before.get_job(get()).await.err().unwrap();
    assert_eq!(
        (error.to_string(), error.may_have_acted()),
        (
            "cannot reach http://localhost:7070: nothing listens".to_owned(),
            false
        )
    );

    let fake = Fake::new();
    fake.answer("GetJob", vec![Answer::Status(Code::Unavailable, "busy")]);
    let mut after = client(&dir, PLAIN, None, fake.dialer(None)).await.unwrap();
    let error = after.get_job(get()).await.err().unwrap();
    assert_eq!(
        (error.to_string(), error.may_have_acted()),
        (
            "no answer from http://localhost:7070 after the request was sent: busy".to_owned(),
            true
        )
    );
    assert_eq!(fake.seen().len(), 1);
}

/// Asserts that after `MayHaveActed`, a second `submit_job` of the same request puts the
/// same message bytes on the wire as the first: the client regenerates no field (no id of
/// its own) per attempt.
#[tokio::test]
async fn a_retried_submit_resends_identical_bytes() {
    let dir = Scratch::new("jobs_retry");
    let fake = Fake::new();
    fake.answer(
        "SubmitJob",
        vec![
            Answer::Status(Code::Unavailable, "lost"),
            Answer::Message(Vec::new()),
        ],
    );
    let mut jobs = client(&dir, PLAIN, None, fake.dialer(None)).await.unwrap();
    let request = SubmitJobRequest {
        spec: Some(JobSpec {
            job_id: "nightly-42".to_owned(),
            image: format!("registry.example/app@sha256:{}", "ab".repeat(32)),
            args: vec!["--fast".to_owned()],
            needs: Some(Needs {
                cpu_millis: 1500,
                ..Needs::default()
            }),
            wall_limit_seconds: Some(600),
            ..JobSpec::default()
        }),
    };
    let first = jobs.submit_job(request.clone()).await.err().unwrap();
    assert!(first.may_have_acted(), "{first}");
    jobs.submit_job(request.clone()).await.unwrap();
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].message, request.encode_to_vec());
    assert_eq!(seen[1].message, seen[0].message);
}

/// Asserts that plain http to a host off the loopback is refused before anything is
/// dialled, with the lease client's whole message.
#[tokio::test]
async fn job_client_refuses_http_off_loopback() {
    let dir = Scratch::new("jobs_http");
    let fake = Fake::new();
    let said = refused(client(&dir, "http://jobs.example:80", None, fake.dialer(None)).await);
    assert_eq!(
        said,
        "--server: \"http://jobs.example:80\" is plain http to a host off the loopback: a bearer token never crosses a network in clear, use https://"
    );
    let said =
        refused(JobClient::connect("http://jobs.example", &token_file(&dir), None, LIMIT).await);
    assert!(
        said.starts_with("--server: \"http://jobs.example\" is plain http"),
        "{said}"
    );
    assert_eq!(fake.dials(), 0);
}

/// Asserts that a token file others can read is refused with the whole message, naming the
/// file, its mode and its owner.
#[tokio::test]
async fn job_client_refuses_a_token_file_others_can_read() {
    let dir = Scratch::new("jobs_token_mode");
    let loose = dir.write("loose", TOKEN.as_bytes(), 0o644);
    let said = refused(JobClient::connect_via(PLAIN, &loose, None, LIMIT, refusing()).await);
    let uid = rustix::process::geteuid().as_raw();
    assert_eq!(
        said,
        format!(
            "--token-file: {} must be a regular file owned by you and readable only by you (mode 0600 or 0400), not mode 0644 owned by uid {uid}",
            loose.display()
        )
    );
}

/// Asserts that an owner-only token file that someone else owns is refused with the whole
/// message. Changing a file's owner needs root, so the test is skipped, saying why, when
/// it does not run as root.
#[tokio::test]
async fn job_client_refuses_a_token_file_owned_by_someone_else() {
    if !rustix::process::geteuid().is_root() {
        eprintln!(
            "skipped: giving the token file to another user needs root, and this test runs as a user"
        );
        return;
    }
    let dir = Scratch::new("jobs_token_owner");
    let theirs = dir.write("theirs", TOKEN.as_bytes(), 0o600);
    std::os::unix::fs::chown(&theirs, Some(65534), Some(65534)).unwrap();
    let said = refused(JobClient::connect_via(PLAIN, &theirs, None, LIMIT, refusing()).await);
    assert_eq!(
        said,
        format!(
            "--token-file: {} must be a regular file owned by you and readable only by you (mode 0600 or 0400), not mode 0600 owned by uid 65534",
            theirs.display()
        )
    );
}

/// Asserts that for `https://` the client trusts the `--ca-cert` file and only it: the
/// server's own certificate is trusted, another certificate is not, and without `--ca-cert`
/// (the bundled public roots) a private certificate is not either. The refusals come
/// before the request is written: the server reads one request in all.
#[tokio::test]
async fn job_client_trusts_only_the_given_ca_cert() {
    let dir = Scratch::new("jobs_tls");
    let (server, pem) = tls_server();
    let (_, other) = tls_server();
    let ours = dir.write("ours.pem", pem.as_bytes(), 0o644);
    let theirs = dir.write("theirs.pem", other.as_bytes(), 0o644);
    let fake = Fake::new();
    let origin = "https://jobs.example";
    let mut trusted = client(&dir, origin, Some(&ours), fake.dialer(Some(server.clone())))
        .await
        .unwrap();
    trusted.list_hosts(ListHostsRequest {}).await.unwrap();
    // Another certificate of the same name does not verify the server's signature; the
    // bundled roots do not know the server's issuer at all.
    let refusal =
        "cannot reach https://jobs.example: TLS with jobs.example: invalid peer certificate: ";
    for (ca, why) in [
        (Some(theirs.as_path()), "BadSignature"),
        (None, "UnknownIssuer"),
    ] {
        let mut jobs = client(&dir, origin, ca, fake.dialer(Some(server.clone())))
            .await
            .unwrap();
        let error = jobs.list_hosts(ListHostsRequest {}).await.err().unwrap();
        assert_eq!(
            (error.to_string(), error.may_have_acted()),
            (format!("{refusal}{why}"), false),
            "{ca:?}"
        );
    }
    assert_eq!(fake.seen().len(), 1);
    let missing = dir.path("missing.pem");
    let said = refused(client(&dir, origin, Some(&missing), fake.dialer(None)).await);
    assert!(
        said.starts_with(&format!("--ca-cert: {}: ", missing.display())),
        "{said}"
    );
}

/// Asserts that every call carries the bearer as `authorization` metadata, and that a call
/// of each kind reaches its own method of the service: the generated client has one method
/// per call of the definition.
#[tokio::test]
async fn job_client_sends_the_bearer_on_every_call() {
    let dir = Scratch::new("jobs_bearer");
    let fake = Fake::new();
    let mut jobs = client(&dir, PLAIN, None, fake.dialer(None)).await.unwrap();
    jobs.submit_job(SubmitJobRequest::default()).await.unwrap();
    jobs.get_job(get()).await.unwrap();
    jobs.list_jobs(ListJobsRequest::default()).await.unwrap();
    jobs.cancel_job(CancelJobRequest::default()).await.unwrap();
    jobs.get_result(GetResultRequest::default()).await.unwrap();
    jobs.get_logs(GetLogsRequest::default()).await.unwrap();
    jobs.list_hosts(ListHostsRequest {}).await.unwrap();
    jobs.cordon_host(CordonHostRequest::default())
        .await
        .unwrap();
    jobs.uncordon_host(UncordonHostRequest::default())
        .await
        .unwrap();
    jobs.list_pools(ListPoolsRequest {}).await.unwrap();
    jobs.list_providers(ListProvidersRequest {}).await.unwrap();
    jobs.get_usage(GetUsageRequest::default()).await.unwrap();
    let seen = fake.seen();
    let paths: Vec<&str> = seen
        .iter()
        .map(|seen| seen.path.trim_start_matches("/jobscheduler.v1.JobService/"))
        .collect();
    assert_eq!(
        paths,
        [
            "SubmitJob",
            "GetJob",
            "ListJobs",
            "CancelJob",
            "GetResult",
            "GetLogs",
            "ListHosts",
            "CordonHost",
            "UncordonHost",
            "ListPools",
            "ListProviders",
            "GetUsage",
        ]
    );
    let bearer = format!("Bearer {TOKEN}");
    assert!(
        seen.iter()
            .all(|seen| seen.authorization.as_deref() == Some(bearer.as_str()))
    );
}

/// Asserts the whole text of every kind of failure, by equality: a bad flag, a server not
/// reached, a refused token, a definite refusal and a lost answer. None holds the token;
/// nor does the `Debug` text of any.
#[tokio::test]
async fn job_client_error_text_never_holds_the_token() {
    let dir = Scratch::new("jobs_texts");
    let fake = Fake::new();
    fake.answer("GetJob", vec![Answer::Status(Code::Unauthenticated, TOKEN)]);
    fake.answer(
        "CancelJob",
        vec![Answer::Status(Code::NotFound, "no job j-1")],
    );
    fake.answer("GetLogs", vec![Answer::Status(Code::Internal, "lost")]);
    let mut jobs = client(&dir, PLAIN, None, fake.dialer(None)).await.unwrap();
    let mut unreached = client(&dir, PLAIN, None, refusing()).await.unwrap();
    let errors = [
        client(&dir, "ftp://jobs.example", None, refusing())
            .await
            .err()
            .unwrap(),
        unreached.get_job(get()).await.err().unwrap(),
        jobs.get_job(get()).await.err().unwrap(),
        jobs.cancel_job(CancelJobRequest::default())
            .await
            .err()
            .unwrap(),
        jobs.get_logs(GetLogsRequest::default())
            .await
            .err()
            .unwrap(),
    ];
    let texts: Vec<(String, String)> = errors
        .iter()
        .map(|error| (error.to_string(), format!("{error:?}")))
        .collect();
    assert_eq!(
        texts,
        [
            (
                "--server: \"ftp://jobs.example\" must start with https:// (or http:// for a loopback host)".to_owned(),
                "NotSent(Flag { flag: \"--server\", why: \"\\\"ftp://jobs.example\\\" must start with https:// (or http:// for a loopback host)\" })".to_owned(),
            ),
            (
                "cannot reach http://localhost:7070: nothing listens".to_owned(),
                "NotSent(Unreached(\"cannot reach http://localhost:7070: nothing listens\"))".to_owned(),
            ),
            (
                "the server refused the token (unauthenticated)".to_owned(),
                "NotSent(Token(\"unauthenticated\"))".to_owned(),
            ),
            (
                "no job j-1 (not-found)".to_owned(),
                "Rejected { code: NotFound, reason: \"no job j-1\" }".to_owned(),
            ),
            (
                "no answer from http://localhost:7070 after the request was sent: lost".to_owned(),
                "MayHaveActed(\"no answer from http://localhost:7070 after the request was sent: lost\")".to_owned(),
            ),
        ]
    );
}

/// Asserts that an HTTP redirect is not followed: the client reads one request's answer,
/// dials nothing else, and fails the call.
#[tokio::test]
async fn job_client_follows_no_redirect() {
    let dir = Scratch::new("jobs_redirect");
    let fake = Fake::new();
    fake.answer("GetJob", vec![Answer::Redirect]);
    let mut jobs = client(&dir, PLAIN, None, fake.dialer(None)).await.unwrap();
    let error = jobs.get_job(get()).await.err().unwrap();
    assert!(error.may_have_acted(), "{error}");
    assert!(
        error
            .to_string()
            .starts_with("no answer from http://localhost:7070 after the request was sent: "),
        "{error}"
    );
    assert_eq!((fake.seen().len(), fake.dials()), (1, 1));
}

/// Asserts that a request whose answer never comes (the server reads it and the connection
/// is dropped) may have acted.
#[tokio::test]
async fn a_dropped_connection_after_the_request_may_have_acted() {
    let dir = Scratch::new("jobs_dropped");
    let fake = Fake::new();
    fake.answer("SubmitJob", vec![Answer::Hang]);
    let mut jobs = client(&dir, PLAIN, None, fake.dialer(None)).await.unwrap();
    let error = jobs
        .submit_job(SubmitJobRequest::default())
        .await
        .err()
        .unwrap();
    assert!(error.may_have_acted(), "{error}");
    assert_eq!(fake.seen().len(), 1);
}

/// Asserts that the program's own dialer, TCP, reaches a server on the loopback through
/// `JobClient::connect` (the listener takes a port the system chooses).
#[tokio::test]
async fn the_tcp_dialer_reaches_a_loopback_server() {
    let dir = Scratch::new("jobs_tcp");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let fake = Fake::new();
    let incoming = tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(tonic::transport::Server::builder().serve_with_incoming(fake.clone(), incoming));
    let mut jobs = JobClient::connect(
        &format!("http://127.0.0.1:{port}"),
        &token_file(&dir),
        None,
        LIMIT,
    )
    .await
    .unwrap();
    jobs.list_pools(ListPoolsRequest {}).await.unwrap();
    assert_eq!(fake.seen().len(), 1);
}
