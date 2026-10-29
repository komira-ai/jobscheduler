//! Shared fixtures: a host, a plan, a Tailscale mode over a fake, and scratch files.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use js_elastic::{AcquireId, Artifact, BootPlan, BootStep, HostName, SchedulerId, Secret};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::SharedClock;
use js_lease_server::network::{
    API_TIMEOUT, LaunchCtx, OAuthClient, TailscaleApi, TailscaleMode, TailscaleSettings,
};

use crate::fake::{CLIENT_SECRET, Fake};

/// The key lifetime of a normal launch.
pub const TEN_MINUTES: Duration = Duration::from_secs(600);

/// The tag the tests launch with.
pub const TAG: &str = "tag:js-agent";

/// The host every test launches.
pub fn host() -> HostName {
    HostName::derive(
        &SchedulerId::new("sched").unwrap(),
        &AcquireId::new("acquire-1").unwrap(),
    )
}

/// A launch of `host` for the pool `gpu`.
pub fn ctx(host: &HostName, valid_for: Duration) -> LaunchCtx<'_> {
    LaunchCtx {
        pool: "gpu",
        host,
        valid_for,
    }
}

/// A clock and the shared form of it the server's code takes.
pub fn clock() -> (ManualClock, SharedClock) {
    let manual = ManualClock::new(Timestamp::from_millis(1_000_000));
    let shared = SharedClock::new(manual.clone());
    (manual, shared)
}

/// The OAuth client the tests use.
pub fn client() -> OAuthClient {
    OAuthClient::new("client-id-1", Secret::new(CLIENT_SECRET.to_owned()))
}

/// An API client for `url`.
pub fn api_at(url: &str, shared: SharedClock, timeout: Duration) -> TailscaleApi {
    TailscaleApi::new(url, "-", client(), shared, timeout).unwrap()
}

/// The Tailscale build the tests fetch.
pub fn binary() -> Artifact {
    Artifact {
        url: "https://pkgs.example.org/tailscale_static_amd64.tgz".to_owned(),
        sha256: [0x5a; 32],
    }
}

/// A Tailscale mode over `fake`.
pub fn mode(fake: &Fake, shared: SharedClock) -> TailscaleMode {
    let settings = TailscaleSettings {
        tag: TAG.to_owned(),
        binary: binary(),
    };
    TailscaleMode::new(api_at(&fake.url(), shared, API_TIMEOUT), settings).unwrap()
}

/// A plan with no network steps: the agent, the scheduler, the pool and the host.
pub fn plan(host: &HostName) -> BootPlan {
    BootPlan {
        agent: Artifact {
            url: "https://sched.example.org:7301/v1/bootstrap/agent/x86_64".to_owned(),
            sha256: [0xab; 32],
        },
        scheduler_url: "https://sched.example.org:7301".to_owned(),
        pool: "gpu".to_owned(),
        host: host.clone(),
        host_prep: vec![BootStep::Run(vec![
            "apt-get".to_owned(),
            "install".to_owned(),
            "-y".to_owned(),
            "curl".to_owned(),
        ])],
        network: Vec::new(),
    }
}

/// A directory of files for one test.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// An empty directory named `name`.
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-lease-server-network")
            .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    /// Writes `bytes` to the file `name` with `mode` and returns its path.
    pub fn write(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// An API client for a mode whose settings are the test's subject. It never calls.
pub fn unused_api() -> TailscaleApi {
    api_at("https://tailscale.example.org", clock().1, API_TIMEOUT)
}
