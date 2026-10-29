//! What the shell vouches for and tells, so that the pure step can rely on it: the order
//! of a save and the calls that depend on it, the random values for acquire ids, a registry
//! view that is complete only when the shell can say so, a silent host that books nothing,
//! machines the provider no longer has, launches the provider holds a call for, and the
//! alerts an operator must see. Time is an injected clock; nothing sleeps.

use std::collections::BTreeSet;
use std::time::Duration;

use js_lease_server::REGISTRY_QUIET;
use js_registry::{HostState, SessionSource};
use js_scaler::{Blocked, Millis, Phase, Record, Records};
use js_types::LeaseId;

use crate::complete::run_for;
use crate::fakes::{live, logged, record, records, request, rig, rig_with, view};

fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// A record of the pool `gpu` that is registered, on the provider's machine `cloud`.
fn registered(name: &str, cloud: &str) -> Record {
    Record {
        cloud_id: Some(cloud.to_owned()),
        ..record(name, Phase::Registered)
    }
}

/// How many times `needle` is in everything logged.
fn times(needle: &str) -> usize {
    logged().matches(needle).count()
}

/// Asserts that a failed save carries out no later action, whatever the action is: a
/// machine past its boot timeout would be marked ending, revoked and released, and with
/// the disk full none of it is, not the mark, not the revoke, not the release. The failure
/// is counted and the state does not move; when the disk works the same tick's decision is
/// made again and the save comes first. It catches a release or a revoke that runs ahead
/// of the record that says it was decided, which a crash would leave as a machine the
/// scaler does not know it was releasing.
#[tokio::test]
async fn a_failed_save_carries_out_no_later_action() {
    let rig = rig(
        records(vec![record("js-save-fail", Phase::Booting)]),
        live(),
    );
    rig.advance(mins(16));
    rig.world().save_ok = false;
    rig.scaler.tick().await;
    assert_eq!(rig.log(), ["save"]);
    assert_eq!(rig.scaler.save_errors(), 1);
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Booting
    );

    rig.world().save_ok = true;
    rig.scaler.tick().await;
    let log = rig.log();
    assert_eq!(
        log[..3],
        ["save", "save", "MarkEnding:js-save-fail"],
        "{log:?}"
    );
    assert!(log.contains(&"revoke:js-save-fail".to_owned()), "{log:?}");
}

/// A source of randomness that has run dry.
struct Dry;

impl SessionSource for Dry {
    fn fresh(&self) -> Option<[u8; 32]> {
        None
    }
}

/// Asserts the random values of a step: the shell gives one for each launch the step may
/// make, so with `max_step` 3 and five requests waiting three machines are launched, each
/// with its own name; and a source that gives nothing launches nothing and writes
/// nothing, the pool showing why. It catches a shell that reuses a value (two launches
/// under one acquire id would be one machine) and one that launches without randomness.
#[tokio::test]
async fn a_step_gets_one_fresh_value_for_each_launch_it_may_make() {
    let mut settings = live();
    settings.max_step = 3;
    let rig = rig(Records::default(), settings.clone());
    rig.world().waiting = (1..=5).map(|n| request(&format!("r{n}"))).collect();
    rig.scaler.tick().await;
    let names: Vec<String> = rig
        .scaler
        .state()
        .records
        .instances
        .iter()
        .map(|record| record.name.clone())
        .collect();
    assert_eq!(names.len(), 3);
    let distinct: BTreeSet<&String> = names.iter().collect();
    assert_eq!(distinct.len(), 3);
    let mints = rig.log().iter().filter(|e| e.starts_with("mint:")).count();
    assert_eq!(mints, 3);

    let dry = rig_with(Records::default(), settings, Box::new(Dry));
    dry.world().waiting = vec![request("r1")];
    dry.scaler.tick().await;
    assert!(dry.log().is_empty());
    assert_eq!(dry.scaler.state().blocked["gpu"], Blocked::Entropy);
}

/// Asserts that the shell vouches for the registry's view only once the server has been
/// up for the settle time. A machine whose host the registry does not show is held while
/// the view cannot be vouched for, a first complete view only starts the count, the count
/// needs `unknown_hold` more, and then the machine is released; and each tick asked the
/// provider whether the machine is gone. It catches a release of a machine right after a
/// restart, when its agent has not yet had the time to say hello again and the registry
/// shows nothing.
#[tokio::test]
async fn a_machine_missing_from_the_registry_is_released_only_on_a_view_the_shell_vouches_for() {
    let rig = rig(records(vec![registered("js-missing", "i-m")]), live());
    rig.scaler.tick().await;
    run_for(&rig, &rig.scaler, REGISTRY_QUIET - Duration::from_secs(5)).await;
    assert!(rig.log().is_empty(), "{:?}", rig.log());

    run_for(&rig, &rig.scaler, Duration::from_secs(5)).await;
    assert!(
        rig.log().is_empty(),
        "the first complete view only starts the count"
    );
    rig.advance(mins(30) - Duration::from_secs(1));
    rig.scaler.tick().await;
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    rig.advance(Duration::from_secs(1));
    rig.scaler.tick().await;
    rig.scaler.tick().await;
    let log = rig.log();
    assert!(log.contains(&"revoke:js-missing".to_owned()), "{log:?}");
    assert!(!rig.world().gone_asked.is_empty());
    assert!(rig.world().gone_asked.iter().all(|id| id == "i-m"));
}

/// Asserts the lease flag of a silent host. Two machines whose hosts are lost: the host
/// that books nothing is, at `unknown_hold` of silence, drained and marked for release,
/// then released once the mark is read back, and the step names it for the log, which says
/// so; the host the registry still books a lease on gets no call at all. It catches a flag
/// set from the wrong side, which would either keep a paid machine for ever or terminate
/// one that holds a lease.
#[tokio::test]
async fn a_silent_host_that_books_nothing_is_released_and_one_that_books_a_lease_is_not() {
    let each = vec![
        registered("js-silent-free", "i-f"),
        registered("js-silent-leased", "i-l"),
    ];
    let rig = rig(records(each), live());
    let mut free = view("js-silent-free");
    free.state = HostState::Lost;
    let mut leased = view("js-silent-leased");
    leased.state = HostState::Awaiting;
    leased.lease = Some(LeaseId::new(1, 7));
    rig.world().hosts = vec![free, leased];
    rig.scaler.tick().await;
    run_for(&rig, &rig.scaler, REGISTRY_QUIET).await;
    rig.advance(mins(30) - Duration::from_secs(1));
    rig.scaler.tick().await;
    assert!(rig.log().is_empty(), "{:?}", rig.log());

    rig.advance(Duration::from_secs(1));
    rig.scaler.tick().await;
    assert_eq!(
        rig.log(),
        ["save", "Drain:js-silent-free", "ReleaseHost:js-silent-free"]
    );
    assert!(rig.scaler.state().silent_released.is_empty());

    rig.advance(Duration::from_secs(5));
    rig.scaler.tick().await;
    assert_eq!(rig.scaler.state().silent_released, ["js-silent-free"]);
    assert!(logged().contains("released a machine whose host was silent"));
    assert!(times("host=js-silent-free") >= 1);
    let log = rig.log();
    assert!(log.contains(&"revoke:js-silent-free".to_owned()), "{log:?}");
    assert!(
        !log.iter().any(|entry| entry.contains("js-silent-leased")),
        "{log:?}"
    );
}

/// Asserts that a read by id settles a machine only where the registry cannot: the
/// provider is asked about a registered machine whose host is lost, and when it says gone
/// the machine's token is revoked, its network device forgotten and its host removed; it is
/// not asked about a machine whose host is live. It catches a settle on a live host's say,
/// which the step would not believe, and a read by id that is never made.
#[tokio::test]
async fn a_machine_the_provider_no_longer_has_is_settled_when_its_host_is_silent() {
    let each = vec![
        registered("js-gone-lost", "i-gone"),
        registered("js-gone-live", "i-live"),
    ];
    let rig = rig(records(each), live());
    let mut lost = view("js-gone-lost");
    lost.state = HostState::Lost;
    lost.lease = Some(LeaseId::new(1, 3));
    rig.world().hosts = vec![lost, view("js-gone-live")];
    rig.world().gone = vec!["i-gone".to_owned(), "i-live".to_owned()];
    rig.scaler.tick().await;
    assert_eq!(rig.world().gone_asked, ["i-gone"]);
    let log = rig.log();
    for call in ["revoke:", "forget:", "Remove:"] {
        let wanted = format!("{call}js-gone-lost");
        assert!(log.contains(&wanted), "{wanted} in {log:?}");
    }
    assert!(
        !log.iter().any(|entry| entry.contains("js-gone-live")),
        "{log:?}"
    );
}

/// Asserts that the shell tells the step which acquire ids the provider holds a call for.
/// A launch told to wait that no request waits for any more is forgotten, as its token
/// is revoked; the same launch is kept, untouched, while the provider holds an unresolved
/// call for its acquire id. It catches a launch dropped while its machine may exist, which
/// would show up later as an orphan nobody releases.
#[tokio::test]
async fn a_planned_launch_is_kept_while_the_provider_holds_a_call_for_it() {
    let planned = Record {
        retry_at: Some(Millis::new(crate::fakes::START + 600_000)),
        ..record("js-planned", Phase::Planned)
    };
    let kept = rig(records(vec![planned.clone()]), live());
    kept.world().unresolved = vec![planned.acquire.clone().unwrap()];
    kept.scaler.tick().await;
    assert!(kept.log().is_empty(), "{:?}", kept.log());
    assert_eq!(kept.scaler.state().records.instances.len(), 1);

    let dropped = rig(records(vec![planned]), live());
    dropped.scaler.tick().await;
    assert!(dropped.scaler.state().records.instances.is_empty());
    assert!(
        dropped.log().contains(&"revoke:js-planned".to_owned()),
        "{:?}",
        dropped.log()
    );
}

/// Asserts the alerts. A machine of ours that no record knows (an orphan), a record of a
/// pool the server does not have, and a silent host that books nothing and is still not
/// marked releasing past twice `unknown_hold` (it finishes runs, so the step leaves it) are
/// each logged, and each only once however many ticks it holds for. It catches an alert
/// that is never raised, and one that repeats on every tick.
#[tokio::test]
async fn orphans_records_of_unknown_pools_and_stuck_silent_hosts_are_said_once() {
    let orphan = Record {
        name: String::new(),
        cloud_id: Some("i-orphan-alert".to_owned()),
        ..record("unused", Phase::Orphan)
    };
    let stray = Record {
        pool: "elsewhere".to_owned(),
        ..registered("js-stray-alert", "i-stray")
    };
    let stuck = registered("js-stuck-alert", "i-stuck");
    let rig = rig(records(vec![orphan, stray, stuck]), live());
    rig.world().machines = vec![js_elastic::Held {
        id: js_elastic::HostRef("i-orphan-alert".to_owned()),
        acquire: "strange".to_owned(),
        name: None,
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: js_elastic::HostStatus::Active,
    }];
    let mut finishing = view("js-stuck-alert");
    finishing.state = HostState::Lost;
    finishing.releasing = true;
    rig.world().hosts = vec![finishing];
    rig.scaler.tick().await;
    rig.scaler.tick().await;
    assert_eq!(times("i-orphan-alert"), 1);
    assert_eq!(times("js-stray-alert"), 1);
    assert_eq!(times("js-stuck-alert"), 0);

    rig.advance(mins(60));
    rig.scaler.tick().await;
    rig.scaler.tick().await;
    assert_eq!(times("i-orphan-alert"), 1);
    assert_eq!(times("js-stray-alert"), 1);
    assert_eq!(times("host js-stuck-alert has been silent"), 1);
}
