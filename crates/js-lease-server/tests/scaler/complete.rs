//! When the shell vouches for the registry's view of its hosts, after a restart of the
//! server: not before every host it knows of has had the time to say hello again, and
//! never later than the quiet time. Time is an injected clock; nothing sleeps. Each test
//! starts a scaler at the current time, as a restarted server does, and ticks it every
//! `STEP`.

use std::time::Duration;

use js_elastic::SchedulerId;
use js_lease::{Booking, LeaseState};
use js_lease_server::{REGISTRY_FLOOR, REGISTRY_QUIET, Scaler, SharedClock};
use js_registry::SequentialSessions;
use js_scaler::{Phase, Records};
use js_types::{ExecMode, HostId, LeaseId};

use crate::fakes::{Handle, Rig, live, record, records, rig, view};

/// The time between two ticks.
const STEP: Duration = Duration::from_secs(5);

/// The boot hold of the test pool: a third of its boot timeout of 15 minutes.
const BOOT_HOLD: Duration = Duration::from_secs(5 * 60);

/// A scaler over the rig's world and clock, started now over `records`, with the lease
/// file naming `leased`: what a restarted server builds, and then tells that it listens.
fn restart(rig: &Rig, records: Records, leased: &[&str]) -> Scaler {
    restart_capped(rig, records, leased, None)
}

/// [`restart`] with the production cap on what one tick adds to the uptime.
fn restart_capped(rig: &Rig, records: Records, leased: &[&str], cap: Option<Duration>) -> Scaler {
    let scaler = Scaler::new(
        Box::new(Handle(rig.world.clone())),
        SharedClock::new(rig.clock.clone()),
        Box::new(SequentialSessions::new()),
        (SchedulerId::new("sched").unwrap(), ExecMode::Container),
        live(),
        records,
    );
    let booked: Vec<Booking> = leased
        .iter()
        .enumerate()
        .map(|(n, host)| Booking {
            lease: LeaseId::new(1, n as u64 + 1),
            host: HostId::new(host).unwrap(),
            state: LeaseState::Held,
        })
        .collect();
    scaler.listening(cap, &booked);
    scaler
}

/// Ticks `scaler` every `STEP` for `span`.
pub async fn run_for(rig: &Rig, scaler: &Scaler, span: Duration) {
    for _ in 0..(span.as_secs() / STEP.as_secs()) {
        rig.advance(STEP);
        scaler.tick().await;
    }
}

/// Ticks `scaler` every `STEP` until it vouches for the view, and says how long that
/// took from its first tick. Fails if it never does.
async fn until_complete(rig: &Rig, scaler: &Scaler) -> Duration {
    scaler.tick().await;
    let mut elapsed = Duration::ZERO;
    while !scaler.registry_complete() {
        assert!(elapsed < 2 * REGISTRY_QUIET, "the view never completes");
        run_for(rig, scaler, STEP).await;
        elapsed += STEP;
    }
    elapsed
}

/// A pool of one machine that was launched before the restart, is past its boot timeout
/// and whose host the registry does not show.
fn overdue() -> Records {
    records(vec![record("js-boot", Phase::Booting)])
}

fn first_phase(scaler: &Scaler) -> Phase {
    scaler.state().records.instances[0].phase
}

/// Asserts (a), the host says hello late: a machine launched 16 minutes before the
/// restart, whose host is not shown, is held while the view is incomplete, and its hello
/// one tick before the floor registers it: no release, no cooldown. It catches the
/// release of a healthy machine whose agent was still in its longest backoff when the
/// view began to count, which a settle as short as that backoff does.
#[tokio::test]
async fn a_hello_just_before_the_floor_registers_a_machine_past_its_boot_timeout() {
    let rig = rig(Records::default(), live());
    rig.advance(Duration::from_secs(16 * 60));
    let scaler = restart(&rig, overdue(), &[]);
    scaler.tick().await;
    run_for(&rig, &scaler, REGISTRY_FLOOR - 2 * STEP).await;
    assert!(!scaler.registry_complete());
    assert_eq!(first_phase(&scaler), Phase::Booting);

    rig.world().hosts = vec![view("js-boot")];
    run_for(&rig, &scaler, STEP).await;
    assert!(!scaler.registry_complete());
    run_for(&rig, &scaler, STEP).await;
    assert!(scaler.registry_complete());
    assert_eq!(first_phase(&scaler), Phase::Registered);
    assert!(scaler.state().records.cooldowns.is_empty());
    let log = rig.log();
    assert!(
        !log.iter().any(|entry| entry.starts_with("revoke:")),
        "{log:?}"
    );
}

/// Asserts (a), no hello at all: the same machine is not released before the quiet time,
/// and then only once its host has also been missing from complete views for the boot hold
/// (a third of the boot timeout, five minutes), counted from the first complete view. It
/// catches a view that never completes (a dead machine would be paid for for ever), one
/// that completes at the floor with the host unheard, and a release at the first complete
/// view.
#[tokio::test]
async fn a_machine_nobody_hears_from_is_released_once_the_quiet_time_has_passed() {
    let rig = rig(Records::default(), live());
    rig.advance(Duration::from_secs(16 * 60));
    let scaler = restart(&rig, overdue(), &[]);
    scaler.tick().await;
    run_for(&rig, &scaler, REGISTRY_QUIET - STEP).await;
    assert!(!scaler.registry_complete());
    assert_eq!(first_phase(&scaler), Phase::Booting);

    run_for(&rig, &scaler, STEP).await;
    assert!(scaler.registry_complete());
    assert_eq!(first_phase(&scaler), Phase::Booting);

    run_for(&rig, &scaler, BOOT_HOLD - STEP).await;
    assert_eq!(first_phase(&scaler), Phase::Booting);
    run_for(&rig, &scaler, STEP).await;
    assert_eq!(first_phase(&scaler), Phase::Releasing);
}

/// Asserts (b): a host the old process heard does not count as heard by the new one. A
/// registered machine whose host said hello to the first scaler holds the second's view
/// open past the floor, and the view completes at the tick that shows the host live; and
/// when the host stays silent the quiet time completes it. It catches a heard set that
/// outlives the restart, or a roster that leaves out the scaler's own records.
#[tokio::test]
async fn a_restarted_scaler_waits_for_a_host_the_old_one_had_heard() {
    let rig = rig(Records::default(), live());
    let each = || records(vec![record("js-a", Phase::Registered)]);
    rig.world().hosts = vec![view("js-a")];
    let before = restart(&rig, each(), &[]);
    assert_eq!(until_complete(&rig, &before).await, REGISTRY_FLOOR);

    rig.world().hosts.clear();
    let after = restart(&rig, each(), &[]);
    after.tick().await;
    run_for(&rig, &after, REGISTRY_FLOOR).await;
    assert!(!after.registry_complete());
    rig.world().hosts = vec![view("js-a")];
    run_for(&rig, &after, STEP).await;
    assert!(after.registry_complete());

    rig.world().hosts.clear();
    let silent = restart(&rig, each(), &[]);
    assert_eq!(until_complete(&rig, &silent).await, REGISTRY_QUIET);
}

/// Asserts (c): a server restarted twice a minute apart counts the second time from its
/// own start. With nothing to wait for, the first completes at the floor, and the second
/// is incomplete at the floor less a tick after the minute the first had already run. It
/// catches a start that is taken once for the process, or from the first scaler.
#[tokio::test]
async fn a_second_restart_a_minute_later_counts_from_its_own_start() {
    let rig = rig(Records::default(), live());
    let first = restart(&rig, Records::default(), &[]);
    assert_eq!(until_complete(&rig, &first).await, REGISTRY_FLOOR);

    rig.advance(Duration::from_secs(60));
    let second = restart(&rig, Records::default(), &[]);
    second.tick().await;
    assert!(!second.registry_complete());
    run_for(&rig, &second, REGISTRY_FLOOR - STEP).await;
    assert!(!second.registry_complete());
    run_for(&rig, &second, STEP).await;
    assert!(second.registry_complete());
}

/// Asserts (d): a host of the lease file whose seat the registry refuses is never shown,
/// and still holds the view open until the quiet time, with no record of the scaler for
/// it; heard, it holds nothing. It catches a roster of the scaler's own machines only,
/// under which the booked host would read as missing the moment the floor passed.
#[tokio::test]
async fn a_leased_host_that_is_never_heard_holds_the_view_until_the_quiet_time() {
    let rig = rig(Records::default(), live());
    let refused = restart(&rig, Records::default(), &["js-refused"]);
    assert_eq!(until_complete(&rig, &refused).await, REGISTRY_QUIET);

    rig.world().hosts = vec![view("js-refused")];
    let heard = restart(&rig, Records::default(), &["js-refused"]);
    assert_eq!(until_complete(&rig, &heard).await, REGISTRY_FLOOR);
}

/// Asserts that a forward leap of the wall clock is not uptime: the server is told to
/// cap what a tick adds at twice its tick interval, and an hour's leap right after the
/// bind counts as the cap. The view is incomplete until the floor of counted time has
/// passed, the first counted cap included. It catches an NTP or RTC correction at boot
/// that completes the view at once and lets a booting machine be released.
#[tokio::test]
async fn a_leap_of_the_wall_clock_is_not_counted_as_uptime() {
    let cap = 2 * STEP;
    let rig = rig(Records::default(), live());
    let scaler = restart_capped(&rig, Records::default(), &[], Some(cap));
    rig.advance(Duration::from_secs(3600));
    scaler.tick().await;
    assert!(!scaler.registry_complete());
    assert_eq!(until_complete(&rig, &scaler).await, REGISTRY_FLOOR - cap);
}
