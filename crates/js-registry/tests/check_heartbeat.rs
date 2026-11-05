//! `Registry::check_heartbeat`: it answers what `Registry::heartbeat` would refuse and
//! what taint the beat would record, and records nothing. The registry is driven
//! directly, on a manual clock, with hand-built heartbeats.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{
    DEFAULT_GRACE_MS, ExclusiveRefusal, Heartbeat, HeldRun, HeldState, Isolation, JobSpec, Limits,
    NodeLoad, Outcome, Phase, RefusalReason, RunResult, Taint, refusal,
};
use js_provider::{Observed, Pool, StartAck, StartSpec};
use js_registry::{
    Binding, Credential, HeartbeatCheck, HostState, HostView, MIN_LOST_RETENTION_MS, OwnHost,
    PoolConfig, Refusal, RequestRecord, RunState, RunView, TailCounters, WholeCounts,
    WholeNodeConfig,
};
use js_types::{HostId, ProviderId, Request, ResourceClass};
use support::*;

fn report(reason: &str) -> Taint {
    Taint {
        reason: reason.into(),
        run_id: None,
        check: Some("leftovers".into()),
    }
}

/// The taint a refused prepare records on a host (`host-unsafe`).
fn host_unsafe() -> Taint {
    Taint {
        reason: "host-unsafe".into(),
        run_id: None,
        check: None,
    }
}

/// The taint a start refused as `host-tainted` records, naming `run`.
fn tainted_start(run: &str) -> Taint {
    Taint {
        reason: "host-tainted".into(),
        run_id: Some(run.into()),
        check: None,
    }
}

/// A result for `run` that ended as `outcome` with `detail`.
fn result(run: &str, outcome: Outcome, detail: &str) -> RunResult {
    RunResult {
        run_id: run.into(),
        lease_id: "1-1".into(),
        outcome,
        exit_code: None,
        detail: Some(detail.into()),
        log_tail: None,
    }
}

fn exclusive_refusal(lease_id: &str, code: RefusalReason) -> ExclusiveRefusal {
    ExclusiveRefusal {
        lease_id: lease_id.into(),
        code,
        detail: "a finding".into(),
    }
}

/// Everything the registry shows of its hosts: the listing (with each host's load and
/// bookings), the bindings a caller persists, each host's own view (its cordon and
/// session start), the counters, and the runs of lease 1.
type Snapshot = (
    Vec<HostView>,
    BTreeMap<HostId, Binding>,
    Vec<Option<OwnHost>>,
    TailCounters,
    WholeCounts,
    Vec<RunView>,
);

fn snapshot(fx: &Fixture) -> Snapshot {
    let hosts = fx.registry.hosts();
    let own = hosts
        .iter()
        .map(|view| fx.registry.own_host(&view.id))
        .collect();
    (
        hosts,
        fx.registry.bindings(),
        own,
        fx.registry.tail_counters(),
        fx.registry.whole_counts(),
        fx.registry.runs_of(&lease(1)),
    )
}

fn view(fx: &Fixture, name: &str) -> HostView {
    let id = host(name);
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|view| view.id == id).unwrap()
}

/// A heartbeat from host `a` in epoch 1, with no session yet, as `edit` leaves it.
fn edited(edit: impl Fn(&mut Heartbeat)) -> Heartbeat {
    let mut message = heartbeat("a", 1);
    edit(&mut message);
    message
}

/// A pool of whole-node hosts with the default whole-node settings.
fn whole_config() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, ResourceClass::whole_node(), pool_shape(), 4)
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .unwrap()
}

fn whole_spec(sequence: u64) -> StartSpec {
    let request = Request {
        class: ResourceClass::whole_node().id().clone(),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[ResourceClass::whole_node()]).unwrap()
}

/// A whole-node pool with host `a` joined (epoch 1) and lease 1 committed to it, not yet
/// reported ready.
fn committed() -> Fixture {
    let fx = fixture_of(whole_config());
    fx.join("a", "t");
    assert_eq!(fx.provider.clone().start(whole_spec(1)), StartAck::Unknown);
    fx
}

/// What the registry shows of lease 1 and its request, beside the snapshot.
fn whole_state(fx: &Fixture) -> (Snapshot, Observed, Option<RequestRecord>) {
    let request = Request {
        class: ResourceClass::whole_node().id().clone(),
        ..request()
    };
    (
        snapshot(fx),
        fx.provider.clone().observe(&lease(1)),
        fx.registry.request_record(&request),
    )
}

/// Asserts that a check records nothing, for a beat that would record a taint, change
/// the phase, end a gap and count a beat: the host listing (state, phase, taint, whether
/// it was heard since its hello), the bindings, each host's own view and the counters are
/// identical before and after, both for an admitted check and a refused one; the
/// heartbeat that follows then records all of it. It catches a check that writes the
/// last-heard time, the phase, the beat count or the taint.
#[test]
fn check_heartbeat_changes_nothing() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    fx.beat("b", 1);
    fx.pass_grace();
    let mut message = fx.heartbeat("a", 1);
    message.phase = Phase::Draining;
    message.taint = Some(report("residue"));
    let before = snapshot(&fx);
    assert_eq!(view(&fx, "a").state, HostState::Lost);

    let check = fx.registry.check_heartbeat(&message, &cred("t")).unwrap();
    let expected = HeartbeatCheck {
        host: host("a"),
        taint: Some(report("residue")),
    };
    assert_eq!(check, expected);
    assert_eq!(snapshot(&fx), before);
    let mut stale = message.clone();
    stale.epoch = 2;
    let refused = fx.registry.check_heartbeat(&stale, &cred("t"));
    assert_eq!(refused, Err(Refusal::StaleEpoch));
    assert_eq!(snapshot(&fx), before);

    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    let after = view(&fx, "a");
    assert_eq!(after.state, HostState::Live);
    assert_eq!(after.phase, Phase::Draining);
    assert!(after.heard_since_hello);
    assert_eq!(after.taint, Some(report("residue")));
    assert_eq!(view(&fx, "b").state, HostState::Lost);
}

/// Asserts that a check leaves a committed whole-node lease as it was: a check of a ready
/// report does not make the lease ready, and a check of a `host-unsafe` refusal of the
/// lease (which would taint the host) neither ends the lease, nor counts a failure for
/// its request, nor taints the host; the heartbeat that follows ends the lease and
/// records the taint the check named. It catches a check that runs the whole-node part
/// of the beat.
#[test]
fn check_heartbeat_changes_nothing_on_a_committed_whole_node_lease() {
    let fx = committed();
    let before = whole_state(&fx);
    assert_ne!(before.1, Observed::Absent);
    let mut ready = fx.heartbeat("a", 1);
    ready.exclusive_ready = Some("1-1".into());
    let check = fx.registry.check_heartbeat(&ready, &cred("t")).unwrap();
    assert_eq!(check.taint, None);
    assert_eq!(whole_state(&fx), before);

    let mut refusing = fx.heartbeat("a", 1);
    refusing.exclusive_refused = Some(exclusive_refusal("1-1", RefusalReason::HostUnsafe));
    let check = fx.registry.check_heartbeat(&refusing, &cred("t")).unwrap();
    assert_eq!(check.taint, Some(host_unsafe()));
    assert_eq!(whole_state(&fx), before);

    fx.registry.heartbeat(&refusing, &cred("t")).unwrap();
    assert_eq!(fx.provider.clone().observe(&lease(1)), Observed::Absent);
    assert_eq!(view(&fx, "a").taint, Some(host_unsafe()));
}

/// Checks `message` from host `a`, then sends it, and asserts that the taint the check
/// named is the one the host then shows, in the listing and in its binding; a check that
/// names none leaves the host's taint as it was. Returns what the check named.
fn check_then_beat(fx: &Fixture, message: &Heartbeat) -> Option<Taint> {
    let check = fx.registry.check_heartbeat(message, &cred("t")).unwrap();
    assert_eq!(check.host, host("a"));
    let prior = view(fx, "a").taint;
    fx.registry.heartbeat(message, &cred("t")).unwrap();
    let shown = view(fx, "a").taint;
    let bound = fx.registry.bindings()[&host("a")].taint().cloned();
    assert_eq!(bound, shown);
    let expected = check.taint.clone().or(prior);
    assert_eq!(shown, expected);
    check.taint
}

/// Asserts, case by case, that the taint a check names is the one the heartbeat then
/// records, and what it is: none for a plain beat; the reported taint; for a start
/// refused as `host-tainted` a `host-tainted` taint naming the run; none for a start
/// refused as `quota-unavailable` or a run killed with a `host-tainted` detail; the report
/// over a refused start; none on a host already tainted, which keeps its first taint. It
/// catches a check whose taint rule drifts from the heartbeat's.
#[test]
fn check_heartbeat_returns_the_taint_the_beat_then_records() {
    let refused_start = result("r9", Outcome::FailedToStart, refusal::HOST_TAINTED);
    let cases: Vec<(&str, Heartbeat, Option<Taint>)> = vec![
        ("no report", edited(|_| {}), None),
        (
            "a report",
            edited(|m| m.taint = Some(report("residue"))),
            Some(report("residue")),
        ),
        (
            "a start refused as host-tainted",
            edited(|m| m.results = vec![refused_start.clone()]),
            Some(tainted_start("r9")),
        ),
        (
            "a start refused for quota",
            edited(|m| {
                let detail = refusal::QUOTA_UNAVAILABLE;
                m.results = vec![result("r9", Outcome::FailedToStart, detail)];
            }),
            None,
        ),
        (
            "a killed run with a host-tainted detail",
            edited(|m| {
                let killed = result("r9", Outcome::Killed, refusal::HOST_TAINTED);
                m.results = vec![killed];
            }),
            None,
        ),
        (
            "the first of several starts refused as host-tainted",
            edited(|m| {
                let mut unnamed = result("r2", Outcome::FailedToStart, "");
                unnamed.detail = None;
                m.results = vec![
                    result("r1", Outcome::Killed, refusal::HOST_TAINTED),
                    unnamed,
                    result("r3", Outcome::FailedToStart, refusal::QUOTA_UNAVAILABLE),
                    result("r4", Outcome::FailedToStart, refusal::HOST_TAINTED),
                    result("r5", Outcome::FailedToStart, refusal::HOST_TAINTED),
                ];
            }),
            Some(tainted_start("r4")),
        ),
        (
            "a report and a refused start",
            edited(|m| {
                m.taint = Some(report("residue"));
                m.results = vec![refused_start.clone()];
            }),
            Some(report("residue")),
        ),
    ];
    for (name, message, expected) in cases {
        let fx = fixture(1);
        fx.join("a", "t");
        let message = Heartbeat {
            session: fx.session("a"),
            ..message
        };
        assert_eq!(check_then_beat(&fx, &message), expected, "{name}");
    }

    let fx = fixture(1);
    fx.join("a", "t");
    let mut first = fx.heartbeat("a", 1);
    first.taint = Some(report("first"));
    assert_eq!(check_then_beat(&fx, &first), Some(report("first")));
    let mut other = fx.heartbeat("a", 1);
    other.taint = Some(report("other"));
    other.results = vec![refused_start];
    assert_eq!(check_then_beat(&fx, &other), None);
    assert_eq!(view(&fx, "a").taint, Some(report("first")));
}

/// Asserts the same for the whole-node part of a beat: a `host-unsafe` refusal of the
/// committed lease names the `host-unsafe` taint; one naming another lease, or a refusal
/// whose reason does not taint (`not-quiet`), names none; a report beside a `host-unsafe`
/// refusal names the report; a report beside a ready report for the lease names the
/// report; a ready report alone names none; a `host-unsafe` refusal on a host with no
/// lease committed names none; a host tainted before the refusal keeps its first taint
/// and the check names none. (The wire refuses a heartbeat that carries both a ready
/// report and a refusal, so that pair never reaches either call.) It catches a check
/// that misses the taint a refused prepare records, or names one the beat does not
/// record.
#[test]
fn check_heartbeat_returns_the_taint_a_refused_prepare_records() {
    let unsafe_refusal = exclusive_refusal("1-1", RefusalReason::HostUnsafe);
    let cases: Vec<(&str, Heartbeat, Option<Taint>)> = vec![
        (
            "host-unsafe for the committed lease",
            edited(|m| m.exclusive_refused = Some(unsafe_refusal.clone())),
            Some(host_unsafe()),
        ),
        (
            "host-unsafe for another lease",
            edited(|m| {
                let other = exclusive_refusal("1-7", RefusalReason::HostUnsafe);
                m.exclusive_refused = Some(other);
            }),
            None,
        ),
        (
            "not-quiet for the committed lease",
            edited(|m| {
                let quiet = exclusive_refusal("1-1", RefusalReason::NotQuiet);
                m.exclusive_refused = Some(quiet);
            }),
            None,
        ),
        (
            "a report and host-unsafe",
            edited(|m| {
                m.taint = Some(report("residue"));
                m.exclusive_refused = Some(unsafe_refusal.clone());
            }),
            Some(report("residue")),
        ),
        (
            "a report and ready for the committed lease",
            edited(|m| {
                m.taint = Some(report("residue"));
                m.exclusive_ready = Some("1-1".into());
            }),
            Some(report("residue")),
        ),
        (
            "ready for the committed lease",
            edited(|m| m.exclusive_ready = Some("1-1".into())),
            None,
        ),
    ];
    for (name, message, expected) in cases {
        let fx = committed();
        let message = Heartbeat {
            session: fx.session("a"),
            ..message
        };
        assert_eq!(check_then_beat(&fx, &message), expected, "{name}");
    }

    // A refusal on a host with no lease committed names no lease, and no taint.
    let fx = fixture_of(whole_config());
    fx.join("a", "t");
    let mut uncommitted = fx.heartbeat("a", 1);
    uncommitted.exclusive_refused = Some(unsafe_refusal.clone());
    assert_eq!(check_then_beat(&fx, &uncommitted), None);

    // A host tainted before its lease is refused keeps the first taint, and the check
    // names none.
    let fx = committed();
    let mut first = fx.heartbeat("a", 1);
    first.taint = Some(report("first"));
    fx.registry.heartbeat(&first, &cred("t")).unwrap();
    let mut refusing = fx.heartbeat("a", 1);
    refusing.exclusive_refused = Some(unsafe_refusal);
    assert_eq!(check_then_beat(&fx, &refusing), None);
    assert_eq!(view(&fx, "a").taint, Some(report("first")));
}

/// Sends `message` with `credential` to both calls and asserts that each refuses it with
/// the same refusal and that neither changed what the registry shows. Returns the
/// refusal.
fn refused_alike(fx: &Fixture, message: &Heartbeat, credential: &Credential) -> Refusal {
    let before = snapshot(fx);
    let registry = &fx.registry;
    let checked = registry.check_heartbeat(message, credential).unwrap_err();
    let beaten = registry.heartbeat(message, credential).unwrap_err();
    assert_eq!(checked, beaten);
    assert_eq!(snapshot(fx), before);
    checked
}

/// Asserts that a check refuses exactly what the heartbeat refuses, with the same
/// refusal, in the same order: a host id that is not one (before a message that breaks
/// the wire rules), a message that breaks the wire rules (before an unknown host), a
/// ready report beside a refusal of the lease (which the wire rules forbid), an unknown
/// host, a host awaiting its hello after a restart, a host forgotten
/// after the lost-host retention, a credential the host is not bound to (before a stale
/// epoch), a stale epoch (before a wrong session), a wrong session, and the session of
/// an earlier hello. It catches a check whose admission drifts from the heartbeat's.
#[test]
fn check_heartbeat_refuses_what_heartbeat_refuses() {
    let config = config(3)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let fx = fixture_of(config);
    fx.join("a", "t");
    let ok = cred("t");

    let nameless = heartbeat("", 1);
    let refusal = refused_alike(&fx, &nameless, &ok);
    assert!(
        matches!(refusal, Refusal::BadHeartbeatHostId(_)),
        "{refusal:?}"
    );

    let mut broken = fx.heartbeat("a", 1);
    broken.session = "not a secret!".into();
    let refusal = refused_alike(&fx, &broken, &ok);
    assert!(matches!(refusal, Refusal::Invalid(_)), "{refusal:?}");

    let mut bad_and_broken = heartbeat("", 1);
    bad_and_broken.session = "not a secret!".into();
    let refusal = refused_alike(&fx, &bad_and_broken, &ok);
    assert!(
        matches!(refusal, Refusal::BadHeartbeatHostId(_)),
        "{refusal:?}"
    );
    let unknown_and_broken = Heartbeat {
        host_id: "z".into(),
        ..broken.clone()
    };
    let refusal = refused_alike(&fx, &unknown_and_broken, &ok);
    assert!(matches!(refusal, Refusal::Invalid(_)), "{refusal:?}");
    let mut ready_and_refused = fx.heartbeat("a", 1);
    ready_and_refused.exclusive_ready = Some("1-1".into());
    let refused = exclusive_refusal("1-1", RefusalReason::HostUnsafe);
    ready_and_refused.exclusive_refused = Some(refused);
    let refusal = refused_alike(&fx, &ready_and_refused, &ok);
    assert!(matches!(refusal, Refusal::Invalid(_)), "{refusal:?}");

    let unknown = fx.heartbeat("a", 1);
    let unknown = Heartbeat {
        host_id: "z".into(),
        ..unknown
    };
    assert_eq!(refused_alike(&fx, &unknown, &ok), Refusal::UnknownHost);

    let taint = Some(report("residue"));
    let tainted = Binding::new(ok.clone(), None).with_taint(taint);
    fx.registry.restore_tainted(&host("w"), tainted);
    let awaiting = Heartbeat {
        host_id: "w".into(),
        ..fx.heartbeat("a", 1)
    };
    assert_eq!(refused_alike(&fx, &awaiting, &ok), Refusal::UnknownHost);

    let current = fx.heartbeat("a", 1);
    assert_eq!(
        refused_alike(&fx, &current, &cred("other")),
        Refusal::HostBound
    );
    let mut stale = fx.heartbeat("a", 1);
    stale.epoch = 2;
    assert_eq!(
        refused_alike(&fx, &stale, &cred("other")),
        Refusal::HostBound
    );
    assert_eq!(refused_alike(&fx, &stale, &ok), Refusal::StaleEpoch);
    let mut stale_and_wrong = stale.clone();
    stale_and_wrong.session = "wrong".into();
    assert_eq!(
        refused_alike(&fx, &stale_and_wrong, &ok),
        Refusal::StaleEpoch
    );
    let mut wrong = fx.heartbeat("a", 1);
    wrong.session = "wrong".into();
    assert_eq!(refused_alike(&fx, &wrong, &ok), Refusal::SessionMismatch);
    let mut empty = fx.heartbeat("a", 1);
    empty.session = String::new();
    assert_eq!(refused_alike(&fx, &empty, &ok), Refusal::SessionMismatch);

    // A new hello replaces the session: the old secret, sent with the new epoch, is
    // refused by both.
    let old = fx.heartbeat("a", 1);
    fx.pass_grace();
    assert_eq!(fx.join("a", "t"), 2);
    let earlier = Heartbeat { epoch: 2, ..old };
    assert_eq!(refused_alike(&fx, &earlier, &ok), Refusal::SessionMismatch);
    let admitted = fx.heartbeat("a", 2);
    let check = fx.registry.check_heartbeat(&admitted, &ok).unwrap();
    assert_eq!(check.host, host("a"));
    fx.registry.heartbeat(&admitted, &ok).unwrap();

    // A host lost for longer than the retention with no booking is forgotten, and both
    // refuse it as unknown.
    fx.clock.advance(Duration::from_millis(
        js_agent_wire::DEFAULT_GRACE_MS + MIN_LOST_RETENTION_MS + 1,
    ));
    let forgotten = fx.heartbeat("a", 2);
    assert_eq!(refused_alike(&fx, &forgotten, &ok), Refusal::UnknownHost);
}

/// A job of one core, run `run`, with no lease bound.
fn job(run: &str) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
        network: Default::default(),
        ports: Vec::new(),
        run: None,
        os: None,
        arch: None,
    }
}

fn held(run: &str, state: HeldState) -> HeldRun {
    HeldRun {
        run_id: run.into(),
        state,
    }
}

/// A load reading `seq` with its counters and no lease records.
fn load(seq: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: 1_000_000 + seq,
        cpu_usage_usec: 0,
        working_set_bytes: 1 << 20,
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        dropped: Default::default(),
    }
}

fn run_state(fx: &Fixture, run: &str) -> RunState {
    run_view(fx, run).state
}

fn run_view(fx: &Fixture, run: &str) -> RunView {
    let runs = fx.registry.runs_of(&lease(1));
    runs.into_iter().find(|view| view.run_id == run).unwrap()
}

/// Asserts that a check leaves the work on a host as it was, for a beat that would change
/// it all: with lease 1 on host `a`, run `r1` running, `r2` queued and a load reading
/// stored, a check of a heartbeat that lists only `r2`, reports `r1` succeeded, carries a
/// newer load and another phase leaves the listing (load and bookings included), the
/// bindings, the counters and the run views identical; the heartbeat that follows ends
/// `r1` with its result, takes `r2` and the newer load. It catches a check that syncs
/// the held list, acknowledges a result or takes a load reading.
#[test]
fn check_heartbeat_changes_no_run_held_list_or_load() {
    let fx = fixture(2);
    fx.join("a", "t");
    assert_eq!(started_on(&fx.provider.clone().start(spec(1))), Some("a"));
    fx.registry
        .queue_run(&host("a"), &lease(1), job("r1"))
        .unwrap();
    fx.registry
        .queue_run(&host("a"), &lease(1), job("r2"))
        .unwrap();
    let mut running = fx.heartbeat("a", 1);
    running.held = vec![held("r1", HeldState::Running)];
    running.load = Some(load(5));
    fx.registry.heartbeat(&running, &cred("t")).unwrap();
    assert_ne!(run_state(&fx, "r1"), RunState::Queued);
    assert_eq!(run_state(&fx, "r2"), RunState::Queued);
    assert_eq!(view(&fx, "a").load.map(|load| load.seq), Some(5));

    let mut message = fx.heartbeat("a", 1);
    message.phase = Phase::Draining;
    message.held = vec![held("r2", HeldState::Running)];
    message.results = vec![RunResult {
        run_id: "r1".into(),
        lease_id: "1-1".into(),
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
        log_tail: None,
    }];
    message.load = Some(load(9));
    let before = snapshot(&fx);
    let check = fx.registry.check_heartbeat(&message, &cred("t")).unwrap();
    assert_eq!(check.taint, None);
    assert_eq!(snapshot(&fx), before);

    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    let ended = run_view(&fx, "r1");
    assert_eq!(ended.state, RunState::Ended);
    let outcome = ended.result.map(|result| result.outcome);
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_ne!(run_state(&fx, "r2"), RunState::Queued);
    assert_eq!(view(&fx, "a").load.map(|load| load.seq), Some(9));
    assert_eq!(view(&fx, "a").phase, Phase::Draining);
}

/// Checks `message` from host `a`, expecting no taint, and asserts that the check changed
/// nothing of the whole-node state.
fn checked_unchanged(fx: &Fixture, message: &Heartbeat) {
    let before = whole_state(fx);
    let check = fx.registry.check_heartbeat(message, &cred("t")).unwrap();
    assert_eq!(check.taint, None);
    assert_eq!(whole_state(fx), before);
}

/// Asserts that a check leaves a ready whole-node lease and both stages of a restore as
/// they were: a `host-unsafe` refusal of a lease already ready names no taint, and the
/// heartbeat that carries it leaves the lease held and the host clean; after the lease
/// ends, two checks before the first beat and two before the second advance nothing, so
/// the host is still releasing after one beat and free after the second. It catches a
/// check that advances the restore or ends a ready lease.
#[test]
fn check_heartbeat_changes_nothing_on_a_ready_or_restoring_host() {
    let fx = committed();
    let mut ready = fx.heartbeat("a", 1);
    ready.exclusive_ready = Some("1-1".into());
    fx.registry.heartbeat(&ready, &cred("t")).unwrap();
    let held_lease = fx.provider.clone().observe(&lease(1));
    assert_ne!(held_lease, Observed::Absent);

    let mut refusing = fx.heartbeat("a", 1);
    refusing.exclusive_refused = Some(exclusive_refusal("1-1", RefusalReason::HostUnsafe));
    checked_unchanged(&fx, &refusing);
    fx.registry.heartbeat(&refusing, &cred("t")).unwrap();
    assert_eq!(fx.provider.clone().observe(&lease(1)), held_lease);
    assert_eq!(view(&fx, "a").taint, None);

    fx.provider.clone().stop(&lease(1));
    let plain = fx.heartbeat("a", 1);
    assert!(view(&fx, "a").releasing);
    checked_unchanged(&fx, &plain);
    checked_unchanged(&fx, &plain);
    fx.registry.heartbeat(&plain, &cred("t")).unwrap();
    assert!(view(&fx, "a").releasing);
    checked_unchanged(&fx, &plain);
    checked_unchanged(&fx, &plain);
    fx.registry.heartbeat(&plain, &cred("t")).unwrap();
    assert!(!view(&fx, "a").releasing);
}

/// Asserts the precedence of the two taints a beat makes itself: a start refused as
/// `host-tainted` beside a `host-unsafe` refusal of the committed lease names the
/// `host-tainted` taint of that run, the check changes nothing, and the heartbeat records
/// that taint and ends the lease. It catches a check that ranks the refused prepare
/// first.
#[test]
fn check_heartbeat_names_a_refused_start_over_a_refused_prepare() {
    let fx = committed();
    let mut both = fx.heartbeat("a", 1);
    let refused_start = result("r9", Outcome::FailedToStart, refusal::HOST_TAINTED);
    both.results = vec![refused_start];
    both.exclusive_refused = Some(exclusive_refusal("1-1", RefusalReason::HostUnsafe));
    let before = whole_state(&fx);
    let check = fx.registry.check_heartbeat(&both, &cred("t")).unwrap();
    assert_eq!(check.taint, Some(tainted_start("r9")));
    assert_eq!(whole_state(&fx), before);

    fx.registry.heartbeat(&both, &cred("t")).unwrap();
    assert_eq!(view(&fx, "a").taint, Some(tainted_start("r9")));
    assert_eq!(fx.provider.clone().observe(&lease(1)), Observed::Absent);
}

/// Asserts that a heartbeat answers for the registry as it is when it arrives, not as a
/// check found it: after a check, a new hello makes the checked heartbeat stale and it
/// records nothing; another taint recorded first is kept over the one the check named;
/// a committed lease that ends leaves the checked refusal naming no lease and no taint.
/// It catches a heartbeat that applies an earlier check's answer.
#[test]
fn heartbeat_answers_for_the_registry_as_it_is_after_a_check() {
    let fx = fixture(1);
    fx.join("a", "t");
    let mut tainting = fx.heartbeat("a", 1);
    tainting.taint = Some(report("residue"));
    fx.pass_grace();
    let check = fx.registry.check_heartbeat(&tainting, &cred("t")).unwrap();
    assert_eq!(check.taint, Some(report("residue")));
    assert_eq!(fx.join("a", "t"), 2);
    let stale = fx.registry.heartbeat(&tainting, &cred("t")).unwrap_err();
    assert_eq!(stale, Refusal::StaleEpoch);
    assert_eq!(view(&fx, "a").taint, None);

    let fx = fixture(1);
    fx.join("a", "t");
    let mut second = fx.heartbeat("a", 1);
    second.taint = Some(report("second"));
    let check = fx.registry.check_heartbeat(&second, &cred("t")).unwrap();
    assert_eq!(check.taint, Some(report("second")));
    let mut first = fx.heartbeat("a", 1);
    first.taint = Some(report("first"));
    fx.registry.heartbeat(&first, &cred("t")).unwrap();
    fx.registry.heartbeat(&second, &cred("t")).unwrap();
    assert_eq!(view(&fx, "a").taint, Some(report("first")));
    let bound = fx.registry.bindings()[&host("a")].taint().cloned();
    assert_eq!(bound, Some(report("first")));

    let fx = committed();
    let mut refusing = fx.heartbeat("a", 1);
    refusing.exclusive_refused = Some(exclusive_refusal("1-1", RefusalReason::HostUnsafe));
    let check = fx.registry.check_heartbeat(&refusing, &cred("t")).unwrap();
    assert_eq!(check.taint, Some(host_unsafe()));
    fx.provider.clone().stop(&lease(1));
    fx.registry.heartbeat(&refusing, &cred("t")).unwrap();
    assert_eq!(view(&fx, "a").taint, None);
}

/// A pool that forgets a lost host after the shortest retention, with host `a` joined
/// (epoch 1).
fn retaining() -> Fixture {
    let config = config(4)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let fx = fixture_of(config);
    fx.join("a", "t");
    fx
}

/// Moves the clock to `past` milliseconds after the end of the grace and the retention,
/// counted from now.
fn after_retention(fx: &Fixture, past: u64) {
    let kept = DEFAULT_GRACE_MS + MIN_LOST_RETENTION_MS;
    fx.clock.advance(Duration::from_millis(kept + past));
}

/// Checks `message` from host `a`, expecting it admitted, and then sends it, expecting the
/// heartbeat admitted too.
fn admitted_alike(fx: &Fixture, message: &Heartbeat) {
    let check = fx.registry.check_heartbeat(message, &cred("t")).unwrap();
    assert_eq!(check.host, host("a"));
    fx.registry.heartbeat(message, &cred("t")).unwrap();
}

/// Asserts that a check refuses a silent host as forgotten exactly when the heartbeat
/// does, each check made before any other call could forget the host: at the end of the
/// grace and the retention the host is held and both admit; one millisecond later both
/// refuse it as unknown; a host booked by a lease, tainted, or cordoned is kept past
/// that, and both admit it. It catches a check whose retention rule differs from the one
/// that forgets hosts.
#[test]
fn check_heartbeat_forgets_a_host_exactly_when_heartbeat_does() {
    let fx = retaining();
    let beat = fx.heartbeat("a", 1);
    after_retention(&fx, 0);
    admitted_alike(&fx, &beat);

    let fx = retaining();
    let beat = fx.heartbeat("a", 1);
    after_retention(&fx, 1);
    let checked = fx.registry.check_heartbeat(&beat, &cred("t")).unwrap_err();
    assert_eq!(checked, Refusal::UnknownHost);
    let beaten = fx.registry.heartbeat(&beat, &cred("t")).unwrap_err();
    assert_eq!(beaten, Refusal::UnknownHost);

    let fx = retaining();
    assert_eq!(started_on(&fx.provider.clone().start(spec(1))), Some("a"));
    after_retention(&fx, 1);
    admitted_alike(&fx, &fx.heartbeat("a", 1));

    let fx = retaining();
    let mut tainting = fx.heartbeat("a", 1);
    tainting.taint = Some(report("residue"));
    fx.registry.heartbeat(&tainting, &cred("t")).unwrap();
    after_retention(&fx, 1);
    admitted_alike(&fx, &fx.heartbeat("a", 1));

    let fx = retaining();
    let hour = Duration::from_secs(3600);
    fx.registry.cordon(&host("a"), hour).unwrap();
    after_retention(&fx, 1);
    admitted_alike(&fx, &fx.heartbeat("a", 1));
}
