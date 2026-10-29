//! What the registry does when a committed whole-node lease does not become ready: a
//! typed refusal, the `prepare_timeout`, the host and request effects of each reason, and
//! the cap on tries. The registry is driven directly, on a manual clock, with
//! hand-built heartbeats. The starts carry no request id, so a request is its content: the
//! tests place the same `whole_request()` under a new lease id each time.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{
    Command, ExclusiveRefusal, HeldRun, HeldState, Isolation, JobSpec, Limits, RefusalReason,
};
use js_lease::{Clock, Timestamp};
use js_provider::{Observed, Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{
    DEFAULT_PREPARE_TIMEOUT_MS, DEFAULT_RESERVE_BACKOFF_MS, Exclusion, NotReady, PoolConfig,
    RunState, WholeNodeConfig,
};
use js_types::{ProviderId, Request, ResourceClass};
use support::*;

const BACKOFF: u64 = 60_000;
const TIMEOUT: u64 = 120_000;

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn plain_config() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, whole(), pool_shape(), 4)
}

/// Short settings, so a test walks the whole deadline in a few steps: a prepare timeout
/// of two minutes, a back-off of one, two tries.
fn short_config() -> PoolConfig {
    let settings = WholeNodeConfig {
        prepare_timeout_ms: TIMEOUT,
        reserve_backoff_ms: BACKOFF,
        max_replace: 2,
        ..WholeNodeConfig::dedicated(4)
    };
    plain_config().with_whole_node(settings).unwrap()
}

fn whole_request() -> Request {
    Request {
        class: whole().id().clone(),
        ..request()
    }
}

fn whole_spec(sequence: u64) -> StartSpec {
    StartSpec::new(lease(sequence), whole_request(), &[whole()]).unwrap()
}

fn no_capacity() -> StartAck {
    StartAck::Refused(RefusalCode::NoCapacity)
}

fn exclusive_not_ready() -> StartAck {
    StartAck::Refused(RefusalCode::ExclusiveNotReady)
}

/// A pool of `config` with every host in `names` joined, and the epochs they got.
fn pool(config: PoolConfig, names: &[&'static str]) -> (Fixture, Vec<(&'static str, u64)>) {
    let fx = fixture_of(config);
    let hosts = names
        .iter()
        .map(|name| (*name, fx.join(name, "t")))
        .collect();
    (fx, hosts)
}

/// Moves the clock on by `ms`, every host sending a serving heartbeat every five seconds
/// (the grace period is fifteen), so none of them is lost.
fn wait(fx: &Fixture, hosts: &[(&str, u64)], ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(5_000);
        fx.clock.advance(Duration::from_millis(step));
        hosts.iter().for_each(|(name, epoch)| fx.beat(name, *epoch));
        left -= step;
    }
}

/// `host` refuses `lease_id` with `refusal`.
fn refuse_with(fx: &Fixture, host: (&str, u64), refusal: ExclusiveRefusal) {
    let mut message = fx.heartbeat(host.0, host.1);
    message.exclusive_refused = Some(refusal);
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
}

fn refuse(fx: &Fixture, host: (&str, u64), lease_id: &str, code: RefusalReason, detail: &str) {
    let refusal = ExclusiveRefusal {
        lease_id: lease_id.into(),
        code,
        detail: detail.into(),
    };
    refuse_with(fx, host, refusal);
}

/// The agent has been answered without `exclusive` and has reported serving twice: the
/// host is clear of the lease that ended.
fn restore(fx: &Fixture, hosts: &[(&str, u64)]) {
    for _ in 0..2 {
        hosts.iter().for_each(|(name, epoch)| fx.beat(name, *epoch));
    }
}

fn after(at: Timestamp, ms: u64) -> Timestamp {
    at.plus(Duration::from_millis(ms))
}

fn record(fx: &Fixture) -> js_registry::RequestRecord {
    fx.registry.request_record(&whole_request()).unwrap()
}

fn exclusions(pairs: &[(&str, Exclusion)]) -> BTreeMap<js_types::HostId, Exclusion> {
    pairs.iter().map(|(name, e)| (host(name), *e)).collect()
}

/// Asserts both effects of a row that excludes the host for `reserve_backoff` and
/// re-queues the request: the lease reads `Absent` at once (long before the prepare
/// timeout), the host holds nothing, the request has one prepare failure and no give-up
/// and is not terminal, the host is not tainted, and the same request is turned away
/// from that host (the only one) until exactly `reserve_backoff` after the refusal.
fn backoff_row(code: RefusalReason) {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    wait(&fx, &hosts, 5_000);
    let refused_at = fx.clock.now();
    refuse(&fx, hosts[0], "1-1", code, "the detail");
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    assert_eq!(fx.registry.hosts()[0].lease, None);
    assert_eq!(fx.registry.hosts()[0].taint, None);
    let kept = record(&fx);
    assert_eq!((kept.prepare_failures, kept.give_ups), (1, 0));
    assert!(!kept.terminal);
    let until = Exclusion::Until(after(refused_at, BACKOFF));
    assert_eq!(kept.exclusions, exclusions(&[("a", until)]));
    restore(&fx, &hosts);
    wait(&fx, &hosts, BACKOFF - 1);
    assert_eq!(provider.start(whole_spec(2)), no_capacity());
    wait(&fx, &hosts, 1);
    assert_eq!(provider.start(whole_spec(3)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(3)));
}

#[test]
fn not_quiet_excludes_the_host_for_the_backoff_and_requeues() {
    backoff_row(RefusalReason::NotQuiet);
}

#[test]
fn noise_over_bound_excludes_the_host_for_the_backoff_and_requeues() {
    backoff_row(RefusalReason::NoiseOverBound);
}

#[test]
fn setup_failed_excludes_the_host_for_the_backoff_and_requeues() {
    backoff_row(RefusalReason::SetupFailed);
}

/// Asserts the `host-unsafe` row: the host is tainted (as by a finding) and takes no
/// lease however long it waits, the request counts one failure and gets no exclusion of
/// its own (the taint is the host's effect), and it is placed again on another host at
/// once.
#[test]
fn host_unsafe_taints_the_host_and_requeues_on_another() {
    let (fx, hosts) = pool(short_config(), &["a", "b"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    refuse(&fx, hosts[0], "1-1", RefusalReason::HostUnsafe, "a finding");
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    let taint = fx.registry.hosts()[0].taint.clone().unwrap();
    assert_eq!(taint.reason, "host-unsafe");
    let kept = record(&fx);
    assert_eq!((kept.prepare_failures, kept.give_ups), (1, 0));
    assert!(kept.exclusions.is_empty());
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(2)));
    assert_eq!(fx.registry.hosts()[1].taint, None);
    let mut ready = fx.heartbeat("b", hosts[1].1);
    ready.exclusive_ready = Some("1-2".into());
    fx.registry.heartbeat(&ready, &cred("t")).unwrap();
    restore(&fx, &hosts);
    wait(&fx, &hosts, 10 * BACKOFF);
    // `b` is held; `a` is restored, but tainted: another request finds nothing.
    let mut other = whole_request();
    other.max_wall = Duration::from_secs(1);
    let spec = StartSpec::new(lease(3), other, &[whole()]).unwrap();
    assert_eq!(provider.start(spec), no_capacity());
}

/// Asserts the `unsupported` row: the host is left out for the rest of the request's
/// life (two back-offs later it is still refused, and another request takes it), and the
/// request counts one failure and goes to another host at once.
#[test]
fn unsupported_excludes_the_host_for_good_and_requeues_on_another() {
    let (fx, hosts) = pool(short_config(), &["a", "b"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    refuse(&fx, hosts[0], "1-1", RefusalReason::Unsupported, "no hook");
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    let kept = record(&fx);
    assert_eq!((kept.prepare_failures, kept.give_ups), (1, 0));
    assert_eq!(kept.exclusions, exclusions(&[("a", Exclusion::ForLife)]));
    assert_eq!(fx.registry.hosts()[0].taint, None);
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(2)));
    provider.stop(&lease(2));
    restore(&fx, &hosts);
    wait(&fx, &hosts, 2 * BACKOFF);
    assert_eq!(provider.start(whole_spec(3)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(3)));
    let mut other = whole_request();
    other.max_wall = Duration::from_secs(1);
    let free = StartSpec::new(lease(4), other, &[whole()]).unwrap();
    assert_eq!(provider.start(free), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(4)));
}

/// Asserts that a refusal puts the request on another host at once: with host `a`
/// restored and free (and first by id) the same request still goes to `b`, and the
/// lease it left is `Absent` at once, not at the prepare timeout.
#[test]
fn a_refused_request_is_placed_on_another_host_at_once() {
    let (fx, hosts) = pool(short_config(), &["a", "b"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(1)));
    refuse(&fx, hosts[0], "1-1", RefusalReason::NotQuiet, "busy");
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    restore(&fx, &hosts);
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(2)));
    assert_eq!(provider.booked().get(&host("a")), None);
}

/// A code the wire does not know decodes as `setup-failed` and acts as that row: the
/// host is excluded for the back-off, not for good and not tainted, and a terminal
/// failure names `setup-failed`.
#[test]
fn an_unknown_code_acts_as_setup_failed() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    let refused_at = fx.clock.now();
    let text = r#"{"lease_id":"1-1","code":"from-the-future","detail":"?"}"#;
    let refusal: ExclusiveRefusal = serde_json::from_str(text).unwrap();
    refuse_with(&fx, hosts[0], refusal.clone());
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    let until = Exclusion::Until(after(refused_at, BACKOFF));
    assert_eq!(record(&fx).exclusions, exclusions(&[("a", until)]));
    assert_eq!(fx.registry.hosts()[0].taint, None);
    restore(&fx, &hosts);
    wait(&fx, &hosts, BACKOFF);
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    refuse_with(
        &fx,
        hosts[0],
        ExclusiveRefusal {
            lease_id: "1-2".into(),
            ..refusal
        },
    );
    let last = fx.registry.not_ready(&whole_request()).unwrap();
    assert_eq!(last.code, "setup-failed");
}

/// Asserts the deadline against the pool's defaults (no whole-node settings): one
/// millisecond short of `prepare_timeout` the lease is kept (`Unknown`, the host
/// holds it), at exactly `prepare_timeout` from the commit it reads `Absent`, the host
/// is released to restore, the request counts one failure and none toward give-ups, and
/// the host is excluded for `reserve_backoff` counted from the deadline. The last code
/// is `prepare-timeout` with no detail.
#[test]
fn the_lease_ends_exactly_at_prepare_timeout() {
    let (fx, hosts) = pool(plain_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    wait(&fx, &hosts, DEFAULT_PREPARE_TIMEOUT_MS - 1);
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert_eq!(fx.registry.hosts()[0].lease, Some(lease(1)));
    assert!(fx.registry.request_record(&whole_request()).is_none());
    wait(&fx, &hosts, 1);
    let deadline = fx.clock.now();
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    assert_eq!(fx.registry.hosts()[0].lease, None);
    let kept = record(&fx);
    assert_eq!((kept.prepare_failures, kept.give_ups), (1, 0));
    let until = after(deadline, DEFAULT_RESERVE_BACKOFF_MS);
    assert_eq!(
        kept.exclusions,
        exclusions(&[("a", Exclusion::Until(until))])
    );
    let last = NotReady {
        code: "prepare-timeout".into(),
        detail: String::new(),
    };
    assert_eq!(kept.last, Some(last));
    // Counted once, however often it is read.
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    assert_eq!(record(&fx).prepare_failures, 1);
}

/// The silent agent (it never beats again after the commit, so the host goes lost) and
/// the host that goes lost in the interval end at the same deadline, whether the clock is
/// read by `advance` or by the lease table's observe.
#[test]
fn a_silent_agent_and_a_lost_host_end_at_the_deadline() {
    for beating_for in [0, TIMEOUT - 30_000] {
        let (fx, hosts) = pool(short_config(), &["a"]);
        let mut provider = fx.provider.clone();
        assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
        wait(&fx, &hosts, beating_for);
        let rest = TIMEOUT - beating_for;
        fx.clock.advance(Duration::from_millis(rest - 1));
        fx.registry.advance();
        assert_eq!(fx.registry.hosts()[0].state, js_registry::HostState::Lost);
        assert_eq!(fx.registry.hosts()[0].lease, Some(lease(1)));
        assert!(fx.registry.request_record(&whole_request()).is_none());
        fx.clock.advance(Duration::from_millis(1));
        fx.registry.advance();
        assert_eq!(fx.registry.hosts()[0].lease, None);
        assert_eq!(record(&fx).prepare_failures, 1);
        assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    }
}

/// The timeout reads the pool's own settings: with a two-minute timeout the lease
/// survives one minute and ends at two.
#[test]
fn the_timeout_is_the_pools_setting() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    wait(&fx, &hosts, TIMEOUT / 2);
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    wait(&fx, &hosts, TIMEOUT / 2);
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
}

/// Asserts the cap: with `max_replace` of two, one failure leaves the request open and
/// the second makes it terminal, naming the last code and detail (the code of the
/// second, not the first); a terminal request is placed nowhere, though a host is free.
#[test]
fn the_request_fails_not_ready_after_max_replace_failures() {
    let (fx, hosts) = pool(short_config(), &["a", "b", "c"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    refuse(&fx, hosts[0], "1-1", RefusalReason::SetupFailed, "first");
    assert_eq!(fx.registry.not_ready(&whole_request()), None);
    assert!(!record(&fx).terminal);
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(2)));
    refuse(
        &fx,
        hosts[1],
        "1-2",
        RefusalReason::NoiseOverBound,
        "second",
    );
    let last = NotReady {
        code: "noise-over-bound".into(),
        detail: "second".into(),
    };
    assert_eq!(fx.registry.not_ready(&whole_request()), Some(last));
    let kept = record(&fx);
    assert_eq!((kept.prepare_failures, kept.give_ups), (2, 0));
    assert!(kept.terminal);
    assert_eq!(provider.start(whole_spec(3)), exclusive_not_ready());
    assert!(provider.booked().is_empty());
}

/// The default cap is three: two failures leave the request open, the third ends it, and
/// a timeout counts as one of them.
#[test]
fn the_default_cap_is_three_and_a_timeout_counts() {
    let config = plain_config();
    let (fx, hosts) = pool(config, &["a", "b", "c"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    refuse(&fx, hosts[0], "1-1", RefusalReason::NotQuiet, "");
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    refuse(&fx, hosts[1], "1-2", RefusalReason::NotQuiet, "");
    assert_eq!(fx.registry.not_ready(&whole_request()), None);
    assert_eq!(provider.start(whole_spec(3)), StartAck::Unknown);
    wait(&fx, &hosts, DEFAULT_PREPARE_TIMEOUT_MS);
    fx.registry.advance();
    let last = fx.registry.not_ready(&whole_request()).unwrap();
    assert_eq!(last.code, "prepare-timeout");
    assert_eq!(record(&fx).prepare_failures, 3);
}

/// A detail of the longest the wire allows, 256 bytes, is stored as received; and the
/// scheduler reads no detail: a `not-quiet` refusal whose text names the other codes
/// is still only a back-off exclusion.
#[test]
fn the_detail_is_stored_verbatim_and_never_read() {
    let (fx, hosts) = pool(short_config(), &["a", "b"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    let detail = "host-unsafe unsupported taint ".repeat(9)[..256].to_string();
    assert_eq!(detail.len(), 256);
    let refused_at = fx.clock.now();
    refuse(&fx, hosts[0], "1-1", RefusalReason::NotQuiet, &detail);
    let kept = record(&fx);
    assert_eq!(kept.last.unwrap().detail, detail);
    let until = Exclusion::Until(after(refused_at, BACKOFF));
    assert_eq!(kept.exclusions, exclusions(&[("a", until)]));
    assert_eq!(fx.registry.hosts()[0].taint, None);
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    refuse(&fx, hosts[1], "1-2", RefusalReason::SetupFailed, &detail);
    let last = fx.registry.not_ready(&whole_request()).unwrap();
    assert_eq!(
        (last.code.as_str(), last.detail.as_str()),
        ("setup-failed", detail.as_str())
    );
}

/// A refusal names a lease: one for another lease, and one for a lease already reported
/// ready, change nothing.
#[test]
fn a_refusal_for_another_lease_or_a_ready_one_is_ignored() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    refuse(&fx, hosts[0], "1-9", RefusalReason::NotQuiet, "other");
    assert_eq!(fx.registry.hosts()[0].lease, Some(lease(1)));
    assert!(fx.registry.request_record(&whole_request()).is_none());
    let mut message = fx.heartbeat("a", hosts[0].1);
    message.exclusive_ready = Some("1-1".into());
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    refuse(&fx, hosts[0], "1-1", RefusalReason::NotQuiet, "late");
    assert_eq!(provider.observe(&lease(1)), seen);
    wait(&fx, &hosts, 10 * TIMEOUT);
    assert_eq!(provider.observe(&lease(1)), seen);
}

/// A request that failed once and is then reported ready loses its record, and a lease
/// its consumer stops is no failure.
#[test]
fn a_granted_request_forgets_its_failures_and_a_stop_counts_none() {
    let (fx, hosts) = pool(short_config(), &["a", "b"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    refuse(&fx, hosts[0], "1-1", RefusalReason::NotQuiet, "");
    assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    assert_eq!(record(&fx).prepare_failures, 1);
    let mut message = fx.heartbeat("b", hosts[1].1);
    message.exclusive_ready = Some("1-2".into());
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert!(fx.registry.request_record(&whole_request()).is_none());

    let (fx, _hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    provider.stop(&lease(1));
    assert!(fx.registry.request_record(&whole_request()).is_none());
}

/// A whole-node booking put back after a restart has no request in memory: its lease
/// still ends at the deadline, and no request is charged for it.
#[test]
fn a_restored_booking_times_out_without_charging_a_request() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    provider.seat(&host("b"), lease(7), None).unwrap();
    wait(&fx, &hosts, TIMEOUT);
    assert_eq!(provider.observe(&lease(7)), Observed::Absent);
    assert!(fx.registry.request_record(&whole_request()).is_none());
    assert_eq!(provider.booked().get(&host("b")), None);
}

/// A small job, queued for the committed lease.
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
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
        native: None,
    }
}

/// Takes lease 1 to ready on host `a`, queues a run, and has the host say hello again still listing the run:
/// the preparation is void, the lease is committed once more with its run still queued.
/// Returns the host with the epoch of its new session.
fn committed_again_with_a_run(fx: &Fixture, epoch: u64) -> (&'static str, u64) {
    let mut ready = fx.heartbeat("a", epoch);
    ready.exclusive_ready = Some("1-1".into());
    fx.registry.heartbeat(&ready, &cred("t")).unwrap();
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job("r1"));
    assert_eq!(queued.unwrap().state, RunState::Queued);
    fx.pass_grace();
    let mut again = hello("a");
    again.held = vec![HeldRun {
        run_id: "r1".into(),
        state: HeldState::Running,
    }];
    ("a", fx.hello(&again, &cred("t")).unwrap().epoch)
}

/// The commands the next heartbeat of host `a` is answered with, the host still listing
/// run `r1` as running.
fn commands_for_listed_run(fx: &Fixture, epoch: u64) -> Vec<Command> {
    let mut message = fx.heartbeat("a", epoch);
    message.held = vec![HeldRun {
        run_id: "r1".into(),
        state: HeldState::Running,
    }];
    fx.registry
        .heartbeat(&message, &cred("t"))
        .unwrap()
        .commands
}

fn stop_r1() -> Vec<Command> {
    vec![Command::Stop {
        run_id: "r1".into(),
    }]
}

/// A refused prepare asks the runs of the committed lease to stop: the host that still
/// lists the run is told to stop it, on the heartbeat after the refusal. It catches a
/// failed prepare that ends the lease and leaves its container running.
#[test]
fn a_refused_prepare_asks_the_leases_runs_to_stop() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    let (_, epoch) = committed_again_with_a_run(&fx, hosts[0].1);
    assert_eq!(commands_for_listed_run(&fx, epoch), []);
    let mut message = fx.heartbeat("a", epoch);
    message.held = vec![HeldRun {
        run_id: "r1".into(),
        state: HeldState::Running,
    }];
    message.exclusive_refused = Some(ExclusiveRefusal {
        lease_id: "1-1".into(),
        code: RefusalReason::NotQuiet,
        detail: String::new(),
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert_eq!(commands_for_listed_run(&fx, epoch), stop_r1());
}

/// A prepare that times out asks the runs of the committed lease to stop, as a refusal
/// does: nothing is sent a millisecond before the deadline, the stop from the deadline.
#[test]
fn a_prepare_timeout_asks_the_leases_runs_to_stop() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    let (_, epoch) = committed_again_with_a_run(&fx, hosts[0].1);
    let mut left = TIMEOUT - 1;
    while left > 0 {
        let step = left.min(5_000);
        fx.clock.advance(Duration::from_millis(step));
        assert_eq!(commands_for_listed_run(&fx, epoch), []);
        left -= step;
    }
    fx.clock.advance(Duration::from_millis(1));
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    assert_eq!(commands_for_listed_run(&fx, epoch), stop_r1());
}

/// A booking restored on a host that lists live runs is ready already: it is not
/// overdue however long it waits, and no request is charged.
#[test]
fn a_restored_booking_with_live_runs_never_times_out() {
    let (fx, hosts) = pool(short_config(), &["a"]);
    let mut message = fx.heartbeat("a", hosts[0].1);
    message.held = vec![HeldRun {
        run_id: "r1".into(),
        state: HeldState::Running,
    }];
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    let mut provider = fx.provider.clone();
    provider.seat(&host("a"), lease(7), None).unwrap();
    wait(&fx, &hosts, TIMEOUT * 3);
    assert_eq!(
        provider.observe(&lease(7)),
        Observed::Present { host: host("a") }
    );
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(7)));
    assert!(fx.registry.request_record(&whole_request()).is_none());
}
