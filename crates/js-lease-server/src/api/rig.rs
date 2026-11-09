//! The rig of the route tests: the server's routes called without a socket over a registry
//! on a manual clock, with one registered host, `a`. Nothing here sleeps.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::to_bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_registry::{Credential, PoolConfig, Registry, SequentialSessions, WholeNodeConfig};
use js_testkit::dir::TestDir;
use js_types::{Capabilities, ProviderId, ResourceClass, Resources, Shape};
use js_types::{ManualClock, Timestamp};
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::agents::Agents;
use crate::clock::SharedClock;
use crate::config::parse_token;
use crate::logs::LogGrants;
use js_sched_placer::place::SchedPlacer;

pub(super) const T0: u64 = 1_000_000;

fn caps() -> Capabilities {
    Capabilities::from([("os".to_string(), "macos".to_string())])
}

/// A server over one registered host, `a`, in a pool of the whole-node class. The registry is
/// set up as the server sets it up: it books each host's whole memory and js-sched's engine,
/// on the rig's clock, keeps the whole-node queue. The pool's shape and the host name an
/// `arch`, so the engine can describe the host.
pub(super) struct Rig {
    pub(super) shared: Arc<Shared>,
    /// The scratch directory of the server's files, removed with the rig.
    _dir: TestDir,
    pub(super) registry: Registry,
}

impl Rig {
    pub(super) async fn new(name: &str) -> Self {
        Self::build(name, false, None).await
    }

    /// A rig whose server serves the status routes (to private peers).
    pub(super) async fn serving_status(name: &str) -> Self {
        Self::build(name, true, None).await
    }

    /// A rig whose server serves the status routes (to private peers), with the counters of
    /// the host credentials of live logs that `logs` holds.
    pub(super) async fn serving_status_with_logs(name: &str, logs: Arc<LogGrants>) -> Self {
        Self::build(name, true, Some(logs)).await
    }

    /// A new empty directory for the rig of the test `name`, unique to this process.
    fn dir(name: &str) -> TestDir {
        TestDir::under(std::env::temp_dir(), &format!("rig_unit-{name}"))
    }

    async fn build(name: &str, status: bool, logs: Option<Arc<LogGrants>>) -> Self {
        let dir = Self::dir(name);
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let shared_clock = SharedClock::new(clock.clone());
        let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
        let mut host_caps = caps();
        host_caps.insert("arch".to_string(), "arm64".to_string());
        let shape = Shape::new(resources, host_caps.clone());
        let pool = PoolConfig::new(
            ProviderId::new("builders").unwrap(),
            ResourceClass::whole_node(),
            shape,
            1,
        );
        let pool = pool.with_timing(1_000, 7_200_000).unwrap();
        let whole = WholeNodeConfig {
            drain_deadline_ms: Some(30_000),
            regain_grace_ms: 600_000,
            ..WholeNodeConfig::dedicated(1)
        };
        let pool = pool.with_whole_node(whole).unwrap();
        let pool = pool.booking_whole_memory();
        let placer = SchedPlacer::new(js_sched_placer::engine::Settings::default(), clock.clone());
        let registry = Registry::with_sessions(
            pool,
            clock.clone(),
            SequentialSessions::new(),
            Arc::new(placer),
        );
        let hello = Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            host_id: "a".into(),
            pool: "builders".into(),
            agent_version: "0.1.0".into(),
            instance: None,
            shape: WireShape {
                cpu_millis: 8000,
                memory_bytes: 16 << 30,
                gpus: 0,
                disk_bytes: 0,
                gpu_class: None,
                max_leases: None,
                accelerator: None,
            },
            capabilities: host_caps,
            gpu_devices: Vec::new(),
            labels: Default::default(),
            phase: Phase::Serving,
            held: Vec::new(),
            accepts_host_credential: false,
            identity: None,
        };
        let agents = Agents::over(registry.clone(), &shared_clock, &dir);
        let token = parse_token(&[b'c'; 40]).unwrap();
        let shared = Shared::new(token, Some(Arc::new(agents)));
        registry.hello(&hello, &Credential::of_token("t")).unwrap();
        let served = status.then(|| {
            let status = Status::new(None, None, Vec::new(), shared_clock.clone());
            Arc::new(status.with_logs(logs))
        });
        let shared = Arc::new(shared.with_status(served));
        Self {
            shared,
            _dir: dir,
            registry,
        }
    }

    pub(super) async fn hosts(&self) -> String {
        text(hosts(State(self.shared.clone())).await).await.1
    }
}

async fn text(reply: Response) -> (StatusCode, String) {
    let status = reply.status();
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Asserts the host listing with a draining host: `mark` shows on the host that was drained,
/// the body is exactly the golden text, and a host nobody drained shows no `mark`. It catches
/// a key renamed, and a mark on a host that is not set aside.
#[tokio::test]
async fn the_host_listing_shows_the_mark() {
    let rig = Rig::new("listing").await;
    let plain = rig.hosts().await;
    for key in ["mark", "reserved_for", "drain_deadline_ms"] {
        assert!(!plain.contains(key), "{plain}");
    }
    rig.registry
        .drain(&js_types::HostId::new("a").unwrap())
        .unwrap();
    let body = rig.hosts().await;
    assert_eq!(body, GOLDEN, "{body}");
}

pub(super) const GOLDEN: &str = r#"{"hosts":[{"id":"a","source":"registered","state":"live","lost":false,"phase":"serving","shape":{"cpu_millis":4000,"memory_bytes":8589934592,"gpus":0},"memory":{"configured_bytes":8589934592,"schedulable_percent":100,"bookable_bytes":8589934592},"capabilities":{"arch":"arm64","os":"macos"},"labels":{},"agent_version":"0.1.0","releasing":false,"mark":"draining"}]}"#;

/// A writer the log lines of a test go to.
#[derive(Clone, Default)]
pub(crate) struct Lines(Arc<Mutex<Vec<u8>>>);

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

/// Installs, once per process, a global subscriber that takes every line and writes it
/// nowhere; when another test installed a global subscriber first, that one stays and serves
/// the same end. A test that captures lines through a thread-scoped subscriber calls it before it
/// installs its own. While tracing knows of at most one subscriber, it decides whether a log
/// line is wanted from the default subscriber of whichever thread reaches the line first,
/// and keeps that answer: a thread with no subscriber, such as another test's or a blocking
/// task it spawned, settles the line as never wanted, and a capturing test that reaches the
/// line later never sees it. With this subscriber always known, every answer is worked out
/// from all the live subscribers, the capturing test's among them.
pub(crate) fn process_sink() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(io::sink)
            .try_init();
    });
}

impl Lines {
    /// The lines written since the last take.
    pub(crate) fn take(&self) -> String {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(std::mem::take(&mut *held)).unwrap()
    }
}

/// Asserts that a writer of `Lines` keeps what is written and flushed to any of its clones, and
/// that a take empties it. It catches a clone that writes to a buffer of its own and a take
/// that leaves the text behind.
#[test]
fn lines_written_through_a_clone_are_taken_once() {
    use io::Write as _;
    let lines = Lines::default();
    let mut writer = lines.make_writer();
    writer.write_all(b"one ").unwrap();
    writer.write_all(b"two").unwrap();
    writer.flush().unwrap();
    assert_eq!(lines.take(), "one two");
    assert_eq!(lines.take(), "");
}
