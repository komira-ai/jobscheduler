//! The marks as the routes show them: `GET /v1/hosts` and the tick's log line. The
//! server's routes are called without a socket over a registry on a manual
//! clock; the pool serves a share class and the whole-node class, which this server's
//! pool files do not yet (a mixed pool), so a reservation can be made on a busy host.
//! Nothing here sleeps.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_lease::{Boot, LeaseTable, ManualClock, Settings, Timestamp, read_bookings};
use js_provider::{Pool, StartSpec};
use js_registry::{
    Credential, PoolConfig, Registry, SequentialSessions, WholeCounts, WholeNodeConfig,
};
use js_types::{
    Admission, Capabilities, ClassId, ProviderId, Request, RequestId, ResourceClass, Resources,
    Shape,
};
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::adapt::BoxedPool;
use crate::agents::Agents;
use crate::config::parse_token;
use crate::logs::LogGrants;
use js_sched_placer::place::SchedPlacer;

pub(super) const T0: u64 = 1_000_000;

fn share_class() -> ResourceClass {
    ResourceClass::new(ClassId::new("share").unwrap(), Admission::BookAll, 0)
}

fn caps() -> Capabilities {
    Capabilities::from([("os".to_string(), "macos".to_string())])
}

fn request(class: &str, wall_s: u64) -> Request {
    Request {
        class: ClassId::new(class).unwrap(),
        resources: Resources::new(1000, 1, 0, None).unwrap(),
        capabilities: caps(),
        max_wall: Duration::from_secs(wall_s),
        labels: Default::default(),
        validation_run: None,
        probe_host: None,
    }
}

/// A server over one registered host, `a`, in a pool whose reservations are due thirty
/// seconds after they are made (a deadline that gives nothing up) and which keeps a request
/// that is not presented again on its wait list for ten minutes.
pub(super) struct Rig {
    pub(super) shared: Arc<Shared>,
    name: String,
    clock: ManualClock,
    asker: js_registry::RegisteredProvider,
    pub(super) registry: Registry,
    /// Whether the registry's queue is decided by js-sched's engine (`--placement
    /// js-sched`), as [`Rig::sched`] builds it.
    sched: bool,
    /// The epoch and session of the host's last hello, for its heartbeats ([`Rig::beat`]).
    agent: Option<(u64, String)>,
}

/// How a rig starts: over an empty directory, or over the directory of an earlier rig, on
/// its clock, with `--boot resume` as the program does after a restart.
enum Start {
    Fresh,
    Again {
        clock: ManualClock,
        /// Whether the host `a` says hello again, holding the lease it held.
        host_back: bool,
    },
}

impl Rig {
    pub(super) async fn new(name: &str) -> Self {
        Self::build(name, false, Start::Fresh, None, false).await
    }

    /// A rig whose registry is set up as the server sets it up for `--placement js-sched`:
    /// it books each host's whole memory and js-sched's engine, on the rig's clock, keeps its
    /// whole-node queue. The pool's shape and the host name an `arch`, so the engine can
    /// describe the host.
    pub(super) async fn sched(name: &str) -> Self {
        Self::build(name, false, Start::Fresh, None, true).await
    }

    /// A rig whose server serves the status routes (to private peers).
    pub(super) async fn serving_status(name: &str) -> Self {
        Self::build(name, true, Start::Fresh, None, false).await
    }

    /// A rig whose server serves the status routes (to private peers), with the counters of
    /// the host credentials of live logs that `logs` holds.
    pub(super) async fn serving_status_with_logs(name: &str, logs: Arc<LogGrants>) -> Self {
        Self::build(name, true, Start::Fresh, Some(logs), false).await
    }

    /// The server started again over the files of this one, on the same clock, with
    /// `--boot resume`: a new registry and a new table, nothing carried over but the lease
    /// file. With `host_back` the host `a` says hello again holding the lease it held;
    /// without it the host never returns.
    pub(super) async fn restart(self, host_back: bool) -> Self {
        let (name, clock, sched) = (self.name.clone(), self.clock.clone(), self.sched);
        drop(self);
        Self::build(&name, false, Start::Again { clock, host_back }, None, sched).await
    }

    /// A server resumed over a lease file whose text is `text`, on a fresh clock, with
    /// the host `a` back.
    pub(super) async fn over_file(name: &str, text: &str) -> Self {
        Self::over_file_in(name, text, false).await
    }

    /// [`Rig::over_file`], under `--placement js-sched` when `sched` ([`Rig::sched`]).
    pub(super) async fn over_file_in(name: &str, text: &str, sched: bool) -> Self {
        let dir = Self::dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("leases.toml"), text).unwrap();
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let start = Start::Again {
            clock,
            host_back: true,
        };
        Self::build(name, false, start, None, sched).await
    }

    fn dir(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/whole_view_unit")
            .join(name)
    }

    async fn build(
        name: &str,
        status: bool,
        start: Start,
        logs: Option<Arc<LogGrants>>,
        sched: bool,
    ) -> Self {
        let dir = Self::dir(name);
        let (clock, boot, host_back) = match start {
            Start::Fresh => {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let clock = ManualClock::new(Timestamp::from_millis(T0));
                (clock, Boot::Initialise, true)
            }
            Start::Again { clock, host_back } => (clock, Boot::Resume, host_back),
        };
        let shared_clock = SharedClock::new(clock.clone());
        let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
        let mut host_caps = caps();
        if sched {
            host_caps.insert("arch".to_string(), "arm64".to_string());
        }
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
        let pool = pool.with_whole_node(whole.clone()).unwrap();
        let pool = if sched {
            pool.booking_whole_memory()
        } else {
            pool
        };
        let registry = Registry::with_sessions(pool, clock.clone(), SequentialSessions::new());
        if sched {
            let placer =
                SchedPlacer::new(js_sched_placer::engine::Settings::default(), clock.clone());
            registry.set_placer(Arc::new(placer));
        }
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
        let classes = vec![share_class(), ResourceClass::whole_node()];
        let mut asker = registry.provider().with_classes(classes.clone());
        let provider = registry.provider().with_classes(classes.clone());
        let file = dir.join("leases.toml");
        // After a restart the host that held a lease says hello before the table is
        // opened, and the lease it held is booked on it again, as an ordinary one (a
        // `seat` would book it as a committed whole-node lease, the class of the pool,
        // which with a reservation on the same host is over the pool's ceiling).
        let early = host_back && boot == Boot::Resume;
        let mut agent = None;
        if early {
            let welcome = registry.hello(&hello, &Credential::of_token("t")).unwrap();
            agent = Some((welcome.epoch, welcome.session));
            for booking in read_bookings(&file).unwrap() {
                let spec = StartSpec::new(booking.lease, request("share", 600), &classes);
                asker.start(spec.unwrap());
            }
        }
        let settings = Settings::new();
        let settings = crate::reservations::with_restart_grace(settings, Some(&whole));
        let table = LeaseTable::open(
            BoxedPool::new(Box::new(provider)),
            shared_clock.clone(),
            settings,
            file,
            boot,
        )
        .unwrap();
        let agents = Agents::over(registry.clone(), &shared_clock, &dir);
        let token = parse_token(&[b'c'; 40]).unwrap();
        let shared = Shared::new(table, token, Some(Arc::new(agents)));
        // The agent's hello comes after the server is up, as it does in the program.
        if host_back && !early {
            let welcome = registry.hello(&hello, &Credential::of_token("t")).unwrap();
            agent = Some((welcome.epoch, welcome.session));
        }
        let served = status.then(|| {
            let status = Status::new(None, None, Vec::new(), shared_clock.clone());
            Arc::new(status.with_logs(logs))
        });
        let shared = Arc::new(shared.with_status(served));
        Self {
            shared,
            name: name.to_string(),
            clock,
            asker,
            registry,
            sched,
            agent,
        }
    }

    /// The host `a` beats, carrying nothing new, as its agent does every few seconds; a host
    /// that has not said hello does not.
    pub(super) fn beat(&self) {
        let Some((epoch, session)) = self.agent.clone() else {
            return;
        };
        let beat = js_agent_wire::Heartbeat {
            host_id: "a".into(),
            epoch,
            session,
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
            log_grant_state: None,
        };
        self.registry
            .heartbeat(&beat, &Credential::of_token("t"))
            .unwrap();
    }

    /// Moves the clock `seconds` on, five at a time; at each step the host beats and the
    /// consumer polls each of `asks`, as they do in the program.
    pub(super) fn pass(&mut self, seconds: u64, asks: &[&str]) {
        let mut left = seconds;
        while left > 0 {
            let step = left.min(5);
            left -= step;
            self.advance(step);
            self.beat();
            for name in asks {
                self.ask(name);
            }
        }
    }

    /// The lease file this rig's server writes.
    pub(super) fn file(&self) -> std::path::PathBuf {
        Self::dir(&self.name).join("leases.toml")
    }

    pub(super) fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
    }

    /// Takes a share lease on the host; its id.
    pub(super) async fn lease(&self) -> String {
        let mut table = self.shared.table.lock().await;
        table.acquire(request("share", 600)).unwrap().id.to_string()
    }

    /// Polls whole-node request `name`, as its consumer would.
    pub(super) fn ask(&mut self, name: &str) {
        let classes = [ResourceClass::whole_node()];
        let spec = StartSpec::new(
            js_types::LeaseId::new(9, 1),
            request("whole-node", 600),
            &classes,
        );
        let spec = spec.unwrap().with_request_id(RequestId::new(name).unwrap());
        self.asker.start(spec);
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

/// Asserts the host listing with a reserved host: `mark`, the request it is for and the
/// deadline show on the reserved host, the body is exactly the golden text, and a host
/// nobody reserved shows none of the three keys. It catches a key renamed, a deadline
/// in seconds, and a mark on a host that is not set aside.
#[tokio::test]
async fn the_host_listing_shows_the_mark_and_its_deadline() {
    let mut rig = Rig::new("listing").await;
    let plain = rig.hosts().await;
    for key in ["mark", "reserved_for", "drain_deadline_ms"] {
        assert!(!plain.contains(key), "{plain}");
    }
    rig.lease().await;
    rig.ask("r1");
    let body = rig.hosts().await;
    assert_eq!(body, GOLDEN, "{body}");
}

/// Asserts that a host a whole-node lease is committed on lists as `held`, with neither
/// the request nor a deadline. It catches a held host listed with no mark and one that
/// still names a request.
#[tokio::test]
async fn a_committed_host_lists_as_held() {
    let mut rig = Rig::new("held").await;
    rig.ask("r1");
    let body = rig.hosts().await;
    assert!(
        body.ends_with(r#""releasing":false,"mark":"held"}]}"#),
        "{body}"
    );
    assert!(!body.contains("reserved_for") && !body.contains("drain_deadline_ms"));
}

pub(super) const GOLDEN: &str = r#"{"hosts":[{"id":"a","source":"registered","state":"live","lost":false,"phase":"serving","lease":"1-1","shape":{"cpu_millis":4000,"memory_bytes":8589934592,"gpus":0},"memory":{"configured_bytes":8589934592,"schedulable_percent":100,"bookable_bytes":8589934592},"capabilities":{"os":"macos"},"labels":{},"agent_version":"0.1.0","releasing":false,"mark":"reserved","reserved_for":"r1","drain_deadline_ms":1030000}]}"#;

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

/// Asserts the tick's line: when a tick changes the reserved, held or queued count the
/// line carries the registry's counts, a tick that changes nothing logs nothing, and a
/// tick that changes a lease (here a lease reaching its wall end) logs the line with the
/// counts it has. It catches counters that are not the registry's, a count that is logged only when a
/// lease changes, and a line logged at every tick.
#[tokio::test]
async fn the_tick_logs_the_counts_of_reserved_held_and_queued() {
    let lines = Lines::default();
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut rig = Rig::new("tick").await;
    rig.lease().await;
    rig.shared.tick().await;
    assert_eq!(lines.take(), "");

    rig.ask("r1");
    assert_eq!(
        rig.registry.whole_counts(),
        WholeCounts {
            reserved: 1,
            held: 0,
            queued: 1
        }
    );
    rig.shared.tick().await;
    let line = lines.take();
    assert!(line.contains("reserved=1 held=0 queued=1"), "{line}");
    assert!(line.contains("tick changed leases"), "{line}");

    rig.shared.tick().await;
    assert_eq!(lines.take(), "");

    rig.advance(601);
    rig.shared.tick().await;
    let line = lines.take();
    assert!(line.contains("expired=1"), "{line}");
    let counts = rig.registry.whole_counts();
    let said = format!(
        "reserved={} held={} queued={}",
        counts.reserved, counts.held, counts.queued
    );
    assert!(line.contains(&said), "{line} vs {said}");
}

/// Asserts that the tick gives nothing up when a reservation reaches its drain deadline
/// with the host still busy: no consumer polls after the deadline, yet after one tick the
/// counts still show the reservation and the host listing still shows the mark, with the
/// deadline it had. It catches a tick that drops a mark at its deadline.
#[tokio::test]
async fn the_tick_keeps_a_reservation_past_its_deadline() {
    // The tick logs; its own subscriber keeps that line out of the other test's.
    process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Lines::default())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut rig = Rig::new("tick_keeps").await;
    rig.lease().await;
    rig.ask("r1");
    assert_eq!(rig.registry.whole_counts().reserved, 1);
    rig.advance(31);
    rig.shared.tick().await;
    assert_eq!(rig.registry.whole_counts().reserved, 1);
    let body = rig.hosts().await;
    assert!(body.contains("\"mark\":\"reserved\""), "{body}");
    assert!(body.contains("\"reserved_for\":\"r1\""), "{body}");
    assert!(body.contains("\"drain_deadline_ms\":1030000"), "{body}");
}
