//! The black-box tests of the released js-agent and js-server (J1 and J6 of the black-box plan,
//! and the failures that belong to the same rig).
//!
//! The released artifacts, taken as the farm takes them and driven only from outside:
//!
//! - the js-agent `.deb` of the release this checkout is stamped with (`cargo xtask release
//!   stamp`, so this crate's version is the release's), installed with apt from the suite
//!   `testing` of the project's repository, on a host made ready as a farm host is, with the
//!   flags file of the farm's shape (`farm/js-agent.flags`), and run by the package's own unit,
//!   `js-agent-crun.service`;
//! - the js-server image of the same release (the js-lease-server OCI image of the GitHub
//!   release, checked against its `SHA256SUMS`), loaded into Docker and started with the control
//!   host's command line (`farm/js-server.args`): its user, a read-only root, no capability, TLS
//!   on every listener, hosts admitted by their certificates, and a Postgres store, which a
//!   container of its own serves and to which the test applies the state schema first.
//!
//! The test reaches them through the job service's API (the hosts, the jobs and their results),
//! the server's readiness route, the agent's health route, the unit, the files the package reads,
//! and the jobs the agent runs: a job is the runner's own `sleep` in an image the test serves
//! from a registry on the loopback. The pass conditions are the constants of the library
//! (`js_blackbox`), measured on the system's monotonic clock with a probe every second.
//!
//! Every test is `#[ignore]`d: it installs packages, mounts a disk, edits `/etc/hosts`, adds a
//! firewall rule and starts containers, so it runs as root on a disposable runner only, with
//! `--ignored --test-threads=1` (.github/workflows/js-blackbox.yml). The tests share one rig,
//! started by the first of them and left running for the next: each starts from a host in a
//! session with no job running, and leaves it so. A test that is not root, or a checkout that is
//! not a release's, fails saying so: nothing is skipped.

#![cfg(target_os = "linux")]

#[path = "released/agent.rs"]
mod agent;
#[path = "released/host.rs"]
mod host;
#[path = "released/image.rs"]
mod image;
#[path = "released/server.rs"]
mod server;

use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use js_blackbox::{
    BACK_WITHIN, LIVE, LOST, OUTAGE, PROBE_EVERY, READY_WITHIN, RUNNING, SESSION_WITHIN, SUCCEEDED,
    Seen, Version, Watch, host as host_of, job as job_of, result as result_of,
};
use js_client::Client;
use js_system_clock::SystemMonotonic;
use js_testkit::dir::TestDir;
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType,
};
use serde_json::json;

use crate::agent::{HOST_ID, HOSTS_LINE, SERVER_NAME};
use crate::host::{put, run, try_run};
use crate::server::Server;

/// The URI SAN hostkeeper puts in a host certificate beside the host's DNS name.
const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// How long a job of the test may run before the test gives up on it (a bound, not a pass
/// condition: the longest job sleeps 150 s).
const JOB_LIMIT: Duration = Duration::from_secs(600);

/// 32 random bytes from the kernel, in hex: a token or a password of the test's.
fn secret() -> String {
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").expect("open /dev/urandom"),
        &mut bytes,
    )
    .expect("read /dev/urandom");
    hex::encode(bytes)
}

/// A port no one listens on, on the loopback, picked by the kernel.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("a free port")
        .port()
}

/// Runs `body` with a watch on the system's monotonic clock, probing every [`PROBE_EVERY`].
fn watch<R>(body: impl FnOnce(&mut Watch<'_>) -> R) -> R {
    let clock = SystemMonotonic::default();
    let mut pause = std::thread::sleep;
    let mut watch = Watch::new(&clock, &mut pause, PROBE_EVERY);
    body(&mut watch)
}

/// The rig every test uses: the server and the agent of the release, in a session.
struct Rig {
    /// The scratch directory (the server's files, the scratch disk's image, the image tar).
    _dir: TestDir,
    /// The release.
    version: Version,
    server: Server,
    /// The job service's client, over TLS to the job port.
    client: Client,
    /// The job image's registry.
    registry: image::Registry,
    /// J6: how long after its container started the server answered `/readyz` 200, or why not.
    ready: Result<Duration, String>,
    /// J1: how long after its unit started the host was in a session, or why not.
    session: Result<Duration, String>,
}

static RIG: OnceLock<Mutex<Rig>> = OnceLock::new();

/// The rig, started by the first test that asks.
fn rig() -> MutexGuard<'static, Rig> {
    RIG.get_or_init(|| Mutex::new(Rig::start()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

impl Rig {
    fn start() -> Rig {
        assert!(
            rustix::process::geteuid().is_root(),
            "the black-box test installs a package, mounts a disk and starts containers: run it as root on a disposable runner"
        );
        let version = Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version");
        assert_ne!(
            version,
            Version(0, 1, 0),
            "this checkout is not stamped with a release's version (cargo xtask release stamp --to X.Y.Z)"
        );
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "released");

        // The CA the hosts' certificates chain to (hostkeeper's, on the farm), the host's
        // certificate, and the server's own, for its name and the loopback address.
        let ca = ca();
        let identity = host_identity(&ca);
        let made = rcgen::generate_simple_self_signed(vec![
            SERVER_NAME.to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .expect("the server's certificate");
        let (server_cert, server_key) = (made.cert.pem(), made.signing_key.serialize_pem());

        let registry = image::Registry::start();
        let store = watch(server::start_postgres);
        let image = server::load_image(&dir, version);

        // J6: the server's container is started; its readiness route is asked every second.
        let server =
            server::start_server(&dir, &image, &store, &ca.pem(), (&server_cert, &server_key));
        let ready = watch(|w| {
            w.until(READY_WITHIN, || match server.ready() {
                Ok(()) => Seen::Yes(()),
                Err(e) => Seen::No(e),
            })
        })
        .within("/readyz answers 200", READY_WITHIN)
        .map(|(after, ())| after)
        .map_err(|e| format!("{e}\n{}", Server::logs()));
        if let Err(e) = &ready {
            panic!("J6: {e}");
        }
        let client = Client::connect(
            &format!("https://127.0.0.1:{}", server.ports.job),
            &server.client_token,
            Some(server.cert.as_path()),
            Duration::from_secs(10),
        )
        .expect("the job service's client");

        // J1: the released package, on a host made ready as a farm host is, started by its unit.
        agent::add_repository("amd64");
        agent::install(version);
        agent::prepare_host(
            &dir,
            &server_cert,
            (&identity.0, &identity.1),
            server.ports.agent,
            registry.port,
        );
        let mut rig = Rig {
            _dir: dir,
            version,
            server,
            client,
            registry,
            ready,
            session: Err("not started".to_owned()),
        };
        agent::start();
        rig.session = rig.in_session(SESSION_WITHIN, version);
        rig
    }

    /// The host as the job service lists it, or why it cannot be read.
    fn host(&self) -> Result<js_blackbox::HostSeen, String> {
        let reply = self
            .client
            .get("/v1/hosts")
            .map_err(|e| format!("GET /v1/hosts: {e}"))?;
        if reply.status != 200 {
            return Err(format!("GET /v1/hosts: {} {}", reply.status, reply.body));
        }
        host_of(&reply.body, HOST_ID).ok_or_else(|| format!("{HOST_ID} is not listed"))
    }

    /// Waits up to `limit` for the host to be in a session with an agent of `version`: listed
    /// live with that version, and the agent's health route 200. How long it took, or why not.
    fn in_session(&self, limit: Duration, version: Version) -> Result<Duration, String> {
        let want = version.to_string();
        watch(|w| {
            w.until(limit, || match (self.host(), agent::healthy()) {
                (Ok(h), Ok(())) if h.state == LIVE && h.agent_version == want => Seen::Yes(()),
                (h, health) => Seen::No(format!("host {h:?}, health {health:?}")),
            })
        })
        .within(&format!("in a session with js-agent {want}"), limit)
        .map(|(after, ())| after)
        .map_err(|e| format!("{e}\n{}", agent::journal()))
    }

    /// The start of every test: the host in a session with the released agent.
    fn steady(&self) {
        if let Err(e) = self.in_session(SESSION_WITHIN, self.version) {
            panic!("the rig is not in a session: {e}");
        }
    }

    /// Submits a job that sleeps `seconds` and ends 0: its id.
    fn submit(&self, name: &str, seconds: u32) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst));
        let body = json!({"spec": {
            "jobId": id,
            "image": self.registry.image,
            "command": "/bin/sleep",
            "args": [seconds.to_string()],
            "wallLimitSeconds": "900",
            "needs": {"cpuMillis": "250", "memoryBytes": "134217728"}
        }});
        let reply = self
            .client
            .post("/v1/jobs", &body)
            .unwrap_or_else(|e| panic!("POST /v1/jobs: {e}"));
        assert!(
            reply.status == 200 || reply.status == 201,
            "POST /v1/jobs: {} {}",
            reply.status,
            reply.body
        );
        id
    }

    /// The job `id` as the job service answers it.
    fn job(&self, id: &str) -> Result<js_blackbox::JobSeen, String> {
        let reply = self
            .client
            .get(&format!("/v1/jobs/{id}"))
            .map_err(|e| format!("GET /v1/jobs/{id}: {e}"))?;
        job_of(&reply.body).map_err(|e| format!("GET /v1/jobs/{id}: {} {e}", reply.status))
    }

    /// Waits for the job to run on the host.
    fn running(&self, id: &str) -> js_blackbox::JobSeen {
        watch(|w| {
            w.until(JOB_LIMIT, || match self.job(id) {
                Ok(j) if j.state == RUNNING && j.host_id == HOST_ID => Seen::Yes(j),
                other => Seen::No(format!("{other:?}")),
            })
        })
        .within(&format!("job {id} runs on {HOST_ID}"), JOB_LIMIT)
        .map(|(_, j)| j)
        .unwrap_or_else(|e| panic!("{e}\n{}", agent::journal()))
    }

    /// Waits for the job to end, then holds its result to exit code 0 in its first attempt.
    fn ends_zero(&self, id: &str) {
        let ended = watch(|w| {
            w.until(JOB_LIMIT, || match self.job(id) {
                Ok(j) if j.ended() => Seen::Yes(j),
                other => Seen::No(format!("{other:?}")),
            })
        })
        .within(&format!("job {id} ends"), JOB_LIMIT)
        .unwrap_or_else(|e| panic!("{e}\n{}", agent::journal()));
        let reply = self
            .client
            .get(&format!("/v1/jobs/{id}/result"))
            .unwrap_or_else(|e| panic!("GET /v1/jobs/{id}/result: {e}"));
        let result = result_of(&reply.body).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            result.state == SUCCEEDED && result.exit_code == Some(0) && result.attempt == 1,
            "job {id} did not end with exit code 0 in its first attempt: {result:?} (job {:?})\n{}",
            ended.1,
            agent::journal()
        );
    }
}

/// The CA of the hosts' certificates: hostkeeper's CA as the server sees it.
fn ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "test-only hostkeeper CA");
    let key = KeyPair::generate().expect("a CA key");
    CertifiedIssuer::self_signed(params, key).expect("the CA")
}

/// The host's certificate and key, as PEM, in the shape hostkeeper issues: one DNS name, the
/// host id, and client authentication alone.
fn host_identity(ca: &CertifiedIssuer<'static, KeyPair>) -> (String, String) {
    let key = KeyPair::generate().expect("a host key");
    let mut params = CertificateParams::new(vec![HOST_ID.to_owned()]).expect("host params");
    let uri = Ia5String::try_from(HOST_ROLE_URI.to_owned()).expect("an IA5 URI");
    params.subject_alt_names.push(SanType::URI(uri));
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("host:{HOST_ID}"));
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.signed_by(&key, ca).expect("the host certificate");
    (cert.pem(), key.serialize_pem())
}

/// A firewall rule that drops what reaches the server's agent port, removed when dropped.
struct Cut(u16);

impl Cut {
    fn args(&self, op: &str) -> Vec<String> {
        let port = self.0.to_string();
        [
            op,
            "INPUT",
            "--protocol",
            "tcp",
            "--dport",
            port.as_str(),
            "--jump",
            "DROP",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn new(port: u16) -> Cut {
        let cut = Cut(port);
        let args = cut.args("--insert");
        run(
            "iptables",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        cut
    }
}

impl Drop for Cut {
    fn drop(&mut self) {
        let args = self.args("--delete");
        let _ = try_run(
            "iptables",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
    }
}

/// `/etc/hosts` without the server's name, put back when dropped.
struct Unresolved(String);

impl Unresolved {
    fn new() -> Unresolved {
        let hosts = std::fs::read_to_string("/etc/hosts").expect("read /etc/hosts");
        let without: String = hosts
            .lines()
            .filter(|l| l.trim() != HOSTS_LINE)
            .map(|l| format!("{l}\n"))
            .collect();
        put(std::path::Path::new("/etc/hosts"), without, 0o644, (0, 0));
        Unresolved(hosts)
    }
}

impl Drop for Unresolved {
    fn drop(&mut self) {
        put(std::path::Path::new("/etc/hosts"), &self.0, 0o644, (0, 0));
    }
}

/// J6. The released js-server image, started as the control host starts it, answers `/readyz`
/// 200 within 60 s of its container's start, and runs a job through the released agent: the job
/// ends with exit code 0.
#[test]
#[ignore = "root on a disposable runner: run by .github/workflows/js-blackbox.yml"]
fn j6_the_released_server_image_is_ready_within_60_s_and_runs_a_job() {
    let rig = rig();
    let ready = rig.ready.clone().unwrap_or_else(|e| panic!("{e}"));
    eprintln!(
        "/readyz answered 200 {} ms after the container started",
        ready.as_millis()
    );
    rig.steady();
    let id = rig.submit("j6", 1);
    rig.ends_zero(&id);
}

/// J1. The released `.deb`, installed and started under systemd with flags of the farm's shape,
/// joins the released server: the host is in a session within 60 s of the unit's start, and a
/// job ends with exit code 0.
#[test]
#[ignore = "root on a disposable runner: run by .github/workflows/js-blackbox.yml"]
fn j1_the_released_agent_with_the_farms_flags_is_in_a_session_within_60_s_and_runs_a_job() {
    let rig = rig();
    let session = rig.session.clone().unwrap_or_else(|e| panic!("{e}"));
    eprintln!(
        "in a session {} ms after the unit started",
        session.as_millis()
    );
    rig.steady();
    let id = rig.submit("j1", 1);
    rig.ends_zero(&id);
}

/// The server is restarted while a job runs: the job ends with exit code 0 in its first
/// attempt, and no listing of the hosts the server gives after the restart shows the host lost.
#[test]
#[ignore = "root on a disposable runner: run by .github/workflows/js-blackbox.yml"]
fn a_server_restart_while_a_job_runs_ends_the_job_0_and_never_shows_the_host_lost() {
    let rig = rig();
    rig.steady();
    let id = rig.submit("server-restart", 90);
    rig.running(&id);
    rig.server.restart();
    // From the restart on, until the job ends: every listing the server gives must show the host
    // not lost, and the job not lost nor its host silent. A probe the server does not answer
    // (still starting) shows nothing and passes.
    let mut seen = 0u32;
    let held = watch(|w| {
        w.hold_until(JOB_LIMIT, || -> Result<bool, String> {
            if let Ok(h) = rig.host() {
                seen += 1;
                if h.state == LOST {
                    return Err(format!("the host is listed {LOST} ({h:?})"));
                }
            }
            match rig.job(&id) {
                Ok(j) if j.host_silent || j.state == "JOB_STATE_LOST" => {
                    Err(format!("the job shows its host lost: {j:?}"))
                }
                Ok(j) => Ok(j.ended()),
                Err(_) => Ok(false),
            }
        })
    });
    if let Err(e) = held.kept("the host is never shown lost") {
        panic!("{e}\n{}", Server::logs());
    }
    assert!(seen > 0, "the restarted server never listed the hosts");
    rig.ends_zero(&id);
}

/// The agent is updated while a job runs, from the previous release's package to this release's,
/// the way the update hook does it on a farm host (the new package installed, the handover file
/// written, the unit restarted): the job is handed over to the new agent and ends with exit code
/// 0 in its first attempt, and the host lists the new version within 60 s of the restart.
#[test]
#[ignore = "root on a disposable runner: run by .github/workflows/js-blackbox.yml"]
fn an_agent_update_while_a_job_runs_hands_the_job_over_and_it_ends_0() {
    let rig = rig();
    rig.steady();
    let previous = agent::previous(rig.version);
    agent::install(previous);
    agent::restart();
    rig.in_session(SESSION_WITHIN, previous)
        .unwrap_or_else(|e| panic!("the previous release {previous}: {e}"));
    let id = rig.submit("agent-update", 90);
    let before = rig.running(&id);
    agent::install(rig.version);
    agent::ask_for_handover();
    agent::restart();
    if let Err(e) = rig.in_session(SESSION_WITHIN, rig.version) {
        panic!("after the update: {e}");
    }
    let now = rig.job(&id).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        (now.attempt, now.host_id.as_str()),
        (before.attempt, HOST_ID),
        "the job left its attempt or its host in the update: {now:?}"
    );
    rig.ends_zero(&id);
}

/// The server is unreachable for 60 s while a job runs (what reaches its agent port is dropped):
/// the job keeps running through the outage, the agent is back in a session within 30 s of the
/// server's return, and the job ends with exit code 0.
#[test]
#[ignore = "root on a disposable runner: run by .github/workflows/js-blackbox.yml"]
fn the_server_unreachable_for_60_s_keeps_the_job_and_the_agent_is_back_within_30_s() {
    let rig = rig();
    rig.steady();
    let id = rig.submit("server-unreachable", 150);
    rig.running(&id);
    {
        let _cut = Cut::new(rig.server.ports.agent);
        let held = watch(|w| {
            w.hold(OUTAGE, || match rig.job(&id) {
                Ok(j) if j.ended() => Err(format!("the job ended in the outage: {j:?}")),
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            })
        });
        if let Err(e) = held.kept("the job keeps running while the server is unreachable") {
            panic!("{e}\n{}", agent::journal());
        }
    }
    let back = rig
        .in_session(BACK_WITHIN, rig.version)
        .unwrap_or_else(|e| panic!("after the server's return: {e}"));
    eprintln!(
        "back in a session {} ms after the server's return",
        back.as_millis()
    );
    rig.ends_zero(&id);
}

/// The server's name cannot be looked up for 60 s after the first contact (its line is taken out
/// of `/etc/hosts`): the agent keeps its session, the host listed live and the agent's health
/// route 200 at every probe, and a job submitted in the outage ends with exit code 0.
#[test]
#[ignore = "root on a disposable runner: run by .github/workflows/js-blackbox.yml"]
fn a_name_lookup_failing_for_60_s_after_the_first_contact_keeps_the_session() {
    let rig = rig();
    rig.steady();
    let _unresolved = Unresolved::new();
    assert!(
        try_run("getent", &["hosts", SERVER_NAME]).is_err(),
        "{SERVER_NAME} is still found after its line left /etc/hosts"
    );
    let id = rig.submit("name-lookup", 30);
    let held = watch(|w| {
        w.hold(OUTAGE, || -> Result<(), String> {
            let h = rig.host()?;
            if h.state != LIVE {
                return Err(format!("the host is listed {}", h.state));
            }
            agent::healthy()
        })
    });
    if let Err(e) = held.kept("the session is kept while the name is not found") {
        panic!("{e}\n{}", agent::journal());
    }
    rig.ends_zero(&id);
}
