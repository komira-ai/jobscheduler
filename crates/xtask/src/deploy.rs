//! `cargo xtask deploy --service NAME... --version X.Y.Z --url https://...`: after a release
//! is published, tell the update coordinator "desired = this release" for each service named,
//! in the order named, and wait until the services' hosts converge (docs/release.md, "The
//! gamma step").
//!
//! # The version text
//!
//! The coordinator compares the desired version with the version text each host reports for
//! the service in its heartbeat, as text. `js-lease-server` (service `js-server`) and
//! `js-agent` (service `js-agent`) report the version of their own binary, `--version`'s
//! last word: the release version `X.Y.Z`, which `release stamp` puts in the build and
//! `release plan` decides. So the text is exactly `X.Y.Z` ([`check_version`]): digits only,
//! no leading zeros, no `v` (the tag is `vX.Y.Z`; the tag is not the version) and no suffix.
//! The workflow passes the plan's `version` output and nothing derived from it.
//!
//! # The steps, per service
//!
//! Each over HTTPS (the runner's token service as the system store trusts it, the coordinator
//! as the system store does, or, with `--ca-pem`, as that CA alone does).
//!
//! 1. With an empty `--url` the verb prints a notice and exits 0: a fork, or a repository
//!    whose operator has not set the coordinator's address, deploys nothing and fails nothing.
//! 2. `GET /v1/convergence?service=NAME` once, before anything is changed: the hosts live then
//!    (at the desired version, updating or failed; not `down`) are the baseline, per role.
//! 3. `POST /v1/desired` with `{"service": NAME, "version": V, "min_up": {...}}`, authenticated
//!    by a GitHub Actions OIDC token whose audience is the coordinator (`--audience`, by
//!    default the URL). `min_up` is required by the coordinator's request shape; it is sent as
//!    zeros, because the limits of a service's roles are in the service's definition on the
//!    coordinator.
//! 4. `GET /v1/convergence?service=NAME` every `--every-secs`, with a fresh token each time.
//!
//! A service ends with success when the coordinator says `converged` and every role has at
//! least as many hosts at the version as it had live at the baseline. It ends with failure
//! when a role has fewer, when the coordinator says `blocked`, when the counts have not
//! changed for `--stall-min` minutes, when `--deadline-min` minutes are up, when a call is
//! refused (401, 403), when the coordinator's desired version is no longer this release's,
//! or the job has no OIDC token to ask for. A poll that fails otherwise is reported and
//! tried again. The next service is not touched after a failure: with `--service js-server
//! --service js-agent` no agent is told to update to a server version that has not
//! converged.
//!
//! The OIDC token is asked of the runner's token service (`ACTIONS_ID_TOKEN_REQUEST_URL` and
//! `ACTIONS_ID_TOKEN_REQUEST_TOKEN`, set for a job with `id-token: write`) and masked in the
//! log as soon as it is read.

use std::io::Write;
use std::time::{Duration, Instant};

use clap::Args as ClapArgs;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::{Value, json};

use crate::fetch;

/// The longest any one call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The most bytes of any answer read.
const MAX_ANSWER: u64 = 1 << 20;

/// How many times a call is tried when its failure may pass.
const ATTEMPTS: u32 = 4;

/// What the calls are named in an error.
const SET_DESIRED: &str = "set the desired version";
const READ_BASELINE: &str = "read the service before the deploy";
const READ_CONVERGENCE: &str = "read the convergence";

/// The paths of the desired state and of the convergence read.
const DESIRED_PATH: &str = "/v1/desired";
const CONVERGENCE_PATH: &str = "/v1/convergence";

/// The variables the platform sets for a job that may ask for an OIDC token.
const TOKEN_URL_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
const TOKEN_KEY_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";

/// Whether a failed call may pass: the coordinator's 503 (it could not reach the token
/// issuer, which is no verdict) on the first two calls of a service.
fn retryable(what: &str, why: &str) -> bool {
    matches!(what, SET_DESIRED | READ_BASELINE) && why == "status 503"
}

/// Whether `version` is the version text the services report: `X.Y.Z`, three numbers of
/// digits without leading zeros, and nothing else.
///
/// # Errors
/// [`Error::Version`] when it is not.
pub fn check_version(version: &str) -> Result<(), Error> {
    let number = |n: &str| {
        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && (n == "0" || !n.starts_with('0'))
    };
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() == 3 && parts.iter().all(|n| number(n)) {
        Ok(())
    } else {
        Err(Error::Version(version.to_owned()))
    }
}

/// Whether `name` is a service name the coordinator accepts: lowercase letters, digits and
/// `-`, at least one.
fn service_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What `args` must satisfy before anything is sent, whether or not a coordinator is set.
fn check_args(args: &Args) -> Result<(), Error> {
    if let Some(bad) = args.services.iter().find(|s| !service_name_ok(s)) {
        return Err(Error::Service(bad.clone()));
    }
    let mut seen = std::collections::BTreeSet::new();
    if let Some(twice) = args.services.iter().find(|s| !seen.insert(s.as_str())) {
        return Err(Error::Duplicate(twice.clone()));
    }
    check_version(&args.version)
}

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The coordinator's control address, `https://<name>:<port>`. Empty: print a notice and
    /// deploy nothing.
    #[arg(long)]
    pub url: String,
    /// The release version, `X.Y.Z`, as the services' heartbeats report it once they run it
    /// (the plan's `version`, not the tag `vX.Y.Z`).
    #[arg(long)]
    pub version: String,
    /// A service to deploy; give the flag once per service. They are deployed in the order
    /// given, each converged before the next is set.
    #[arg(long = "service", required = true)]
    pub services: Vec<String>,
    /// The CA certificate (PEM text) the coordinator's certificate must chain to, instead of
    /// the system store's roots. Empty: the system store.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub ca_pem: String,
    /// The audience of the OIDC token (default: `--url` as given).
    #[arg(long)]
    pub audience: Option<String>,
    /// Minutes to wait for one service to converge before failing.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=1440))]
    pub deadline_min: u64,
    /// Minutes the counts may stay as they are before failing.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=1440))]
    pub stall_min: u64,
    /// Seconds between two reads.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub every_secs: u64,
}

/// Why the deploy did not end converged.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The job has no OIDC token service.
    #[error(
        "{TOKEN_URL_VAR} or {TOKEN_KEY_VAR} is not set: the job needs `permissions: id-token: write`"
    )]
    NoTokenService,
    /// A service name the coordinator would not accept.
    #[error("--service {0:?} is not a service name: lowercase letters, digits and -")]
    Service(String),
    /// A service named twice.
    #[error("--service {0:?} is given twice")]
    Duplicate(String),
    /// No certificate in `--ca-pem`.
    #[error("--ca-pem holds no certificate")]
    Ca,
    /// A version that the services could never report.
    #[error(
        "--version {0:?} is not the form the services report: it must be X.Y.Z, digits only (the tag vX.Y.Z is not that form)"
    )]
    Version(String),
    /// A URL that is not `https://host[:port]`.
    #[error("the URL must be https://<host>[:port] with no path, got {0:?}")]
    Url(String),
    /// A call failed.
    #[error("{service}: {what}: {why}")]
    Call {
        /// The service the call was about.
        service: String,
        /// The call.
        what: &'static str,
        /// Why it failed.
        why: String,
    },
    /// The coordinator refused the job's token.
    #[error(
        "{service}: {what}: refused with status {status}: the coordinator's OIDC allow-list does not accept this job's token for this service"
    )]
    Refused {
        /// The service.
        service: String,
        /// The call.
        what: &'static str,
        /// The status.
        status: u16,
    },
    /// An answer this client cannot read.
    #[error("the coordinator answered something this client cannot read: {0}")]
    Answer(String),
    /// A host failed its update.
    #[error(
        "{0}: blocked: a host failed its update to this version; the table above says which role"
    )]
    Blocked(String),
    /// Another writer replaced the desired version.
    #[error(
        "{service}: the coordinator's desired version is {desired:?}, not this release's: another writer replaced it"
    )]
    Superseded {
        /// The service.
        service: String,
        /// What the coordinator says is desired.
        desired: String,
    },
    /// Hosts that were live are not at the version.
    #[error(
        "{service}, {role}: {expected} hosts were live before the deploy, {reached} run {version}: {} went down",
        .expected - .reached
    )]
    Lost {
        /// The service.
        service: String,
        /// The role.
        role: String,
        /// The hosts live at the baseline.
        expected: u64,
        /// The hosts at the version.
        reached: u64,
        /// The version.
        version: String,
    },
    /// The counts stopped changing.
    #[error("{service}: the counts have not changed for {seconds} seconds")]
    Stalled {
        /// The service.
        service: String,
        /// The stall limit.
        seconds: u64,
    },
    /// The deadline passed.
    #[error("{service}: not converged after {seconds} seconds")]
    Deadline {
        /// The service.
        service: String,
        /// The deadline.
        seconds: u64,
    },
    /// The log could not be written.
    #[error("write the log: {0}")]
    Output(String),
}

/// How the deploy waits.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The coordinator, `https://host[:port]`, no trailing slash.
    pub url: String,
    /// The audience of the OIDC token.
    pub audience: String,
    /// The version text to set.
    pub version: String,
    /// The services, in the order they are deployed.
    pub services: Vec<String>,
    /// The longest one service may take.
    pub deadline: Duration,
    /// The longest the counts may stay as they are.
    pub stall: Duration,
    /// The time between two reads.
    pub every: Duration,
}

impl Plan {
    /// The plan of `args`.
    #[must_use]
    pub fn of(args: &Args) -> Self {
        Self {
            url: args.url.trim_end_matches('/').to_owned(),
            audience: args.audience.clone().unwrap_or_else(|| args.url.clone()),
            version: args.version.clone(),
            services: args.services.clone(),
            deadline: Duration::from_secs(args.deadline_min * 60),
            stall: Duration::from_secs(args.stall_min * 60),
            every: Duration::from_secs(args.every_secs),
        }
    }
}

/// The path of the convergence read of `service` (a name needs no escaping:
/// [`service_name_ok`]).
#[must_use]
pub fn convergence_path(service: &str) -> String {
    format!("{CONVERGENCE_PATH}?service={service}")
}

/// The body of the `POST /v1/desired` that sets `version` for `service`.
#[must_use]
pub fn desired_body(service: &str, version: &str) -> Value {
    json!({"service": service, "version": version, "min_up": {"storage": 0, "frontend": 0}})
}

/// The time the wait reads and spends: `now` is the time since some fixed start, `sleep`
/// waits for the duration it is given. A test passes a virtual clock whose `sleep` moves `now`.
pub struct Clock<'a> {
    /// The time since a fixed start.
    pub now: Box<dyn Fn() -> Duration + 'a>,
    /// Waits.
    pub sleep: Box<dyn Fn(Duration) + 'a>,
}

/// The runner's token service.
#[derive(Clone, Debug)]
pub struct TokenService {
    /// Where to ask.
    pub url: String,
    /// The bearer key.
    pub key: String,
}

impl TokenService {
    /// The service the platform set for this job.
    ///
    /// # Errors
    /// [`Error::NoTokenService`] when either variable is missing or empty.
    pub fn from_env() -> Result<Self, Error> {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        match (var(TOKEN_URL_VAR), var(TOKEN_KEY_VAR)) {
            (Some(url), Some(key)) => Ok(Self { url, key }),
            _ => Err(Error::NoTokenService),
        }
    }
}

/// Whom each side of the deploy is trusted by.
#[derive(Clone, Copy, Debug)]
pub struct Roots<'a> {
    /// The runner's token service.
    pub runner: &'a [CertificateDer<'static>],
    /// The coordinator.
    pub coordinator: &'a [CertificateDer<'static>],
}

/// The certificates in `pem`.
///
/// # Errors
/// [`Error::Ca`] when there is none, or the text is not PEM.
pub fn pem_roots(pem: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certs: Result<Vec<_>, _> = CertificateDer::pem_slice_iter(pem.as_bytes()).collect();
    certs.ok().filter(|c| !c.is_empty()).ok_or(Error::Ca)
}

/// The verb: the line for stdout when it ends converged.
///
/// # Errors
/// [`crate::Error`], for every end but convergence.
pub fn run(args: &Args) -> Result<String, crate::Error> {
    // Before the notice for an unset URL: a version the services could never report is a
    // defect of the caller whether or not a coordinator is there to be told.
    check_args(args)?;
    if args.url.trim().is_empty() {
        return Ok(
            "::notice::the deploy step is off: the repository variable JS_COORDINATOR_URL is not set"
                .to_owned(),
        );
    }
    let service = TokenService::from_env()?;
    run_with(
        args,
        &service,
        &fetch::host_roots(),
        &mut std::io::stdout().lock(),
    )
}

/// The verb for `args` with the token service and the system roots given: the coordinator is
/// trusted by `--ca-pem` when it is not empty, else by `host`.
///
/// # Errors
/// [`crate::Error`], for every end but convergence.
pub fn run_with(
    args: &Args,
    service: &TokenService,
    host: &[CertificateDer<'static>],
    out: &mut impl Write,
) -> Result<String, crate::Error> {
    check_args(args)?;
    let plan = Plan::of(args);
    let pinned;
    let roots = Roots {
        runner: host,
        coordinator: if args.ca_pem.trim().is_empty() {
            host
        } else {
            pinned = pem_roots(&args.ca_pem)?;
            &pinned
        },
    };
    deploy(&plan, service, &roots, out)?;
    Ok(format!(
        "ok: every live host of {} runs {} ({})",
        plan.services.join(", "),
        plan.version,
        plan.url
    ))
}

/// Sets `plan.version` for each service of the plan in turn and waits for it, writing each
/// table to `out`.
///
/// # Errors
/// [`Error`]: anything but `converged`, for the first service that does not.
pub fn deploy(
    plan: &Plan,
    service: &TokenService,
    roots: &Roots<'_>,
    out: &mut impl Write,
) -> Result<(), Error> {
    let start = Instant::now();
    let clock = Clock {
        now: Box::new(|| start.elapsed()),
        sleep: Box::new(std::thread::sleep),
    };
    deploy_on(plan, service, roots, &clock, out)
}

/// [`deploy`] on a clock it is given.
///
/// # Errors
/// [`Error`]: anything but `converged`, for the first service that does not.
pub fn deploy_on(
    plan: &Plan,
    service: &TokenService,
    roots: &Roots<'_>,
    clock: &Clock<'_>,
    out: &mut impl Write,
) -> Result<(), Error> {
    check_version(&plan.version)?;
    if !plan.url.starts_with("https://") || plan.url[8..].contains('/') {
        return Err(Error::Url(plan.url.clone()));
    }
    let agent = |roots: &[CertificateDer<'static>]| -> ureq::Agent {
        ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(CALL_TIMEOUT))
            .tls_config(fetch::tls_config(roots))
            .build()
            .into()
    };
    let client = Client {
        runner: agent(roots.runner),
        coordinator: agent(roots.coordinator),
        plan,
        service,
        clock,
    };
    for name in &plan.services {
        deploy_service(&client, name, out)?;
    }
    Ok(())
}

/// One service: baseline, set, wait.
fn deploy_service(client: &Client<'_>, name: &str, out: &mut dyn Write) -> Result<(), Error> {
    let plan = client.plan;
    let clock = client.clock;
    let read_path = convergence_path(name);
    log(out, &format!("service {name}: deploy {}", plan.version))?;
    let baseline =
        Read::of(&client.send_retrying(out, name, READ_BASELINE, &read_path, None)?)?.live();
    log(out, &baseline_line(&baseline))?;
    let body = desired_body(name, &plan.version);
    client.send_retrying(out, name, SET_DESIRED, DESIRED_PATH, Some(&body))?;
    log(out, &format!("desired version {} set", plan.version))?;

    let started = (clock.now)();
    let mut changed = started;
    let mut last: Option<(String, Value)> = None;
    loop {
        match client.send(out, name, READ_CONVERGENCE, &read_path, None) {
            Ok(answer) => {
                let read = Read::of(&answer)?;
                log(out, &read.table())?;
                if read.desired != plan.version {
                    return Err(Error::Superseded {
                        service: name.to_owned(),
                        desired: read.desired,
                    });
                }
                match read.state.as_str() {
                    "converged" => return read.holds(name, &baseline, &plan.version),
                    "blocked" => return Err(Error::Blocked(name.to_owned())),
                    _ => {}
                }
                let now = (read.state, answer["roles"].clone());
                if last.as_ref() != Some(&now) {
                    last = Some(now);
                    changed = (clock.now)();
                }
            }
            Err(e @ Error::Refused { .. }) => return Err(e),
            Err(e) => log(out, &format!("the read failed, trying again: {e}"))?,
        }
        let elapsed = (clock.now)().saturating_sub(started);
        if elapsed >= plan.deadline {
            return Err(Error::Deadline {
                service: name.to_owned(),
                seconds: plan.deadline.as_secs(),
            });
        }
        if (clock.now)().saturating_sub(changed) >= plan.stall {
            return Err(Error::Stalled {
                service: name.to_owned(),
                seconds: plan.stall.as_secs(),
            });
        }
        (clock.sleep)(plan.every.min(plan.deadline - elapsed));
    }
}

/// One line of the log, flushed.
fn log(out: &mut dyn Write, text: &str) -> Result<(), Error> {
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::Output(e.to_string()))
}

struct Client<'a> {
    runner: ureq::Agent,
    coordinator: ureq::Agent,
    plan: &'a Plan,
    service: &'a TokenService,
    clock: &'a Clock<'a>,
}

impl Client<'_> {
    /// [`Client::send`], tried again (up to `ATTEMPTS` calls) when the failure may pass.
    fn send_retrying(
        &self,
        out: &mut dyn Write,
        service: &str,
        what: &'static str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let mut attempt = 1;
        loop {
            match self.send(out, service, what, path, body) {
                Err(Error::Call { what, why, .. })
                    if attempt < ATTEMPTS && retryable(what, &why) =>
                {
                    log(
                        out,
                        &format!("the call failed, trying again: {what}: {why}"),
                    )?;
                    attempt += 1;
                    (self.clock.sleep)(self.plan.every);
                }
                other => return other,
            }
        }
    }

    /// A fresh OIDC token for the plan's audience; masked in the log.
    fn token(&self, out: &mut dyn Write, service: &str) -> Result<String, Error> {
        let call = |why: String| Error::Call {
            service: service.to_owned(),
            what: "ask the runner for an OIDC token",
            why,
        };
        let mut url = self.service.url.clone();
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("audience=");
        url.push_str(&percent_encode(&self.plan.audience));
        let mut response = self
            .runner
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.service.key))
            .call()
            .map_err(|e| call(e.to_string()))?;
        let status = response.status().as_u16();
        let text = answer_text(&mut response).map_err(call)?;
        if status != 200 {
            return Err(call(format!("status {status}")));
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| call(e.to_string()))?;
        let token = value["value"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| call("the answer has no token".to_owned()))?;
        // Masked in the log before anything else can print it.
        writeln!(out, "::add-mask::{token}").map_err(|e| Error::Output(e.to_string()))?;
        Ok(token.to_owned())
    }

    /// One call to the coordinator with a fresh token: a POST of `body`, or a GET.
    fn send(
        &self,
        out: &mut dyn Write,
        service: &str,
        what: &'static str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let call = |why: String| Error::Call {
            service: service.to_owned(),
            what,
            why,
        };
        let bearer = format!("Bearer {}", self.token(out, service)?);
        let url = format!("{}{path}", self.plan.url);
        let mut response = match body {
            Some(body) => self
                .coordinator
                .post(&url)
                .header("Authorization", bearer)
                .content_type("application/json")
                .send(body.to_string().as_str()),
            None => self
                .coordinator
                .get(&url)
                .header("Authorization", bearer)
                .call(),
        }
        .map_err(|e| call(e.to_string()))?;
        let status = response.status().as_u16();
        if matches!(status, 401 | 403) {
            return Err(Error::Refused {
                service: service.to_owned(),
                what,
                status,
            });
        }
        let text = answer_text(&mut response).map_err(call)?;
        if status != 200 {
            return Err(call(format!("status {status}")));
        }
        serde_json::from_str(&text).map_err(|e| call(e.to_string()))
    }
}

fn answer_text(response: &mut ureq::http::Response<ureq::Body>) -> Result<String, String> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_ANSWER)
        .read_to_string()
        .map_err(|e| e.to_string())
}

/// `text` with everything but unreserved characters as `%XX`.
fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `text` as one short plain line: control characters dropped, no leading `::` (the runner
/// reads a line that starts with it as a workflow command), at most 64 characters.
fn plain(text: &str) -> String {
    let line: String = text.chars().filter(|c| !c.is_control()).take(64).collect();
    line.trim_start_matches(':').to_owned()
}

/// The baseline as one log line.
fn baseline_line(baseline: &[(String, u64)]) -> String {
    let roles: Vec<String> = baseline.iter().map(|(r, n)| format!("{r} {n}")).collect();
    format!(
        "baseline: hosts live before the deploy: {}",
        if roles.is_empty() {
            "none".to_owned()
        } else {
            roles.join(", ")
        }
    )
}

/// What the coordinator said, as far as the table needs it.
struct Read {
    state: String,
    desired: String,
    quiet: bool,
    roles: Vec<(String, [u64; 4])>,
}

impl Read {
    fn of(answer: &Value) -> Result<Self, Error> {
        let bad = |what: &str| Error::Answer(format!("no {what}"));
        let text = |key: &str| answer[key].as_str().map(str::to_owned);
        let mut roles = Vec::new();
        for row in answer["roles"].as_array().ok_or_else(|| bad("roles"))? {
            let count = |key: &str| row[key].as_u64().ok_or_else(|| bad(key));
            roles.push((
                plain(row["role"].as_str().ok_or_else(|| bad("role"))?),
                [
                    count("at_desired")?,
                    count("updating")?,
                    count("failed")?,
                    count("down")?,
                ],
            ));
        }
        Ok(Self {
            state: text("state")
                .map(|t| plain(&t))
                .ok_or_else(|| bad("state"))?,
            desired: text("desired").map_or_else(|| "(none)".to_owned(), |t| plain(&t)),
            quiet: answer["quiet"].as_bool().unwrap_or(false),
            roles,
        })
    }

    /// The hosts live now (not `down`) per role.
    fn live(&self) -> Vec<(String, u64)> {
        self.roles
            .iter()
            .map(|(role, [at, updating, failed, _])| (role.clone(), at + updating + failed))
            .collect()
    }

    /// Whether every role of the baseline has as many hosts at the version as it had live.
    fn holds(&self, service: &str, baseline: &[(String, u64)], version: &str) -> Result<(), Error> {
        for (role, expected) in baseline {
            let reached = self
                .roles
                .iter()
                .find(|(r, _)| r == role)
                .map_or(0, |(_, [at, ..])| *at);
            if reached < *expected {
                return Err(Error::Lost {
                    service: service.to_owned(),
                    role: role.clone(),
                    expected: *expected,
                    reached,
                    version: version.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn table(&self) -> String {
        let mut out = format!(
            "farm: {}, desired {}{}\n{:<12}{:>11}{:>10}{:>8}{:>6}",
            self.state,
            self.desired,
            if self.quiet {
                " (coordinator just started)"
            } else {
                ""
            },
            "role",
            "at_desired",
            "updating",
            "failed",
            "down"
        );
        for (role, [at, updating, failed, down]) in &self.roles {
            out.push_str(&format!(
                "\n{role:<12}{at:>11}{updating:>10}{failed:>8}{down:>6}"
            ));
        }
        out
    }
}
