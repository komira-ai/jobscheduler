//! `deploy` against real HTTPS: a local server with its own CA that plays the runner's
//! token service and the coordinator, records every request, and answers each as a script
//! says; time is a virtual clock, so no test waits or asserts on elapsed time. And the
//! `cargo xtask deploy` binary for its flags and exit codes.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use xtask::deploy::{self, Clock, Error, Plan, Roots, TokenService};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const VERSION: &str = "0.2.3";

/// One request the server saw.
#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    authorization: String,
    body: String,
}

/// What a call of the coordinator is, for the script.
enum Call<'a> {
    /// `GET /v1/convergence?service=`: `posted` once this service's desired was set, and
    /// `n` the reads answered since (0 for the baseline).
    Read {
        service: &'a str,
        posted: bool,
        n: usize,
    },
    /// `POST /v1/desired` for `service`, the `attempt`th time (from 1).
    Post { service: &'a str, attempt: usize },
}

/// A canned HTTP/1.1 response. The body `"not json"` (a JSON string with that text) is sent
/// as the bare text `not json`, which is not JSON.
fn response(status: u16, body: &Value) -> Vec<u8> {
    let body = if body == &json!("not json") {
        "not json".to_owned()
    } else {
        body.to_string()
    };
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The request in `stream`, or `None` when the client went away first.
fn read_request(stream: impl Read) -> Option<Req> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let mut parts = first.split(' ');
    let (method, path) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let (mut length, mut authorization) = (0, String::new());
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().ok()?,
                "authorization" => authorization = value.trim().to_owned(),
                _ => {}
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Req {
        method,
        path,
        authorization,
        body: String::from_utf8(body).ok()?,
    })
}

/// The farm: its base URL, the CA's certificate and the requests seen so far.
struct Farm {
    base: String,
    ca: CertificateDer<'static>,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Req>>>,
}

impl Farm {
    /// Serves forever on a loopback port over TLS. `/token` answers `tok-1`, `tok-2`, ...;
    /// the coordinator's paths answer as `script` says (status and JSON).
    fn start(script: impl Fn(Call<'_>) -> (u16, Value) + Send + 'static) -> Farm {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca =
            CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("server config");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!(
            "https://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            let (mut tokens, mut reads, mut attempts) = (0, HashMap::new(), HashMap::new());
            let mut posted: HashMap<String, ()> = HashMap::new();
            for stream in listener.incoming() {
                let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
                let mut tls = rustls::StreamOwned::new(conn, stream.expect("accept"));
                let Some(req) = read_request(&mut tls) else {
                    continue;
                };
                let answer = if let Some(status) = req.path.strip_prefix("/token-status-") {
                    let digits = status.split('?').next().unwrap_or("");
                    response(digits.parse().unwrap_or(500), &json!({}))
                } else if req.path.starts_with("/token-empty") {
                    response(200, &json!({"value": ""}))
                } else if req.path.starts_with("/token-garbage") {
                    b"HTTP/1.1 200 X\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope".to_vec()
                } else if req.path.starts_with("/token") {
                    tokens += 1;
                    response(200, &json!({"value": format!("tok-{tokens}")}))
                } else if let Some(service) = req.path.strip_prefix("/v1/convergence?service=") {
                    let n = reads.entry(service.to_owned()).or_insert(0);
                    let (status, body) = script(Call::Read {
                        service,
                        posted: posted.contains_key(service),
                        n: *n,
                    });
                    if posted.contains_key(service) {
                        *n += 1;
                    }
                    response(status, &body)
                } else if req.path == "/v1/desired" {
                    let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
                    let service = body["service"].as_str().unwrap_or("").to_owned();
                    let attempt = attempts.entry(service.clone()).or_insert(0);
                    *attempt += 1;
                    let (status, body) = script(Call::Post {
                        service: &service,
                        attempt: *attempt,
                    });
                    if status == 200 {
                        posted.insert(service, ());
                    }
                    response(status, &body)
                } else {
                    response(404, &json!({}))
                };
                log.lock().expect("log").push(req);
                let _ = tls.write_all(&answer);
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        Farm {
            base,
            ca: ca.der().clone(),
            ca_pem: ca.pem(),
            seen,
        }
    }

    /// The requests the coordinator's paths saw, in order.
    fn coordinator_calls(&self) -> Vec<Req> {
        let all = self.seen.lock().expect("log").clone();
        all.into_iter()
            .filter(|r| r.path.starts_with("/v1/"))
            .collect()
    }

    fn token_service(&self) -> TokenService {
        TokenService {
            url: format!("{}/token", self.base),
            key: "runner-key".to_owned(),
        }
    }

    fn plan(&self, services: &[&str]) -> Plan {
        Plan {
            url: self.base.clone(),
            audience: self.base.clone(),
            version: VERSION.to_owned(),
            services: services.iter().map(|s| (*s).to_owned()).collect(),
            deadline: Duration::from_secs(3600),
            stall: Duration::from_secs(1200),
            every: Duration::from_secs(30),
        }
    }

    /// Runs the deploy of `plan` on a virtual clock: the result, the log and the sleeps.
    fn deploy(&self, plan: &Plan) -> (Result<(), Error>, String, Vec<Duration>) {
        let ca = std::slice::from_ref(&self.ca);
        let roots = Roots {
            runner: ca,
            coordinator: ca,
        };
        let now = Cell::new(Duration::ZERO);
        let slept = std::cell::RefCell::new(Vec::new());
        let clock = Clock {
            now: Box::new(|| now.get()),
            sleep: Box::new(|d| {
                now.set(now.get() + d);
                slept.borrow_mut().push(d);
            }),
        };
        let mut out = Vec::new();
        let result = deploy::deploy_on(plan, &self.token_service(), &roots, &clock, &mut out);
        drop(clock);
        let log = String::from_utf8(out).expect("utf-8");
        (result, log, slept.into_inner())
    }
}

/// What `GET /v1/convergence` answers: one role row `(role, at_desired, updating, failed, down)`.
fn conv(state: &str, desired: &str, role: (&str, u64, u64, u64, u64)) -> (u16, Value) {
    let (role, at, updating, failed, down) = role;
    (
        200,
        json!({"state": state, "desired": desired, "quiet": false, "roles": [
            {"role": role, "at_desired": at, "updating": updating, "failed": failed, "down": down}
        ]}),
    )
}

/// A farm that is at `0.1.0` until a service's desired is set and then answers `after`.
fn farm_with(after: impl Fn(&str, usize) -> (u16, Value) + Send + 'static) -> Farm {
    Farm::start(move |call| match call {
        Call::Read {
            service,
            posted: false,
            ..
        } => conv("converged", "0.1.0", (role_of(service), 1, 0, 0, 0)),
        Call::Read { service, n, .. } => after(service, n),
        Call::Post { .. } => (200, json!({})),
    })
}

fn role_of(service: &str) -> &'static str {
    if service == "js-server" {
        "server"
    } else {
        "agent"
    }
}

/// Asserts the requests of a deploy of both services: for each, in this order, the baseline
/// read, the POST with exactly `{"service", "version", "min_up"}`, and the poll that ends
/// it, all with `service` in the query or body, each with its own fresh token, and the
/// token requests naming the audience. js-agent is not touched before js-server converged.
#[test]
fn the_requests_set_each_service_in_order_with_a_fresh_token() {
    let farm = farm_with(|service, _| conv("converged", VERSION, (role_of(service), 1, 0, 0, 0)));
    let (result, log, _) = farm.deploy(&farm.plan(&["js-server", "js-agent"]));
    result.expect("deploy");
    let calls: Vec<(String, String, Option<Value>)> = farm
        .coordinator_calls()
        .into_iter()
        .map(|r| (r.method, r.path, serde_json::from_str(&r.body).ok()))
        .collect();
    let post = |service: &str| {
        (
            "POST".to_owned(),
            "/v1/desired".to_owned(),
            Some(
                json!({"service": service, "version": VERSION, "min_up": {"storage": 0, "frontend": 0}}),
            ),
        )
    };
    let read = |service: &str| {
        (
            "GET".to_owned(),
            format!("/v1/convergence?service={service}"),
            None,
        )
    };
    assert_eq!(
        calls,
        vec![
            read("js-server"),
            post("js-server"),
            read("js-server"),
            read("js-agent"),
            post("js-agent"),
            read("js-agent"),
        ]
    );
    let seen = farm.coordinator_calls();
    for (i, req) in seen.iter().enumerate() {
        assert_eq!(
            req.authorization,
            format!("Bearer tok-{}", i + 1),
            "{req:?}"
        );
    }
    let tokens: Vec<Req> = farm
        .seen
        .lock()
        .expect("log")
        .iter()
        .filter(|r| r.path.starts_with("/token"))
        .cloned()
        .collect();
    assert_eq!(tokens.len(), 6);
    let audience = format!(
        "audience=https%3A%2F%2F{}",
        farm.base.trim_start_matches("https://").replace(':', "%3A")
    );
    assert!(
        tokens
            .iter()
            .all(|t| t.path.ends_with(&audience) && t.authorization == "Bearer runner-key"),
        "{tokens:?}"
    );
    // Every token is masked in the log before it could be printed.
    for n in 1..=6 {
        assert!(log.contains(&format!("::add-mask::tok-{n}\n")), "{log}");
    }
}

/// Asserts that a service that does not converge stops the deploy: js-agent is never read or
/// set while js-server is blocked, and the error names js-server.
#[test]
fn agents_are_not_touched_while_the_server_is_blocked() {
    let farm = farm_with(|service, _| conv("blocked", VERSION, (role_of(service), 0, 0, 1, 0)));
    let (result, _, _) = farm.deploy(&farm.plan(&["js-server", "js-agent"]));
    assert_eq!(result, Err(Error::Blocked("js-server".to_owned())));
    assert!(
        farm.coordinator_calls()
            .iter()
            .all(|r| !r.path.contains("js-agent") && !r.body.contains("js-agent"))
    );
}

/// Asserts that the wait polls until the coordinator says converged: two in-progress reads
/// with a host updating, a sleep of the interval after each, then success.
#[test]
fn the_wait_polls_until_converged() {
    let farm = farm_with(|service, n| {
        let role = role_of(service);
        if n < 2 {
            conv("in_progress", VERSION, (role, 0, 1, 0, 0))
        } else {
            conv("converged", VERSION, (role, 1, 0, 0, 0))
        }
    });
    let (result, log, slept) = farm.deploy(&farm.plan(&["js-server"]));
    result.expect("deploy");
    assert_eq!(slept, vec![Duration::from_secs(30); 2]);
    assert!(
        log.contains("baseline: hosts live before the deploy: server 1"),
        "{log}"
    );
    assert_eq!(farm.coordinator_calls().len(), 5);
}

/// Asserts that a converged answer with fewer hosts at the version than were live before is
/// a failure: a host the release crashed is not hidden by the coordinator's own answer.
#[test]
fn a_host_lost_during_the_deploy_fails_it() {
    let farm = Farm::start(|call| match call {
        Call::Read { posted: false, .. } => conv("converged", "0.1.0", ("agent", 2, 0, 0, 0)),
        Call::Read { .. } => conv("converged", VERSION, ("agent", 1, 0, 0, 1)),
        Call::Post { .. } => (200, json!({})),
    });
    let (result, _, _) = farm.deploy(&farm.plan(&["js-agent"]));
    assert_eq!(
        result,
        Err(Error::Lost {
            service: "js-agent".to_owned(),
            role: "agent".to_owned(),
            expected: 2,
            reached: 1,
            version: VERSION.to_owned(),
        })
    );
}

/// Asserts that a desired version that is no longer this release's (another writer) fails
/// the deploy even though the coordinator says converged.
#[test]
fn a_replaced_desired_version_fails_the_deploy() {
    let farm = farm_with(|service, _| conv("converged", "0.9.9", (role_of(service), 1, 0, 0, 0)));
    let (result, _, _) = farm.deploy(&farm.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Superseded {
            service: "js-server".to_owned(),
            desired: "0.9.9".to_owned()
        })
    );
}

/// Asserts that counts that never change end the wait as stalled, and that a farm whose
/// counts keep changing without converging ends it at the deadline.
#[test]
fn a_stalled_wait_and_a_wait_past_the_deadline_fail() {
    let stuck =
        farm_with(|service, _| conv("in_progress", VERSION, (role_of(service), 0, 1, 0, 0)));
    let (result, _, slept) = stuck.deploy(&stuck.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Stalled {
            service: "js-server".to_owned(),
            seconds: 1200
        })
    );
    assert_eq!(slept.len(), 40);

    let moving = farm_with(|service, n| {
        conv(
            "in_progress",
            VERSION,
            (role_of(service), n as u64, 1, 0, 0),
        )
    });
    let (result, _, _) = moving.deploy(&moving.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Deadline {
            service: "js-server".to_owned(),
            seconds: 3600
        })
    );
}

/// Asserts that a 403 (the allow-list does not name the service for this repository) is
/// final, not retried, and names the service; and that a 503 on the POST is retried and
/// then succeeds.
#[test]
fn a_refusal_is_final_and_a_503_is_retried() {
    let refused = Farm::start(|call| match call {
        Call::Read { .. } => conv("converged", "0.1.0", ("server", 1, 0, 0, 0)),
        Call::Post { .. } => (403, json!({})),
    });
    let (result, _, slept) = refused.deploy(&refused.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Refused {
            service: "js-server".to_owned(),
            what: "set the desired version",
            status: 403
        })
    );
    assert!(slept.is_empty());

    let flaky = Farm::start(|call| match call {
        Call::Read { posted: false, .. } => conv("converged", "0.1.0", ("server", 1, 0, 0, 0)),
        Call::Read { .. } => conv("converged", VERSION, ("server", 1, 0, 0, 0)),
        Call::Post {
            service: "js-server",
            attempt,
        } if attempt < 3 => (503, json!({})),
        Call::Post { .. } => (200, json!({})),
    });
    let (result, _, slept) = flaky.deploy(&flaky.plan(&["js-server"]));
    result.expect("deploy");
    assert_eq!(slept.len(), 2);
}

/// Asserts that a coordinator whose certificate the roots do not trust is a failure, and
/// that nothing was set.
#[test]
fn an_untrusted_coordinator_is_refused() {
    let farm = farm_with(|service, _| conv("converged", VERSION, (role_of(service), 1, 0, 0, 0)));
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: &[],
    };
    let clock = Clock {
        now: Box::new(|| Duration::ZERO),
        sleep: Box::new(|_| {}),
    };
    let result = deploy::deploy_on(
        &farm.plan(&["js-server"]),
        &farm.token_service(),
        &roots,
        &clock,
        &mut Vec::new(),
    );
    assert!(matches!(result, Err(Error::Call { .. })), "{result:?}");
    assert!(farm.coordinator_calls().iter().all(|r| r.method != "POST"));
}

/// Asserts that only `X.Y.Z` is a version text: the tag form, a commit suffix, a
/// pre-release, a missing or extra part, leading zeros, spaces and the empty text are refused.
#[test]
fn only_a_plain_release_version_is_accepted() {
    for ok in ["0.1.0", "10.20.30", "0.0.0"] {
        assert_eq!(deploy::check_version(ok), Ok(()), "{ok}");
    }
    for bad in [
        "v0.1.0",
        "0.1",
        "0.1.0.0",
        "0.1.0+g0123456789ab",
        "0.1.0-rc.1",
        "01.2.3",
        "1.2.03",
        " 1.2.3",
        "1.2.3\n",
        "",
        "a.b.c",
        "1..3",
    ] {
        assert_eq!(
            deploy::check_version(bad),
            Err(Error::Version(bad.to_owned())),
            "{bad:?}"
        );
    }
}

/// The plan of the request bodies and paths, by themselves.
#[test]
fn the_body_and_the_path_name_the_service() {
    assert_eq!(
        deploy::desired_body("js-agent", "1.2.3"),
        json!({"service": "js-agent", "version": "1.2.3", "min_up": {"storage": 0, "frontend": 0}})
    );
    assert_eq!(
        deploy::convergence_path("js-agent"),
        "/v1/convergence?service=js-agent"
    );
}

/// `cargo xtask deploy` with `args`: exit code, stdout and stderr.
fn xtask_deploy(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .arg("deploy")
        .args(args)
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .output()
        .expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Asserts that with an empty `--url` the verb prints the notice and passes, so a
/// repository without a coordinator is not failed.
#[test]
fn with_no_coordinator_the_verb_is_off_and_passes() {
    let (code, out, err) = xtask_deploy(&[
        "--url",
        "",
        "--version",
        VERSION,
        "--service",
        "js-server",
        "--service",
        "js-agent",
    ]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "::notice::the deploy step is off: the repository variable JS_COORDINATOR_URL is not set\n"
    );
}

/// Asserts that a bad version, service name or repeated service fails with exit 1 before
/// anything is sent, even with no coordinator set (the address is unreachable), and that
/// no `--service` is a usage error (exit 2).
#[test]
fn the_verb_refuses_bad_arguments_before_sending_anything() {
    for (url, version, services, says) in [
        (
            "https://127.0.0.1:9",
            "v0.2.3",
            &["js-server"][..],
            "not the form the services report",
        ),
        (
            "",
            "0.2.3+g0123456789ab",
            &["js-server"][..],
            "not the form the services report",
        ),
        (
            "https://127.0.0.1:9",
            VERSION,
            &["Js_Server"][..],
            "not a service name",
        ),
        (
            "https://127.0.0.1:9",
            VERSION,
            &["js-agent", "js-agent"][..],
            "given twice",
        ),
    ] {
        let mut args = vec!["--url", url, "--version", version];
        for s in services {
            args.extend(["--service", s]);
        }
        let (code, _, err) = xtask_deploy(&args);
        assert_eq!(code, Some(1), "{version} {services:?}: {err}");
        assert!(err.contains(says), "{err}");
    }
    let (code, _, err) = xtask_deploy(&["--url", "", "--version", VERSION]);
    assert_eq!(code, Some(2), "{err}");
}

/// Asserts that a job without an OIDC token service fails and says what the job needs.
#[test]
fn a_job_without_a_token_service_fails() {
    let (code, _, err) = xtask_deploy(&[
        "--url",
        "https://127.0.0.1:9",
        "--version",
        VERSION,
        "--service",
        "js-server",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("id-token: write"), "{err}");
}

/// A token service at `path` of the farm.
fn token_at(farm: &Farm, path: &str) -> TokenService {
    TokenService {
        url: format!("{}{path}", farm.base),
        key: "runner-key".to_owned(),
    }
}

/// Runs the deploy of js-server with `token` as the runner's token service and `roots`.
fn deploy_with_token(
    farm: &Farm,
    token: &TokenService,
    roots: &Roots<'_>,
) -> (Result<(), Error>, Vec<Req>) {
    let clock = Clock {
        now: Box::new(|| Duration::ZERO),
        sleep: Box::new(|_| {}),
    };
    let result = deploy::deploy_on(
        &farm.plan(&["js-server"]),
        token,
        roots,
        &clock,
        &mut Vec::new(),
    );
    (result, farm.coordinator_calls())
}

fn converged_farm() -> Farm {
    farm_with(|service, _| conv("converged", VERSION, (role_of(service), 1, 0, 0, 0)))
}

/// Asserts that a token service that answers anything but a token fails the deploy before
/// the coordinator is called, naming the token request: a status other than 200, an answer
/// that is not JSON, and an empty token.
#[test]
fn a_token_service_that_gives_no_token_fails_the_deploy() {
    let farm = converged_farm();
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: ca,
    };
    for (path, says) in [
        ("/token-status-500", "status 500"),
        ("/token-garbage", "expected"),
        ("/token-empty", "the answer has no token"),
    ] {
        let (result, calls) = deploy_with_token(&farm, &token_at(&farm, path), &roots);
        match result {
            Err(Error::Call { what, why, .. }) => {
                assert_eq!(what, "ask the runner for an OIDC token", "{path}");
                assert!(why.contains(says), "{path}: {why}");
            }
            other => panic!("{path}: {other:?}"),
        }
        assert!(calls.is_empty(), "{path}: {calls:?}");
    }
}

/// Asserts that a token service address that already has a query gets the audience added
/// with `&`, and that a runner whose certificate the roots do not trust fails the deploy.
#[test]
fn the_audience_joins_an_existing_query_and_an_untrusted_runner_fails() {
    let farm = converged_farm();
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: ca,
    };
    let (result, _) = deploy_with_token(&farm, &token_at(&farm, "/token?api=1"), &roots);
    result.expect("deploy");
    let tokens: Vec<String> = farm
        .seen
        .lock()
        .expect("log")
        .iter()
        .filter(|r| r.path.starts_with("/token"))
        .map(|r| r.path.clone())
        .collect();
    assert!(
        tokens
            .iter()
            .all(|p| p.starts_with("/token?api=1&audience=https%3A")),
        "{tokens:?}"
    );

    let untrusted = Roots {
        runner: &[],
        coordinator: ca,
    };
    let (result, calls) = deploy_with_token(&farm, &farm.token_service(), &untrusted);
    assert!(
        matches!(&result, Err(Error::Call { what, .. }) if *what == "ask the runner for an OIDC token"),
        "{result:?}"
    );
    assert_eq!(calls.len(), 3, "{calls:?}");
}

/// Asserts that a URL that is not `https://host[:port]` is refused before any call: plain
/// http, and a path after the host.
#[test]
fn a_url_that_is_not_https_host_port_is_refused() {
    let farm = converged_farm();
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: ca,
    };
    for url in ["http://127.0.0.1:9".to_owned(), format!("{}/v1", farm.base)] {
        let mut plan = farm.plan(&["js-server"]);
        plan.url = url.clone();
        let clock = Clock {
            now: Box::new(|| Duration::ZERO),
            sleep: Box::new(|_| {}),
        };
        let result = deploy::deploy_on(
            &plan,
            &farm.token_service(),
            &roots,
            &clock,
            &mut Vec::new(),
        );
        assert_eq!(result, Err(Error::Url(url)));
    }
    assert!(farm.seen.lock().expect("log").is_empty());
}

/// Asserts that a refusal while polling ends the deploy (it is not read again), while any
/// other failed read is reported and tried again until the farm converges.
#[test]
fn a_refused_poll_is_final_and_a_failed_poll_is_tried_again() {
    let refused = Farm::start(|call| match call {
        Call::Read { posted: false, .. } => conv("converged", "0.1.0", ("server", 1, 0, 0, 0)),
        _ => (401, json!({})),
    });
    let (result, _, slept) = refused.deploy(&refused.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Refused {
            service: "js-server".to_owned(),
            what: "set the desired version",
            status: 401
        })
    );
    assert!(slept.is_empty());

    let refused_poll = Farm::start(|call| match call {
        Call::Read { posted: true, .. } => (403, json!({})),
        Call::Read { .. } => conv("converged", "0.1.0", ("server", 1, 0, 0, 0)),
        Call::Post { .. } => (200, json!({})),
    });
    let (result, _, slept) = refused_poll.deploy(&refused_poll.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Refused {
            service: "js-server".to_owned(),
            what: "read the convergence",
            status: 403
        })
    );
    assert!(slept.is_empty());

    let flaky = farm_with(|service, n| {
        if n == 0 {
            (500, json!({}))
        } else {
            conv("converged", VERSION, (role_of(service), 1, 0, 0, 0))
        }
    });
    let (result, log, slept) = flaky.deploy(&flaky.plan(&["js-server"]));
    result.expect("deploy");
    assert_eq!(slept.len(), 1);
    assert!(
        log.contains("the read failed, trying again: js-server: read the convergence: status 500"),
        "{log}"
    );
}

/// Asserts that a 503 on the baseline read or the POST is retried four calls in all and then
/// fails with the 503.
#[test]
fn a_503_that_does_not_pass_fails_after_four_calls() {
    let posting = Farm::start(|call| match call {
        Call::Read { .. } => conv("converged", "0.1.0", ("server", 1, 0, 0, 0)),
        Call::Post { .. } => (503, json!({})),
    });
    let (result, _, slept) = posting.deploy(&posting.plan(&["js-server"]));
    assert_eq!(
        result,
        Err(Error::Call {
            service: "js-server".to_owned(),
            what: "set the desired version",
            why: "status 503".to_owned()
        })
    );
    assert_eq!(slept.len(), 3);

    let reading = Farm::start(|_| (503, json!({})));
    let (result, _, slept) = reading.deploy(&reading.plan(&["js-server"]));
    assert!(
        matches!(&result, Err(Error::Call { what, .. }) if *what == "read the service before the deploy"),
        "{result:?}"
    );
    assert_eq!(slept.len(), 3);
}

/// Asserts that an answer over the size limit fails the call (it is not read whole), naming
/// the call.
#[test]
fn an_answer_over_the_size_limit_fails_the_call() {
    let farm = Farm::start(|_| (200, json!({"pad": "x".repeat(1_100_000)})));
    let (result, _, _) = farm.deploy(&farm.plan(&["js-server"]));
    assert!(
        matches!(&result, Err(Error::Call { what, .. }) if *what == "read the service before the deploy"),
        "{result:?}"
    );
}

/// Asserts what an answer must hold: an answer with no roles field, no state, or a role row
/// without its name or a count is one this client cannot read and names the missing part;
/// a baseline with no live hosts is logged as `none` and the deploy goes on; a coordinator
/// that has just started says so in the table.
#[test]
fn an_answer_missing_a_part_is_refused_and_an_empty_baseline_is_none() {
    for (answer, missing) in [
        (
            json!({"state": "converged", "desired": "0.1.0"}),
            "no roles",
        ),
        (json!({"desired": "0.1.0", "roles": []}), "no state"),
        (
            json!({"state": "converged", "roles": [{"at_desired": 1}]}),
            "no role",
        ),
        (
            json!({"state": "converged", "roles": [{"role": "server"}]}),
            "no at_desired",
        ),
    ] {
        let farm = Farm::start(move |_| (200, answer.clone()));
        let (result, _, _) = farm.deploy(&farm.plan(&["js-server"]));
        assert_eq!(result, Err(Error::Answer(missing.to_owned())));
    }

    let farm = Farm::start(|call| match call {
        Call::Read { posted: false, .. } => (
            200,
            json!({"state": "converged", "desired": "0.1.0", "roles": []}),
        ),
        Call::Read { .. } => (
            200,
            json!({"state": "converged", "desired": VERSION, "quiet": true, "roles": []}),
        ),
        Call::Post { .. } => (200, json!({})),
    });
    let (result, log, _) = farm.deploy(&farm.plan(&["js-server"]));
    result.expect("deploy");
    assert!(
        log.contains("baseline: hosts live before the deploy: none"),
        "{log}"
    );
    assert!(
        log.contains("farm: converged, desired 0.2.3 (coordinator just started)"),
        "{log}"
    );
}

/// A sink that fails once what it is given contains the text.
struct FailAt(&'static str);

impl Write for FailAt {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if String::from_utf8_lossy(buf).contains(self.0) {
            Err(std::io::Error::other("sink closed"))
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that a log that cannot be written ends the deploy with the write error, whether
/// it fails on a log line or on the line that masks a token.
#[test]
fn a_log_that_cannot_be_written_fails_the_deploy() {
    let farm = converged_farm();
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: ca,
    };
    let clock = Clock {
        now: Box::new(|| Duration::ZERO),
        sleep: Box::new(|_| {}),
    };
    for at in ["service js-server", "::add-mask::"] {
        let result = deploy::deploy_on(
            &farm.plan(&["js-server"]),
            &farm.token_service(),
            &roots,
            &clock,
            &mut FailAt(at),
        );
        assert_eq!(result, Err(Error::Output("sink closed".to_owned())), "{at}");
    }
}

/// The flags of a deploy of `services` to `farm`.
fn args_for(farm: &Farm, ca_pem: &str, services: &[&str]) -> deploy::Args {
    deploy::Args {
        url: format!("{}/", farm.base),
        version: VERSION.to_owned(),
        services: services.iter().map(|s| (*s).to_owned()).collect(),
        ca_pem: ca_pem.to_owned(),
        audience: None,
        deadline_min: 60,
        stall_min: 20,
        every_secs: 30,
    }
}

/// Asserts the verb end to end in this process: the system roots (here, the farm's CA)
/// trust the coordinator when `--ca-pem` is empty, `--ca-pem` is used when given, and the
/// line it returns names the services, version and URL (without the trailing slash).
/// Service names with digits are names.
#[test]
fn the_verb_deploys_and_reports_the_line() {
    let farm = converged_farm();
    let host = [farm.ca.clone()];
    let token = farm.token_service();
    for ca_pem in ["", farm.ca_pem.as_str(), "  \n"] {
        let mut out = Vec::new();
        let line = deploy::run_with(
            &args_for(&farm, ca_pem, &["js-server", "js-agent2"]),
            &token,
            &host,
            &mut out,
        )
        .expect("deploy");
        assert_eq!(
            line,
            format!(
                "ok: every live host of js-server, js-agent2 runs {VERSION} ({})",
                farm.base
            )
        );
        assert!(
            String::from_utf8(out)
                .expect("utf-8")
                .contains("service js-agent2: deploy")
        );
    }
}

/// Asserts that `--ca-pem` text with no certificate is refused as such, and that bad
/// arguments are refused by the verb itself before anything is sent: an empty service name
/// and a repeated one.
#[test]
fn the_verb_refuses_a_bad_ca_and_bad_arguments() {
    let farm = converged_farm();
    let host = [farm.ca.clone()];
    let token = farm.token_service();
    let run = |args: &deploy::Args| deploy::run_with(args, &token, &host, &mut Vec::new());
    assert!(matches!(
        run(&args_for(&farm, "not a certificate", &["js-server"])),
        Err(xtask::Error::Deploy(Error::Ca))
    ));
    assert!(matches!(
        run(&args_for(&farm, "", &[""])),
        Err(xtask::Error::Deploy(Error::Service(s))) if s.is_empty()
    ));
    assert!(matches!(
        run(&args_for(&farm, "", &["js-agent", "js-agent"])),
        Err(xtask::Error::Deploy(Error::Duplicate(_)))
    ));
    assert!(farm.seen.lock().expect("log").is_empty());
}

/// Asserts that `deploy` with a real clock runs the whole deploy (a farm that has converged
/// needs no wait), and that the binary with a token service in its environment goes on to
/// call it: the runner's certificate is not in the system store, so it fails naming the
/// token request.
#[test]
fn the_real_clock_deploys_and_the_binary_asks_the_token_service() {
    let farm = converged_farm();
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: ca,
    };
    deploy::deploy(
        &farm.plan(&["js-server"]),
        &farm.token_service(),
        &roots,
        &mut Vec::new(),
    )
    .expect("deploy");

    let out = Command::new(XTASK)
        .args(["deploy", "--url", &farm.base, "--version", VERSION])
        .args(["--service", "js-server"])
        .env(
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            format!("{}/token", farm.base),
        )
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-key")
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ask the runner for an OIDC token"), "{err}");
}

/// Asserts that the flags become a plan: the URL without its trailing slash, the audience
/// the URL as given unless `--audience` is, and the waits in seconds.
#[test]
fn the_plan_of_the_flags() {
    let farm = converged_farm();
    let mut args = args_for(&farm, "", &["js-server"]);
    let plan = Plan::of(&args);
    assert_eq!(plan.url, farm.base);
    assert_eq!(plan.audience, format!("{}/", farm.base));
    assert_eq!(plan.deadline, Duration::from_secs(3600));
    assert_eq!(plan.stall, Duration::from_secs(1200));
    assert_eq!(plan.every, Duration::from_secs(30));
    args.audience = Some("coordinator".to_owned());
    assert_eq!(Plan::of(&args).audience, "coordinator");
}

/// Asserts that an answer that is not JSON fails the call and names it, and that the real
/// clock waits between reads (the interval is made short so the test is quick): a farm that
/// converges on its second read ends the deploy converged.
#[test]
fn an_answer_that_is_not_json_fails_and_the_real_clock_waits_between_reads() {
    let farm = Farm::start(|_| (200, json!("not json")));
    let (result, _, _) = farm.deploy(&farm.plan(&["js-server"]));
    assert!(
        matches!(&result, Err(Error::Call { what, why, .. })
            if *what == "read the service before the deploy" && why.contains("expected")),
        "{result:?}"
    );

    let farm = farm_with(|service, n| {
        let role = role_of(service);
        if n == 0 {
            conv("in_progress", VERSION, (role, 0, 1, 0, 0))
        } else {
            conv("converged", VERSION, (role, 1, 0, 0, 0))
        }
    });
    let ca = std::slice::from_ref(&farm.ca);
    let roots = Roots {
        runner: ca,
        coordinator: ca,
    };
    let mut plan = farm.plan(&["js-server"]);
    plan.every = Duration::from_millis(1);
    let mut out = Vec::new();
    deploy::deploy(&plan, &farm.token_service(), &roots, &mut out).expect("deploy");
    assert_eq!(farm.coordinator_calls().len(), 4);
}
