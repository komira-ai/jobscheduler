//! The marks as the routes show them: the renewal answer, `GET /v1/hosts` and the tick's
//! log line. The server's routes are called without a socket over a registry on a manual
//! clock; the pool serves a share class and the whole-node class, which this server's
//! pool files do not yet (a mixed pool), so a reservation can be made on a busy host.
//! Nothing here sleeps.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_lease::{Boot, LeaseTable, ManualClock, Settings, Timestamp, read_bookings};
use js_provider::{Provider, StartSpec};
use js_registry::{
    Credential, PoolConfig, Registry, SequentialSessions, WholeCounts, WholeNodeConfig,
};
use js_types::{
    Admission, Capabilities, ClassId, ProviderId, Request, RequestId, ResourceClass, Resources,
    Shape,
};
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::adapt::BoxedProvider;
use crate::agents::Agents;
use crate::config::parse_token;

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
    }
}

/// A server over one registered host, `a`, in a pool that gives up a reserved host after
/// thirty seconds and keeps a request that is not presented again on its wait list for ten
/// minutes, so only the drain deadline ends a reservation here.
pub(super) struct Rig {
    pub(super) shared: Arc<Shared>,
    name: String,
    clock: ManualClock,
    asker: js_registry::RegisteredProvider,
    pub(super) registry: Registry,
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
        Self::build(name, false, Start::Fresh).await
    }

    /// A rig whose server serves the status routes (to private peers).
    pub(super) async fn serving_status(name: &str) -> Self {
        Self::build(name, true, Start::Fresh).await
    }

    /// The server started again over the files of this one, on the same clock, with
    /// `--boot resume`: a new registry and a new table, nothing carried over but the lease
    /// file. With `host_back` the host `a` says hello again holding the lease it held;
    /// without it the host never returns.
    pub(super) async fn restart(self, host_back: bool) -> Self {
        let (name, clock) = (self.name.clone(), self.clock.clone());
        drop(self);
        Self::build(&name, false, Start::Again { clock, host_back }).await
    }

    /// A server resumed over a lease file whose text is `text`, on a fresh clock, with
    /// the host `a` back.
    pub(super) async fn over_file(name: &str, text: &str) -> Self {
        let dir = Self::dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("leases.toml"), text).unwrap();
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let start = Start::Again {
            clock,
            host_back: true,
        };
        Self::build(name, false, start).await
    }

    fn dir(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/whole_view_unit")
            .join(name)
    }

    async fn build(name: &str, status: bool, start: Start) -> Self {
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
        let shape = Shape::new(resources, caps());
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
        let registry = Registry::with_sessions(pool, clock.clone(), SequentialSessions::new());
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
            },
            capabilities: caps(),
            gpu_devices: Vec::new(),
            labels: Default::default(),
            phase: Phase::Serving,
            held: Vec::new(),
            accepts_host_credential: false,
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
        if early {
            registry.hello(&hello, &Credential::of_token("t")).unwrap();
            for booking in read_bookings(&file).unwrap() {
                let spec = StartSpec::new(booking.lease, request("share", 600), &classes);
                asker.start(spec.unwrap());
            }
        }
        let settings = Settings::new(Duration::from_secs(300), Duration::from_secs(3600)).unwrap();
        let settings = crate::reservations::with_restart_grace(settings, Some(&whole));
        let table = LeaseTable::open(
            BoxedProvider(Box::new(provider)),
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
            registry.hello(&hello, &Credential::of_token("t")).unwrap();
        }
        let served = status.then(|| {
            Arc::new(Status::new(
                None,
                None,
                None,
                Vec::new(),
                shared_clock.clone(),
            ))
        });
        let shared = Arc::new(shared.with_status(served));
        Self {
            shared,
            name: name.to_string(),
            clock,
            asker,
            registry,
        }
    }

    pub(super) fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
    }

    /// Takes a share lease on the host; its id.
    pub(super) async fn lease(&self) -> String {
        let mut table = self.shared.table.lock().await;
        table
            .acquire(request("share", 600), None)
            .unwrap()
            .id
            .to_string()
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

    async fn renew(&self, id: &str) -> (StatusCode, String) {
        let reply = renew(State(self.shared.clone()), Path(id.into()), Bytes::new()).await;
        text(reply.unwrap()).await
    }

    async fn hosts(&self) -> String {
        text(hosts(State(self.shared.clone())).await).await.1
    }
}

async fn text(reply: Response) -> (StatusCode, String) {
    let status = reply.status();
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Asserts the renewal answer over the life of a reservation. A lease on a host that is
/// not reserved renews to a body with only the expiry; when a waiting request reserves
/// the host the same lease renews, never refused, to a new expiry and `host_reserved`;
/// when the reservation is given up the field is gone again. It catches a renewal
/// refused for a reservation, a flag that never shows, a flag that stays after the
/// give-up, and an expiry that does not move.
#[tokio::test]
async fn a_renewal_says_when_its_host_is_reserved() {
    let mut rig = Rig::new("renew").await;
    let id = rig.lease().await;
    rig.advance(10);
    let (status, body) = rig.renew(&id).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "{\"expires_at_ms\":1310000}")
    );

    rig.ask("r1");
    assert_eq!(rig.registry.reservations().len(), 1);
    rig.advance(10);
    rig.ask("r1");
    let (status, body) = rig.renew(&id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"expires_at_ms\":1320000,\"host_reserved\":true}");

    // The consumer polls every ten seconds, so its entry stays on the wait list.
    rig.advance(10);
    rig.ask("r1");
    rig.advance(10);
    rig.ask("r1");
    assert!(
        rig.registry.reservations().is_empty(),
        "given up at the deadline"
    );
    let (status, body) = rig.renew(&id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"expires_at_ms\":1340000}");
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

const GOLDEN: &str = r#"{"hosts":[{"id":"a","source":"registered","state":"live","lost":false,"phase":"serving","lease":"1-1","shape":{"cpu_millis":4000,"memory_bytes":8589934592,"gpus":0},"memory":{"configured_bytes":8589934592,"schedulable_percent":100,"bookable_bytes":8589934592},"capabilities":{"os":"macos"},"labels":{},"agent_version":"0.1.0","releasing":false,"mark":"reserved","reserved_for":"r1","drain_deadline_ms":1030000}]}"#;

/// A writer the log lines of a test go to.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

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

impl Lines {
    fn take(&self) -> String {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(std::mem::take(&mut *held)).unwrap()
    }
}

/// Asserts the tick's line: when a tick changes the reserved, held or queued count the
/// line carries the registry's counts, a tick that changes nothing logs nothing, and a
/// tick that changes a lease (here an expiry) logs the line with the counts it has.
/// It catches counters that are not the registry's, a count that is logged only when a
/// lease changes, and a line logged at every tick.
#[tokio::test]
async fn the_tick_logs_the_counts_of_reserved_held_and_queued() {
    let lines = Lines::default();
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

    rig.advance(400);
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

/// Asserts that the tick alone gives up a reservation that reaches its drain deadline
/// with the host still busy: no consumer polls after the deadline, yet after one tick the
/// counts show no reservation and the host listing shows no mark. It catches a tick that
/// does not bring the registry's marks up to the clock, which would leave the host listed
/// as reserved, and counted so, until the next poll.
#[tokio::test]
async fn the_tick_alone_gives_up_a_reservation_at_its_deadline() {
    // The tick logs; its own subscriber keeps that line out of the other test's.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Lines::default())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut rig = Rig::new("tick_giveup").await;
    rig.lease().await;
    rig.ask("r1");
    assert_eq!(rig.registry.whole_counts().reserved, 1);
    rig.advance(31);
    rig.shared.tick().await;
    assert_eq!(rig.registry.whole_counts().reserved, 0);
    let body = rig.hosts().await;
    assert!(!body.contains("\"mark\""), "{body}");
    assert!(!body.contains("reserved_for"), "{body}");
}
