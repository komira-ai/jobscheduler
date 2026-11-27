//! The scaler's shell over a fake world: what it writes first, what it does when a call
//! fails, what it remembers for the next tick, and what it does before the first tick of a
//! restarted server. Time is an injected clock; nothing sleeps.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use js_elastic::HostStatus;
use js_registry::{HostMark, HostState};
use js_scaler::{Blocked, CooldownReason, Health, LaunchMode, Phase, Records, Settings, State};

use crate::fakes::{Gate, allocation, job, live, only_saves, record, records, rig, view};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// The name of the first machine the scaler has a record of.
fn first_name(state: &State) -> String {
    state.records.instances[0].name.clone()
}

/// Asserts the order of a launch: the records are written, then the acquire is made, and
/// the next ticks, with the request still waiting, make no call for it except to write the
/// record that the answer moved on. It catches a call made before its record is on disk,
/// and a second launch for a request that already has a machine coming.
#[tokio::test]
async fn a_waiting_request_is_launched_once_with_its_records_written_first() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("r1")];
    rig.scaler.tick().await;
    let name = first_name(&rig.scaler.state());
    let expected = ["save".to_owned(), format!("acquire:{name}")];
    assert_eq!(rig.log(), expected);
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Launching
    );
    assert_eq!(rig.world().saved.as_ref().unwrap().instances.len(), 1);

    rig.advance(secs(5));
    rig.scaler.tick().await;
    rig.advance(secs(5));
    rig.scaler.tick().await;
    let state = rig.scaler.state();
    assert_eq!(state.records.instances.len(), 1);
    assert_eq!(state.records.instances[0].phase, Phase::Booting);
    assert_eq!(state.records.instances[0].cloud_id.as_deref(), Some("i-1"));
    let mut expected = expected.to_vec();
    expected.push("save".to_owned());
    assert_eq!(rig.log(), expected);
}

/// Asserts that records that cannot be written stop the tick: nothing else is called,
/// the failure is counted, and the step's decision is dropped, so the next tick decides
/// again from the same state and, once the disk works, launches once. It catches a launch
/// whose record is not on disk, and a state that moves on although the file did not.
#[tokio::test]
async fn records_that_cannot_be_written_stop_the_tick() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("r1")];
    rig.world().save_ok = false;
    rig.scaler.tick().await;
    assert_eq!(rig.log(), ["save"]);
    assert_eq!(rig.scaler.save_errors(), 1);
    assert!(rig.scaler.state().records.instances.is_empty());

    rig.world().save_ok = true;
    rig.scaler.tick().await;
    assert_eq!(rig.log().len(), 3);
    assert_eq!(rig.scaler.save_errors(), 1);
    assert_eq!(rig.scaler.state().records.instances.len(), 1);
}

/// Asserts the fix for a release that was under way at a restart. The records name a
/// machine being released, one registered and one gone; before any tick, and so before the
/// server admits a hello, the machine being released is marked ending in the registry and
/// the others are not. The first tick then releases it. It catches a restart that leaves
/// such a host to say hello unmarked.
#[tokio::test]
async fn a_restart_marks_what_was_being_released_before_anything_else() {
    let each = vec![
        record("js-leaving", Phase::Releasing),
        record("js-staying", Phase::Registered),
        record("js-gone", Phase::Gone),
    ];
    let rig = rig(records(each), live());
    assert_eq!(rig.log(), ["MarkEnding:js-leaving"]);

    rig.world().hosts = vec![view("js-leaving"), view("js-staying")];
    rig.scaler.tick().await;
    let log = rig.log();
    assert_eq!(log[1], "release:");
    assert!(!log.iter().any(|entry| entry.contains("UNSAFE")), "{log:?}");
}

/// Asserts that a pool whose checks do not pass, or whose list cannot be read, launches
/// nothing and says why: while the checks have not finished (`Checking`) and while one
/// fails, and while the provider's list fails, though the checks pass. The offers are the
/// checks' and the shell asks the provider for none. It catches a failed list read as an
/// empty one (which would launch without seeing what exists), and a launch while the
/// checks fail.
#[tokio::test]
async fn a_pool_whose_checks_or_list_fail_launches_nothing() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("r1")];
    rig.world().health = Health::Checking;
    rig.scaler.tick().await;
    assert!(rig.log().is_empty());
    assert_eq!(rig.scaler.state().blocked["gpu"], Blocked::Checking);

    rig.world().health = Health::Failing;
    rig.scaler.tick().await;
    assert!(rig.log().is_empty());
    assert_eq!(rig.scaler.state().blocked["gpu"], Blocked::Unavailable);

    rig.world().health = Health::Available;
    rig.world().list_fails = true;
    rig.scaler.tick().await;
    assert!(rig.log().is_empty());
    assert_eq!(rig.scaler.state().blocked["gpu"], Blocked::ListFailed);

    rig.world().list_fails = false;
    rig.scaler.tick().await;
    assert_eq!(rig.log().len(), 2);
}

/// Asserts that a pool in `plan` mode calls nothing and writes nothing however long it
/// runs and however much waits: no acquire, no record. It shows what it would
/// launch, and that it is in plan. It catches a launch from a plan pool in the shell, as
/// the provider's own lock would in the pool itself.
#[tokio::test]
async fn a_plan_pool_launches_nothing() {
    let plan = Settings {
        launch: LaunchMode::Plan,
        ..live()
    };
    let rig = rig(Records::default(), plan);
    rig.world().queued = vec![job("r1"), job("r2")];
    for _ in 0..5 {
        rig.scaler.tick().await;
        rig.advance(mins(1));
    }
    assert!(rig.log().is_empty());
    assert!(rig.world().saved.is_none());
    let state = rig.scaler.state();
    assert!(state.records.instances.is_empty());
    assert_eq!(state.blocked["gpu"], Blocked::Plan);
    assert_eq!(state.plan.len(), 1);
    assert_eq!(state.plan[0].offer, "a10");
}

/// Asserts a machine's whole life through the shell, over a registry that marks the way
/// the real one does: launched, booted, registered, idle for `idle`, drained, marked
/// releasing, terminated, read back terminating and then gone, removed from the
/// registry and forgotten. No release is made of a host that is not marked releasing and
/// quiet, the removal is after the last release, and no tick after that calls anything. It
/// catches an order of calls that could cut work.
#[tokio::test]
async fn a_machine_is_launched_registered_idle_drained_released_and_removed() {
    let rig = rig(Records::default(), live());
    rig.world().queued = vec![job("r1")];
    rig.scaler.tick().await;
    rig.scaler.tick().await;
    let name = first_name(&rig.scaler.state());
    rig.world().queued.clear();
    rig.world().hosts = vec![view(&name)];
    rig.scaler.tick().await;
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );

    rig.advance(mins(10) - secs(1));
    rig.scaler.tick().await;
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );
    rig.advance(secs(1));
    for _ in 0..8 {
        rig.scaler.tick().await;
        rig.advance(secs(5));
    }
    let state = rig.scaler.state();
    assert_eq!(state.records.instances[0].phase, Phase::Gone);
    let log = rig.log();
    let at = |entry: String| log.iter().position(|each| *each == entry);
    let drain = at(format!("Drain:{name}")).unwrap();
    let release_host = at(format!("ReleaseHost:{name}")).unwrap();
    let release = at(format!("release:{name}")).unwrap();
    let remove = at(format!("remove:{name}")).unwrap();
    assert!(drain < release_host && release_host < release);
    assert!(release < remove);
    assert!(at(format!("forget:{name}")).is_some());
    let released = log
        .iter()
        .filter(|each| each.starts_with("release:"))
        .count();
    assert_eq!(released, 2, "terminating, then gone");
    assert!(!log.iter().any(|entry| entry.contains("UNSAFE")), "{log:?}");
    assert!(rig.world().hosts.is_empty());
    assert!(rig.world().machines.is_empty());

    let before = rig.log().len();
    rig.scaler.tick().await;
    assert_eq!(rig.log().len(), before);
}

/// Asserts that a host that holds work is not drained and one that a request fits is
/// taken back: a registered host with a live job allocation is left alone for any time; a
/// drained host that a queued job arrives for is undrained and no machine is launched for
/// it. It catches a drain that ignores the job allocation on the host, and a launch for a
/// job a drained host could serve.
#[tokio::test]
async fn a_busy_host_is_kept_and_a_drained_host_is_taken_back_for_a_request() {
    let rig = rig(records(vec![record("js-held", Phase::Registered)]), live());
    rig.world().hosts = vec![view("js-held")];
    rig.world().live = vec![allocation("j-held", "js-held")];
    rig.scaler.tick().await;
    rig.advance(mins(60));
    rig.scaler.tick().await;
    assert!(rig.log().is_empty());

    rig.world().live.clear();
    rig.scaler.tick().await;
    rig.advance(mins(10));
    rig.scaler.tick().await;
    assert_eq!(rig.log(), ["save", "Drain:js-held"]);
    assert_eq!(rig.world().hosts[0].mark, Some(HostMark::Draining));

    rig.world().queued = vec![job("r1")];
    rig.scaler.tick().await;
    let log = rig.log();
    assert_eq!(log.last().unwrap(), "Undrain:js-held");
    assert!(
        !log.iter().any(|entry| entry.starts_with("acquire:")),
        "{log:?}"
    );
    assert_eq!(rig.world().hosts[0].mark, None);
}

/// Asserts that a machine the provider lists as terminated is released whatever the
/// registry says, that the list's status reaches the step, and that a machine the shell
/// has never heard of is adopted as an orphan, counted and left alone. It catches a list
/// that is read and not used.
#[tokio::test]
async fn the_providers_list_reaches_the_step() {
    let rig = rig(records(vec![record("js-lost", Phase::Registered)]), live());
    rig.world().hosts = vec![view("js-lost")];
    rig.world().machines = vec![js_elastic::Held {
        id: js_elastic::HostRef("i-9".to_owned()),
        acquire: "gpu-js-lost".to_owned(),
        name: Some("js-lost".to_owned()),
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Terminated,
        ends_unix_seconds: None,
    }];
    rig.world().machines.push(js_elastic::Held {
        id: js_elastic::HostRef("i-77".to_owned()),
        acquire: "stranger".to_owned(),
        name: None,
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
        ends_unix_seconds: None,
    });
    rig.scaler.tick().await;
    let state = rig.scaler.state();
    let phases: Vec<Phase> = state.records.instances.iter().map(|r| r.phase).collect();
    assert_eq!(phases, [Phase::Releasing, Phase::Orphan]);
    assert!(
        rig.log().contains(&"release:js-lost".to_owned()),
        "{:?}",
        rig.log()
    );
}

/// Asserts that hosts the scaler must leave alone are left alone, whatever the registry
/// says of them: two machines of the pool whose hosts are silent (lost, and waiting to say
/// hello again after a restart) and still hold a job allocation are held and never released
/// on their silence, however long (silence frees no allocation), and hosts it did not launch (set aside for a request, finishing
/// runs) are not drained or released. It catches a release on
/// silence alone, which would terminate a machine that may hold work the registry cannot
/// see.
#[tokio::test]
async fn silent_hosts_that_hold_jobs_and_hosts_it_did_not_launch_are_left_alone() {
    let each = vec![
        record("js-lost", Phase::Registered),
        record("js-waiting", Phase::Registered),
    ];
    let rig = rig(records(each), live());
    let mut lost = view("js-lost");
    lost.state = HostState::Lost;
    let mut awaiting = view("js-waiting");
    awaiting.state = HostState::Awaiting;
    rig.world().live = vec![
        allocation("j-lost", "js-lost"),
        allocation("j-waiting", "js-waiting"),
    ];
    let mut reserved = view("other-1");
    reserved.mark = Some(HostMark::Reserved);
    let mut held = view("other-2");
    held.mark = Some(HostMark::Held);
    let mut finishing = view("other-3");
    finishing.releasing = true;
    rig.world().hosts = vec![lost, awaiting, reserved, held, finishing];
    rig.scaler.tick().await;
    rig.advance(mins(600));
    rig.scaler.tick().await;
    assert!(only_saves(&rig.log()), "{:?}", rig.log());
    assert!(
        rig.scaler
            .state()
            .records
            .instances
            .iter()
            .all(|r| r.phase == Phase::Registered)
    );
}

/// Asserts that the scaler's state and its count of failed saves, which the status page and
/// the checks read, are answered while a tick is inside a provider's call: the call is held
/// at a gate the test opens, and the reads must finish within five seconds before it does.
/// It catches a tick that holds the lock of the shell's memory across a provider call,
/// which stalls the status page for as long as the call takes.
#[tokio::test]
async fn the_state_is_read_while_a_provider_call_is_blocked() {
    let rig = rig(records(vec![]), live());
    let (entered, in_list) = mpsc::channel();
    let (open, release) = mpsc::channel();
    rig.world().gate = Some(Arc::new(Gate {
        entered: Mutex::new(entered),
        release: Mutex::new(release),
    }));
    let scaler = rig.scaler.clone();
    let tick = tokio::spawn(async move { scaler.tick().await });
    let waited = tokio::task::spawn_blocking(move || in_list.recv_timeout(secs(5)));
    waited
        .await
        .unwrap()
        .expect("the tick reached the provider");

    let reader = rig.scaler.clone();
    let (read, answered) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = (reader.state(), reader.save_errors());
        let _ = read.send(());
    });
    let waited = tokio::task::spawn_blocking(move || answered.recv_timeout(secs(5)));
    let answer = waited.await.unwrap();
    open.send(()).unwrap();
    tick.await.unwrap();
    assert!(
        answer.is_ok(),
        "the state was not read while the call was held"
    );
}

/// Asserts that a live host which has announced no shape reaches the step with none, not
/// with the pool's floor: a host that announced a shape which fits the waiting request
/// covers it and nothing is launched, and the same host unannounced covers nothing, so a
/// machine is launched for the request. The floor of the view would fit this request, so a
/// stand-in for the missing shape would show as a request covered. It catches a shell that
/// falls back to the floor for a host it has not heard announce.
#[tokio::test]
async fn an_unannounced_host_reaches_the_step_with_no_shape() {
    let announced = rig(Records::default(), live());
    announced.world().hosts = vec![view("js-announced")];
    announced.world().queued = vec![job("r1")];
    announced.scaler.tick().await;
    assert!(announced.log().is_empty(), "{:?}", announced.log());

    let silent = rig(Records::default(), live());
    let mut unannounced = view("js-unannounced");
    unannounced.claim = None;
    silent.world().hosts = vec![unannounced];
    silent.world().queued = vec![job("r1")];
    silent.scaler.tick().await;
    let log = silent.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], "save");
    assert!(log[1].starts_with("acquire:"), "{log:?}");
}

/// A rig whose machine was launched at the start of the clock, said hello and has not been
/// heard from since: the host reads silent, and `heard_since_hello` is `heard`.
fn hello_admitted(heard: bool) -> crate::fakes::Rig {
    let mut lost = record("js-lost", Phase::Registered);
    lost.seen_live = false;
    let rig = rig(records(vec![lost]), live());
    let mut host = view("js-lost");
    host.state = HostState::Lost;
    host.heard_since_hello = heard;
    rig.world().hosts = vec![host];
    rig
}

/// Asserts that a machine whose hello was admitted but whose Welcome was lost, so it never
/// heartbeats, is left alone until `boot_timeout` after its launch and is then released
/// with the boot-failure cooldown started. It catches a shell that tells the step every
/// host has been heard since its hello, which holds the machine for `unknown_hold` instead.
#[tokio::test]
async fn a_lost_welcome_is_released_at_boot_timeout() {
    let rig = hello_admitted(false);
    let boot_timeout = live().boot_timeout;
    rig.advance(boot_timeout - Duration::from_millis(1));
    rig.scaler.tick().await;
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );
    assert!(rig.scaler.state().records.cooldowns.is_empty());

    rig.advance(Duration::from_millis(1));
    rig.scaler.tick().await;
    let state = rig.scaler.state();
    assert_eq!(state.records.instances[0].phase, Phase::Releasing);
    assert_eq!(
        state.records.cooldowns["gpu"].reason,
        CooldownReason::BootFailed
    );
}

/// Asserts that the same silent host, once it has heartbeated since its hello, is not
/// released at `boot_timeout`. It catches a shell that reads silence alone as a failed boot.
#[tokio::test]
async fn a_host_heard_since_its_hello_is_not_released_at_boot_timeout() {
    let rig = hello_admitted(true);
    rig.advance(live().boot_timeout);
    rig.scaler.tick().await;
    let state = rig.scaler.state();
    assert_eq!(state.records.instances[0].phase, Phase::Registered);
    assert!(state.records.cooldowns.is_empty());
}
