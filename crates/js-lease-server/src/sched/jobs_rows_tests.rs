//! The hosts a pass offers, read from the job store ([`Agents::offered`]) and the bindings the
//! store holds ([`BindingStore::scan_stored`]), over a fake store: every reason a host with a
//! row, a host the registry holds, or both, is or is not offered. Every clock is manual;
//! nothing sleeps.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{HeldRun, HeldState, Phase, Taint};
use js_jobs::interfere::{INJECTED, Interfere};
use js_jobs::{JobId, NodeHello, NodeRow, NodeState, ScaleMark};
use js_registry::Credential;
use js_sched_placer::place::SchedPlacer;
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock as StoreClock, Store, Txn};
use js_testkit::dir::TestDir;
use js_types::cordon::Asker;
use js_types::{CordonHolder, ManualClock, Shape, Timestamp};
use serde_json::json;

use super::tests::{booked, host, on_the_machine, shape};
use crate::agent_api::job_rig::placed;
use crate::agents::{Agents, assemble_placed};
use crate::clock::SharedClock;
use crate::job_store::{ServerIds, ServerJobs};
use crate::sched::engine_rig::{GIB, T0, hello_of, pool};
use crate::sched::rank::STATIC;
use crate::sched::wiring_tests::{JOIN, config_of, scratch};

/// A server over a fake store: its agent side (the registry holds no host yet), its placer,
/// the store and the job tables over it.
struct World {
    agents: Agents,
    placer: Arc<SchedPlacer>,
    store: Arc<Interfere<Fake>>,
    jobs: Arc<ServerJobs>,
    _dir: TestDir,
}

/// A server named `name` of a pool of up to twelve machines, placing with `js-sched`. It keeps
/// its host bindings in the store when `bindings_in_store`, and then ranks only the hosts of
/// the `static` provider.
fn world(name: &str, bindings_in_store: bool) -> World {
    let dir = scratch(name);
    let config = config_of(
        dir.path(),
        &pool(12, 8000, ""),
        &["--placement", "js-sched"],
    );
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let assembled = assemble_placed(
        config.agents,
        config.usage,
        &clock,
        &config.state_file,
        None,
    );
    let (agents, placer) = assembled.unwrap();
    let agents = Arc::try_unwrap(agents).ok().unwrap();
    let store = Arc::new(Interfere::new(Fake::with_flaw(
        Arc::new(StoreClock::default()),
        Flaw::None,
    )));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = js_jobs::JobStore::new(Arc::new(Arc::clone(&dynamic)), ServerIds::random());
    let jobs = Arc::new(jobs.unwrap());
    let agents = match bindings_in_store {
        true => agents.kept_in(dynamic),
        false => agents,
    };
    let mut agents = agents.with_jobs(Arc::clone(&jobs));
    agents.providers = vec![STATIC];
    World {
        agents,
        placer,
        store,
        jobs,
        _dir: dir,
    }
}

impl World {
    /// `host` says hello to the registry (past the route, so no row is written).
    fn register(&self, host: &str) {
        let hello = hello_of(host, 16_000, 32 * GIB);
        let welcome = self
            .agents
            .registry
            .hello(&hello, &Credential::of_token(JOIN));
        welcome.unwrap();
    }

    /// The row of `host` as another server's hello wrote it: in `pool`, of `shape` when it has
    /// one, announcing `phase`.
    fn row(&self, host_name: &str, pool: &str, shape: Option<Shape>, phase: Phase) -> NodeRow {
        let hello = NodeHello {
            session: None,
            pool: pool.to_owned(),
            claim: shape.clone(),
            shape,
            labels: Default::default(),
            phase,
        };
        self.jobs.hello_node(&host(host_name), &hello).unwrap()
    }

    /// The row of `host` as a hello wrote it, of the usual shape and serving.
    fn serving_row(&self, host_name: &str) -> NodeRow {
        self.row(host_name, "burst", Some(shape()), Phase::Serving)
    }

    /// Stores `value` as the bytes of the binding row of `host`.
    fn bind(&self, host_name: &str, value: &[u8]) {
        let key = format!("hosts/binding/{host_name}");
        let put = self
            .store
            .txn(Txn::new().put(key, value.to_vec(), Cond::Any));
        put.unwrap().unwrap();
    }

    /// Stores a readable binding of `host` that `extra` adds fields to.
    fn bind_with(&self, host_name: &str, extra: serde_json::Value) {
        let mut value = json!({ "version": 1, "credential": "a".repeat(64) });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        self.bind(host_name, &serde_json::to_vec(&value).unwrap());
    }

    /// Runs a pass's read of the hosts at the registry's time.
    fn offered(&self) -> Result<super::super::jobs_rows::Offered, String> {
        let now = Timestamp::from_millis(T0);
        self.agents.offered(&self.jobs, &self.placer, now)
    }
}

/// A taint by the registry's own record.
fn taint() -> Taint {
    Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    }
}

/// Asserts, over a server that keeps bindings in the store, which hosts a read offers and why
/// the others are left out: a host with a row that is open and serving with a clean listing is
/// offered, as is a host the registry holds live that has no row; a row with no booked shape,
/// of a pool this server does not serve, whose phase is not serving, or whose last listing was
/// not clean, is left out; so is a host with a `taint/` row, a `hostmark/` row, or a stored
/// binding that holds a taint; a host of a provider the platform does not list is not
/// offered; and a host the registry holds, with a row, is left out for its own record's taint,
/// scale-down mark, cordon, release under way or unclean listing, and without a row for its
/// taint. A `taint/` row of a host the registry holds is put on the registry's record and
/// returned, once, and not for a record that holds a taint already or a host it does not
/// hold. The hosts heard from are the rows' hosts and the hosts the registry holds live, and a
/// binding that cannot be read is skipped. It catches an offer that drops a host another
/// server took the hello of, one that keeps offering a host a row, a binding or the
/// registry's own record closes, a taint row of a held host that is never carried to the
/// binding, and a pass that stops on one binding it cannot read.
#[test]
fn a_read_offers_the_open_hosts_of_the_rows_and_the_registry_and_no_closed_one() {
    let w = world("rows_offered", true);
    for name in [
        "held-open",
        "held-taint",
        "held-adopted",
        "held-adopted-twice",
        "held-mark",
        "held-cordon",
        "held-unclean",
        "held-norow",
        "held-norow-tainted",
    ] {
        w.register(name);
    }
    let mut busy = hello_of("held-releasing", 16_000, 32 * GIB);
    busy.held = vec![HeldRun {
        run_id: "run-1".to_owned(),
        state: HeldState::Running,
    }];
    let welcome = w.agents.registry.hello(&busy, &Credential::of_token(JOIN));
    welcome.unwrap();
    let registry = &w.agents.registry;
    registry.taint_host(&host("held-taint"), taint());
    registry.taint_host(&host("held-adopted-twice"), taint());
    registry.taint_host(&host("held-norow-tainted"), taint());
    registry.drain(&host("held-mark")).unwrap();
    let lease = Duration::from_secs(600);
    let holder = Asker::Holder(CordonHolder::new("ops-1").unwrap());
    registry
        .cordon(&host("held-cordon"), lease, &holder)
        .unwrap();
    w.agents.note_listing(&host("held-unclean"), false);
    for name in [
        "held-open",
        "held-taint",
        "held-adopted",
        "held-adopted-twice",
        "held-mark",
        "held-cordon",
        "held-releasing",
        "held-unclean",
    ] {
        w.serving_row(name);
    }
    // Rows that no registry of this server holds.
    for name in [
        "far-open",
        "far-bound-taint",
        "far-unranked",
        "far-taint",
        "far-marked",
    ] {
        w.serving_row(name);
    }
    w.row("far-no-shape", "burst", None, Phase::Serving);
    w.row("far-other-pool", "elsewhere", Some(shape()), Phase::Serving);
    w.row("far-draining", "burst", Some(shape()), Phase::Draining);
    let unclean = w.serving_row("far-unclean");
    let state = NodeState {
        phase: Phase::Serving,
        load: None,
        first_listing_pending: false,
        last_listing_clean: false,
        missing_in_a_row: Default::default(),
    };
    let every = Duration::from_secs(3600);
    w.jobs
        .refresh_node(&host("far-unclean"), unclean.record.epoch, &state, every)
        .unwrap();
    // Taints and marks the rows hold.
    placed(
        &w.jobs,
        "job-t1",
        "held-adopted",
        on_the_machine(1),
        booked(1),
    );
    placed(
        &w.jobs,
        "job-t2",
        "held-adopted-twice",
        on_the_machine(1),
        booked(1),
    );
    placed(&w.jobs, "job-t3", "far-taint", on_the_machine(1), booked(1));
    let marked = w
        .jobs
        .mark_host(&host("far-marked"), ScaleMark::Releasing, None);
    assert!(marked.unwrap());
    // Bindings the store holds: open, tainted, naming an instance (of a provider the platform
    // does not list), and two that cannot be read.
    w.bind_with("far-open", json!({}));
    w.bind_with("far-bound-taint", json!({ "taint_reason": "residue" }));
    w.bind_with(
        "far-unranked",
        json!({ "instance_provider": "cloud", "instance_id": "i-1" }),
    );
    w.bind("far-garbage", b"not json");
    w.bind_with("far-bad-credential", json!({ "credential": "short" }));

    // A read that fails at any of the four tables says so, and the pass places nothing.
    for reads_before in 0..4 {
        w.store.fail_read_after(reads_before);
        let error = w.offered().unwrap_err();
        assert!(error.contains(INJECTED), "{reads_before}: {error}");
    }

    let offered = w.offered().unwrap();
    let names = |hosts: &mut dyn Iterator<Item = String>| hosts.collect::<BTreeSet<String>>();
    let open = names(&mut offered.open.keys().map(ToString::to_string));
    assert_eq!(
        open,
        ["far-open", "held-norow", "held-open"]
            .map(String::from)
            .into()
    );
    let heard = names(&mut offered.hello.iter().map(ToString::to_string));
    let expected: BTreeSet<String> = [
        "far-bound-taint",
        "far-draining",
        "far-marked",
        "far-no-shape",
        "far-open",
        "far-other-pool",
        "far-taint",
        "far-unclean",
        "far-unranked",
        "held-adopted",
        "held-adopted-twice",
        "held-cordon",
        "held-mark",
        "held-norow",
        "held-norow-tainted",
        "held-open",
        "held-releasing",
        "held-taint",
        "held-unclean",
    ]
    .map(String::from)
    .into();
    assert_eq!(heard, expected);
    assert_eq!(
        offered.adopted,
        [(host("held-adopted"), JobId::parse("job-t1").unwrap())],
        "only a taint row the registry's record does not hold yet is taken on"
    );
    let views = w.agents.registry.hosts();
    let adopted = views.iter().find(|view| view.id == host("held-adopted"));
    assert_eq!(
        adopted
            .unwrap()
            .taint
            .as_ref()
            .and_then(|t| t.run_id.as_deref()),
        Some("job-t1")
    );
}

/// Asserts that a server that keeps its bindings in files has no stored bindings to scan, and
/// that its read of the hosts still offers a host with a row. It catches a scan that reads
/// the store of a server that has none, or a read that fails when there is nothing stored.
#[test]
fn a_server_with_its_bindings_in_files_scans_no_stored_binding() {
    let w = world("rows_files", false);
    assert!(w.agents.store.scan_stored().unwrap().is_none());
    w.serving_row("far-open");
    let offered = w.offered().unwrap();
    assert_eq!(offered.open.keys().collect::<Vec<_>>(), [&host("far-open")]);
}

/// Asserts that the scan of the stored bindings returns every binding it can read, by host,
/// with its taint, and leaves out the ones it cannot read (not JSON, a credential that is not
/// a credential). It catches a scan that fails whole on one bad value, or one that returns a
/// binding without its taint.
#[test]
fn the_scan_of_stored_bindings_returns_the_readable_ones() {
    let w = world("rows_scan", true);
    w.bind_with("bound", json!({ "taint_reason": "residue" }));
    w.bind_with("plain", json!({}));
    w.bind("garbage", b"not json");
    w.bind_with("bad-credential", json!({ "credential": "short" }));
    let found = w.agents.store.scan_stored().unwrap().unwrap();
    let hosts: Vec<String> = found.keys().map(ToString::to_string).collect();
    assert_eq!(hosts, ["bound", "plain"]);
    assert!(found[&host("bound")].taint().is_some());
    assert!(found[&host("plain")].taint().is_none());
    w.store.fail_read_after(0);
    let error = w.agents.store.scan_stored().unwrap_err();
    assert!(error.to_string().contains(INJECTED), "{error}");
}

/// Asserts that a pass whose read of the store's head fails says so and offers nothing, both
/// when it reads the rows afresh and when a heartbeat's pass asks whether the rows it kept
/// still stand. It catches a failed head read taken as a store that has not moved, which would
/// offer hosts by rows that may be stale.
#[test]
fn a_store_head_that_cannot_be_read_offers_nothing() {
    let w = world("rows_head", true);
    w.store.fail_next_head();
    let error = w.offered().unwrap_err();
    assert!(error.contains(INJECTED), "{error}");
    w.offered().unwrap();
    w.store.fail_next_head();
    let now = Timestamp::from_millis(T0);
    let error = w.agents.offered_after_beat(&w.jobs, &w.placer, now);
    let error = error.unwrap_err();
    assert!(error.contains(INJECTED), "{error}");
}
