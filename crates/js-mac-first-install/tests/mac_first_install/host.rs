//! What a Mac Studio holds before js-agent's first install, and the lease server it joins.
//!
//! On a Studio, hostkeeper's daemon keeps the host's facts (`/var/lib/hostkeeper/facts.json`)
//! and its host certificate (`/var/lib/hostkeeper/identity/host.{crt,key}`), and the Mac config
//! package puts the agent's flags file and the lease server's CA in `/etc/js-agent/`. Here the
//! same files are written at the same paths, with the same owners and modes: the certificate is
//! minted by a CA of the test's own in the shape hostkeeper issues (one DNS SAN, the host id;
//! `clientAuth`; hostkeeper's host-role URI), and the flags are the config package's lines with a
//! loopback lease server and documentation-range endpoints in place of the production ones.
//!
//! The lease server is the one the agent's end-to-end tests start in their process (js-lease-server
//! `start`), over TLS with the certificate the flags file's `--ca-cert` names, trusting the test's
//! CA for agents (`--agent-client-ca`), with a pool `mac-build` that asks `os=macos` and
//! `arch=arm64`. Its clock reads the host's time when it starts and then runs with the real time,
//! and the server is ticked as its own interval would tick it (see [`Pump`]): a restarted
//! agent's hello is refused `host-live` until the session before it has gone without a beat for
//! the pool's grace, as on a real server, which a clock that stood still would never let happen;
//! and what the server stamps (a cordon's expiry) is judged by the update hook against the
//! host's own clock, as a real server's is, which a clock set apart from the host's would fail.
//! The certificates are valid from 2026-09-01 to 3000-01-01.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_client::{Client, HostIdentity};
use js_lease_server::{Cli, Config, Running, SharedClock, TickHandle, channel_ticker, start};
use js_types::{ManualClock, Timestamp};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType, SerialNumber, date_time_ymd,
};
use serde_json::Value;
use tokio::runtime::Runtime;

/// The host id, which hostkeeper's facts name and the certificate's one DNS SAN carries.
pub const HOST_ID: &str = "studio-first-install-1";

/// The pool of the Studios.
const POOL: &str = "mac-build";

/// The agent's flags file, where the Mac config package installs it.
pub const FLAGS_FILE: &str = "/etc/js-agent/flags";

/// The lease server's CA, beside the flags.
const CA_FILE: &str = "/etc/js-agent/ca.pem";

/// hostkeeper's facts of the host.
const FACTS_FILE: &str = "/var/lib/hostkeeper/facts.json";

/// hostkeeper's copy of the host identity.
const IDENTITY_DIR: &str = "/var/lib/hostkeeper/identity";

/// The URI SAN hostkeeper puts on a host's own certificate.
const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// The client token the test reads the host list with.
const CLIENT_TOKEN: &str = "client-token-0123456789-abcdefghijkl";

/// The pool's join token, which an agent with a host certificate neither reads nor sends.
const JOIN: &str = "join-token-A-0123456789-abcdefghijklmnop";

/// The pool the Studios join.
const POOL_TOML: &str = r#"
name = "mac-build"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 1000
memory_bytes = 1073741824

[capabilities]
os = "macos"
arch = "arm64"
"#;

/// `contents` at `path`, mode `mode`, its missing parents made 0755 (root's, as the test runs
/// as root).
pub fn put(path: &Path, contents: &str, mode: u32) {
    let parent = path.parent().expect("a parent");
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(parent)
        .unwrap_or_else(|e| panic!("make {}: {e}", parent.display()));
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// A certificate authority of the test's own: hostkeeper's CA as the lease server sees it.
struct Ca(CertifiedIssuer<'static, KeyPair>);

impl Ca {
    fn new() -> Ca {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-only hostkeeper CA");
        let key = KeyPair::generate().expect("a CA key");
        Ca(CertifiedIssuer::self_signed(params, key).expect("the CA"))
    }

    /// The host certificate of `host` and its key, as PEM, in the shape hostkeeper issues.
    fn host(&self, host: &str) -> (String, String) {
        let key = KeyPair::generate().expect("a host key");
        let mut params = CertificateParams::new(vec![host.to_owned()]).expect("host params");
        let uri = Ia5String::try_from(HOST_ROLE_URI.to_owned()).expect("an IA5 URI");
        params.subject_alt_names.push(SanType::URI(uri));
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, format!("host:{host}"));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(SerialNumber::from(1u64));
        params.not_before = date_time_ymd(2026, 9, 1);
        params.not_after = date_time_ymd(3000, 1, 1);
        let cert = params
            .signed_by(&key, &self.0)
            .expect("the host certificate");
        (cert.pem(), key.serialize_pem())
    }
}

/// The lease server, running on a runtime of its own.
pub struct LeaseServer {
    running: Option<Running>,
    runtime: Runtime,
    pump: Option<Pump>,
    url: String,
    /// The server's own certificate, which the agent and the test trust.
    server_cert: PathBuf,
    client_token: PathBuf,
}

impl LeaseServer {
    /// A server on a first boot over the files of `dir`, trusting `agent_ca` for agents.
    fn start(dir: &Path, agent_ca: &str) -> LeaseServer {
        let names = vec!["127.0.0.1".to_owned(), "localhost".to_owned()];
        let made = rcgen::generate_simple_self_signed(names).expect("the server's certificate");
        let write = |name: &str, contents: &str, mode: u32| {
            let path = dir.join(name);
            put(&path, contents, mode);
            path
        };
        let server_cert = write("server-cert.pem", &made.cert.pem(), 0o644);
        let server_key = write("server-key.pem", &made.signing_key.serialize_pem(), 0o600);
        let ca = write("agent-ca.pem", agent_ca, 0o644);
        let client_token = write("client-token", CLIENT_TOKEN, 0o600);
        let join = write("join-token", JOIN, 0o600);
        let pool = write("agent-pool.toml", POOL_TOML, 0o644);
        let text = |path: &Path| path.to_str().expect("a UTF-8 path").to_owned();
        let argv = [
            "js-lease-server".to_owned(),
            "--listen".to_owned(),
            "127.0.0.1:0".to_owned(),
            "--boot".to_owned(),
            "initialise".to_owned(),
            "--state-file".to_owned(),
            text(&dir.join("leases.toml")),
            "--token-file".to_owned(),
            text(&client_token),
            "--agent-token-file".to_owned(),
            text(&join),
            "--agent-pool-config".to_owned(),
            text(&pool),
            "--tls-cert".to_owned(),
            text(&server_cert),
            "--tls-key".to_owned(),
            text(&server_key),
            "--agent-client-ca".to_owned(),
            text(&ca),
        ];
        let cli = Cli::try_parse_from(argv).expect("the server's flags");
        let config = Config::from_cli(cli).expect("the server's configuration");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the host's clock is after 1970");
        let millis = u64::try_from(now.as_millis()).expect("the host's time in milliseconds");
        let clock = ManualClock::new(Timestamp::from_millis(millis));
        let (ticks, ticker) = channel_ticker();
        let runtime = Runtime::new().expect("a runtime");
        let running = runtime
            .block_on(start(config, SharedClock::new(clock.clone()), ticker))
            .expect("the lease server starts");
        let url = format!("https://{}", running.addr());
        let pump = Pump::start(clock, ticks, runtime.handle().clone());
        LeaseServer {
            running: Some(running),
            runtime,
            pump: Some(pump),
            url,
            server_cert,
            client_token,
        }
    }

    /// The address the flags file's `--server` names.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The entry of `host` in `GET /v1/hosts`, read with the client token, as JSON
    /// (`/v1/hosts/<host>` is the agent's read of itself, by its own credential).
    pub fn host(&self, host: &str) -> Value {
        let client = Client::connect(
            &self.url,
            &self.client_token,
            Some(&self.server_cert),
            Duration::from_secs(10),
        )
        .expect("a client of the server");
        let reply = client.get("/v1/hosts").expect("the server answers");
        assert_eq!(reply.status, 200, "{}", reply.body);
        let listing = reply.body;
        let found = listing["hosts"]
            .as_array()
            .and_then(|hosts| hosts.iter().find(|entry| entry["id"] == host));
        found
            .unwrap_or_else(|| panic!("{host} is not listed: {listing}"))
            .clone()
    }
}

impl LeaseServer {
    /// `GET /v1/hosts/<host>` as the agent reads itself (the route the hook's verify reads): by
    /// the host certificate the Studio holds, which the server takes the host's id from.
    pub fn host_as_itself(&self, host: &str) -> Value {
        let identity = HostIdentity::new(
            Path::new(IDENTITY_DIR).join("host.crt"),
            Path::new(IDENTITY_DIR).join("host.key"),
        );
        let client = Client::connect_with_identity(
            &self.url,
            &identity,
            Some(&self.server_cert),
            Duration::from_secs(10),
        )
        .expect("a client of the server by the host certificate");
        let reply = client
            .get(&format!("/v1/hosts/{host}"))
            .expect("the server answers");
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.body
    }
}

impl LeaseServer {
    /// The instant the session `host` is in began (`session_started_ms` of the agent's reading of
    /// itself), `None` when the server shows it in no session.
    pub fn session_started(&self, host: &str) -> Option<u64> {
        self.host_as_itself(host)["session_started_ms"].as_u64()
    }
}

/// How often [`Pump`] moves the server's clock and ticks it.
const PUMP_EVERY: Duration = Duration::from_millis(250);

/// Moves the lease server's clock on with the real time and ticks the server each
/// [`PUMP_EVERY`], from a thread of its own, until it is dropped. Nothing is judged by how long
/// anything takes: it only lets the server's time pass as a real server's does, so that a
/// session that has stopped beating ends.
struct Pump {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Pump {
    fn start(clock: ManualClock, ticks: TickHandle, runtime: tokio::runtime::Handle) -> Pump {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut last = std::time::Instant::now();
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                stopped.recv_timeout(PUMP_EVERY)
            {
                let now = std::time::Instant::now();
                clock.advance(now - last);
                last = now;
                runtime.block_on(ticks.step());
            }
        });
        Pump {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LeaseServer {
    fn drop(&mut self) {
        // The pump first: it ticks the server on its runtime.
        drop(self.pump.take());
        if let Some(running) = self.running.take() {
            let _ = self.runtime.block_on(running.shutdown());
        }
    }
}

/// Writes what a Studio holds before the install, starts the lease server over `dir`, and
/// returns it.
pub fn studio(dir: &Path) -> LeaseServer {
    let ca = Ca::new();
    let server = LeaseServer::start(dir, &ca.pem());
    put(
        Path::new(FACTS_FILE),
        &format!("{{\"schema\":1,\"host_id\":\"{HOST_ID}\",\"host_group\":\"{POOL}\"}}\n"),
        0o644,
    );
    let (cert, key) = ca.host(HOST_ID);
    DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(IDENTITY_DIR)
        .expect("the identity directory");
    fs::set_permissions(IDENTITY_DIR, fs::Permissions::from_mode(0o750)).expect("chmod identity");
    put(&Path::new(IDENTITY_DIR).join("host.crt"), &cert, 0o644);
    put(&Path::new(IDENTITY_DIR).join("host.key"), &key, 0o640);
    let server_cert = fs::read_to_string(&server.server_cert).expect("the server's certificate");
    put(Path::new(CA_FILE), &server_cert, 0o644);
    put(Path::new(FLAGS_FILE), &flags(server.url()), 0o644);
    server
}

/// The Mac config package's flags file, with the lease server at `server`.
fn flags(server: &str) -> String {
    [
        "# js-agent flags of the HostGroup mac-build, as the Mac config package installs them.",
        &format!("--server={server}"),
        "--pool=mac-build",
        "--host-id-from-facts=/var/lib/hostkeeper/facts.json",
        "--ca-cert=/etc/js-agent/ca.pem",
        "--host-cert=/var/lib/hostkeeper/identity/host.crt",
        "--host-key=/var/lib/hostkeeper/identity/host.key",
        "# The native runtime's account pool: the agent does not start without it.",
        "--native-accounts=/var/db/js-agent/accounts",
        "--native-account-range=700-799",
        "--native-account-ready=1",
        "# The native runtime's network.",
        "--native-network=on",
        "--native-pf-uid-start=600",
        "--native-pf-uid-count=100",
        "--native-pf-endpoint=192.0.2.1:9300",
        "--native-pf-endpoint=192.0.2.2:9300",
        "",
    ]
    .join("\n")
}

impl Ca {
    /// The CA's certificate as PEM.
    fn pem(&self) -> String {
        self.0.pem()
    }
}
