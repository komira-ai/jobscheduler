//! The client tokens read again at the tick: the server is started in process from flags, the
//! job service is put behind the clients handle of that configuration, and the tick is
//! stepped by hand (`InProcess::tick`), so a reload happens exactly where a test says. No
//! socket is bound and nothing sleeps.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use js_jobs::Retention;
use js_proto::v1::{GetJobRequest, GetJobResponse};
use js_state::ManualClock;
use tonic::Code;
use tracing::Level;

use super::access_tests::{Events, info};
use super::writes_rig::{Fixed, Writes, wire};
use crate::agent_api::job_rig::{OneClock, START_MS, tables_on};
use crate::clock::SharedClock;
use crate::config::LiveClients;
use crate::config::clients_rig::{ClientsRig, DEFAULT_TOKEN, JOIN};
use crate::in_process::InProcess;

/// The first token of `ci`.
const A: &str = "ci-token-A-0123456789-abcdefghijklmnopq";

/// The token `ci`'s file is rewritten with.
const B: &str = "ci-token-B-0123456789-abcdefghijklmnopq";

/// The token `ci`'s file holds after a bad edit is mended.
const C: &str = "ci-token-C-0123456789-abcdefghijklmnopq";

/// The token of `ops`, which no case rewrites.
const OPS: &str = "ops-token-0123456789-abcdefghijklmnopqrs";

/// A server started in process with the clients `ci` (its token in the file returned) and
/// `ops`, and the job service behind the same clients handle the server's tick reloads.
struct Reloading {
    /// The directory of the token files, kept until the test ends.
    _rig: ClientsRig,
    file: std::path::PathBuf,
    server: InProcess,
    writes: Writes,
}

impl Reloading {
    async fn new(name: &str) -> Reloading {
        let rig = ClientsRig::new(name);
        let file = rig.token_file("ci", A);
        let ops = rig.token_file("ops", OPS);
        let extra: Vec<OsString> = vec![
            "--client-token".into(),
            format!("ci={}", file.display()).into(),
            "--client-token".into(),
            format!("ops:operator={}", ops.display()).into(),
        ];
        let config = rig.config_of_flags(&extra).expect("the flags start");
        let clients: LiveClients = config.clients.clone();
        let clock = SharedClock::new(js_types::ManualClock::default());
        let server = InProcess::start(config, clock).expect("the server starts");
        let store_clock = Arc::new(ManualClock::default());
        store_clock.advance(Duration::from_millis(START_MS));
        let (_, jobs) = tables_on(Arc::clone(&store_clock), Retention::DEFAULT);
        let shared = SharedClock::new(OneClock(store_clock));
        let writes = Writes::with_clients(Arc::new(jobs), shared, Fixed::new(true), clients).await;
        Reloading {
            _rig: rig,
            file,
            server,
            writes,
        }
    }

    /// Whether `token` is admitted, over gRPC and over the gateway, which must agree: a read of
    /// a job that does not exist is `NOT_FOUND` (404) past the bearer layer and
    /// `UNAUTHENTICATED` (401) before it.
    async fn admits(&self, token: &str) -> bool {
        let request = GetJobRequest {
            job_id: "no-such-job".to_owned(),
        };
        let grpc = self
            .writes
            .grpc_as::<_, GetJobResponse>(token, "GetJob", request)
            .await
            .expect_err("no such job");
        let (http, _) = self
            .writes
            .http_as(token, "GET", "/v1/jobs/no-such-job", None)
            .await;
        match (grpc.code(), http) {
            (Code::NotFound, StatusCode::NOT_FOUND) => true,
            (Code::Unauthenticated, StatusCode::UNAUTHORIZED) => false,
            other => panic!("the transports disagree on {token:?}: {other:?}"),
        }
    }
}

/// A bad edit of a token file.
type Edit = Box<dyn Fn(&Path)>;

/// Writes `text` to `path`, owner-only.
fn rewrite(path: &Path, text: &str) {
    std::fs::write(path, text).expect("the file is written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("the mode");
}

/// Asserts that rewriting `ci`'s token file from A to B changes nothing until the tick, and
/// that after one tick A is refused (401, `UNAUTHENTICATED`) and B admitted as `ci` (the job it
/// submits is recorded as `ci`'s), with `ops` untouched; that the tick that changed the
/// tokens logs one info line saying so, and a tick that changed nothing logs none. Catches a
/// bearer layer that holds a copy of the set taken at start (both tokens would stay as they
/// were), a reload that never runs on the tick, and a reload that renames the client.
#[tokio::test]
async fn a_tick_revokes_a_rewritten_token_and_admits_the_new_one() {
    let (events, _scope) = Events::scoped();
    let s = Reloading::new("reload_rotates").await;
    assert!(s.admits(A).await && s.admits(OPS).await && !s.admits(B).await);
    s.server.tick().await;
    let quiet = events.take();
    let said = |events: &[super::access_tests::Event]| {
        info(events)
            .iter()
            .filter(|event| event.fields["message"].contains("client tokens changed"))
            .count()
    };
    assert_eq!(said(&quiet), 0, "a tick that changed nothing said it did");

    rewrite(&s.file, B);
    assert!(
        s.admits(A).await && !s.admits(B).await,
        "a reload before the tick"
    );
    s.server.tick().await;
    assert_eq!(said(&events.take()), 1);
    assert!(
        !s.admits(A).await,
        "the old token is still admitted after the tick"
    );
    assert!(s.admits(B).await, "the new token is refused after the tick");
    assert!(s.admits(OPS).await, "an untouched client lost its token");
    let job = s
        .writes
        .grpc_as::<_, js_proto::v1::SubmitJobResponse>(
            B,
            "SubmitJob",
            js_proto::v1::SubmitJobRequest {
                spec: Some(wire("after-rotation")),
            },
        )
        .await
        .expect("recorded")
        .job
        .expect("an answer carries the job");
    assert_eq!(job.client.as_deref(), Some("ci"));
}

/// Asserts that each bad edit of `ci`'s token file keeps every client's token in force over
/// the tick (A and `ops`'s are still admitted, the bad file's token is not) and logs exactly
/// one warning, whose fields are exactly the fixed message and the reason naming the entry
/// and the file (so it holds no token): a token too short, an empty file, a
/// file removed, one others may read (mode 0644), `ops`'s token, the lease routes' token
/// (`default`'s) and the agent side's join token; and that once the file holds a valid new
/// token C, the next tick puts it in force. Catches a reload that empties the set (every
/// client locked out), one that drops only the bad client, one that lets two clients share
/// a token or a client hold the join token, a silent refusal, and a token in the warning.
#[tokio::test]
async fn a_bad_token_file_keeps_every_client_token_in_force() {
    let (events, _scope) = Events::scoped();
    let s = Reloading::new("reload_keeps").await;
    let at = s.file.display().to_string();
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&s.file).expect("meta"));
    let ci = "--client-token ci";
    let own = "each client needs its own token";
    let edits: [(&str, Edit, String); 7] = [
        (
            "too short",
            Box::new(|path| rewrite(path, "short")),
            format!(
                "{ci}: {at} holds a token of 5 bytes; a token must have at least 32 (use a random one)"
            ),
        ),
        (
            "empty",
            Box::new(|path| rewrite(path, "")),
            format!("{ci}: {at} is empty"),
        ),
        (
            "removed",
            Box::new(|path| std::fs::remove_file(path).expect("removed")),
            format!("{ci}: {at}: No such file or directory (os error 2)"),
        ),
        (
            "readable by others",
            Box::new(|path| {
                rewrite(path, C);
                let mode = std::fs::Permissions::from_mode(0o644);
                std::fs::set_permissions(path, mode).expect("the mode");
            }),
            format!(
                "{ci}: {at} must be a regular file owned by the user running the server and \
                 readable only by that user (mode 0600 or 0400), not mode 0644 owned by uid {uid}"
            ),
        ),
        (
            "ops's token",
            Box::new(|path| rewrite(path, OPS)),
            format!("--client-token ops: holds the same token as {ci}; {own}"),
        ),
        (
            "default's token",
            Box::new(|path| rewrite(path, DEFAULT_TOKEN)),
            format!("{ci}: holds the same token as the client of --token-file; {own}"),
        ),
        (
            "the join token",
            Box::new(|path| rewrite(path, JOIN)),
            format!("{ci}: holds a join token of --agent-token-file: the two must differ"),
        ),
    ];
    for (case, edit, wants) in edits {
        edit(&s.file);
        events.take();
        s.server.tick().await;
        let taken = events.take();
        let warnings: Vec<_> = taken
            .iter()
            .filter(|event| event.level == Level::WARN)
            .collect();
        assert_eq!(warnings.len(), 1, "{case}: {taken:#?}");
        let wants = BTreeMap::from([
            (
                "message".to_owned(),
                "cannot read the client token files again; the client tokens in force stay"
                    .to_owned(),
            ),
            ("error".to_owned(), wants),
        ]);
        assert_eq!(warnings[0].fields, wants, "{case}");
        assert!(s.admits(A).await, "{case}: the token in force was dropped");
        assert!(s.admits(OPS).await, "{case}: another client was dropped");
        assert!(
            !s.admits(C).await,
            "{case}: a refused file's token is in force"
        );
    }
    rewrite(&s.file, C);
    s.server.tick().await;
    assert!(
        !s.admits(A).await && s.admits(C).await,
        "a mended file is not read"
    );
}
