//! The host lifetime through the scaler's shell, over a fake world: a launch carries the
//! cloud lifetime to the provider, a busy host is retired at `max_age` with its job left
//! alone, nothing is released until the job is over, and a restarted shell retires again
//! what a crash left half done. Time is an injected clock; nothing sleeps.

use std::time::Duration;

use js_registry::HostMark;
use js_scaler::{Phase, Settings};

use crate::fakes::{allocation, job, live, rig, rig_with, view};

fn mins(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A live pool with an hour of `max_age` and a lease ceiling of half an hour.
fn bounded() -> Settings {
    Settings {
        max_age: Some(mins(60)),
        max_lease_wall: Some(mins(30)),
        ..live()
    }
}

/// Asserts that a launch reaches the provider with `max_age` plus the ceiling plus the
/// prepare timeout, `unknown_hold` and margin, 140 minutes here, and that a pool with no `max_age` launches with none.
/// It catches a lifetime lost between the step and the provider's request.
#[tokio::test]
async fn a_launch_hands_the_provider_the_cloud_lifetime() {
    let bound = rig(Default::default(), bounded());
    bound.world().queued = vec![job("r1")];
    bound.scaler.tick().await;
    assert_eq!(bound.world().lifetimes, [Some(mins(140))]);

    let plain = rig(Default::default(), live());
    plain.world().queued = vec![job("r1")];
    plain.scaler.tick().await;
    assert_eq!(plain.world().lifetimes, [None]);
}

/// Asserts the way down for a host that holds a job for good: at `max_age` the shell
/// retires it (the registry marks it draining and the job's allocation stays live), nothing
/// is released or asked to be released while the allocation is live however many ticks
/// pass, and when it is freed the host is marked for release and the machine released; the
/// fake provider logs any release of a host that could still take work, and there is none.
/// It catches a retire that waits for quiet, one that cuts the job, and a release of a busy
/// host.
#[tokio::test]
async fn a_busy_host_is_retired_at_max_age_and_released_only_when_its_job_ends() {
    let rig = rig(Default::default(), bounded());
    rig.world().queued = vec![job("r1")];
    rig.scaler.tick().await;
    rig.scaler.tick().await;
    let name = rig.scaler.state().records.instances[0].name.clone();
    rig.world().queued.clear();
    rig.world().hosts = vec![view(&name)];
    rig.world().live = vec![allocation("j-busy", &name)];
    rig.scaler.tick().await;
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Registered
    );

    rig.advance(mins(60) - secs(1));
    rig.scaler.tick().await;
    assert!(!rig.log().iter().any(|entry| entry.starts_with("Retire")));
    rig.advance(secs(1));
    rig.scaler.tick().await;
    assert!(rig.log().contains(&format!("Retire:{name}")));
    assert_eq!(rig.world().hosts[0].mark, Some(HostMark::Draining));
    assert_eq!(rig.world().live.len(), 1);
    assert_eq!(
        rig.scaler.state().records.instances[0].phase,
        Phase::Draining
    );

    let before = rig.log().len();
    for _ in 0..10 {
        rig.advance(mins(5));
        rig.scaler.tick().await;
    }
    assert_eq!(rig.log().len(), before, "{:?}", rig.log());
    assert_eq!(rig.world().live.len(), 1);

    rig.world().live.clear();
    for _ in 0..8 {
        rig.scaler.tick().await;
        rig.advance(secs(5));
    }
    let log = rig.log();
    assert!(log.contains(&format!("ReleaseHost:{name}")), "{log:?}");
    assert!(log.contains(&format!("release:{name}")), "{log:?}");
    assert!(
        !log.iter().any(|entry| entry.starts_with("UNSAFE")),
        "{log:?}"
    );
    assert_eq!(rig.scaler.state().records.instances[0].phase, Phase::Gone);
}

/// Asserts that a retire lost in a crash is made again by the shell of a restarted server:
/// the first shell wrote its records with the host draining, its retire never reached a
/// registry that kept no mark, and a second shell started on those records, with the
/// clock moved past the age as time passes while a server is down, retires the busy host at
/// its first tick. It catches a shell that takes the records' draining phase for a retire
/// done.
#[tokio::test]
async fn a_restarted_shell_retires_again_what_a_crash_left_half_done() {
    let first = rig(Default::default(), bounded());
    first.world().queued = vec![job("r1")];
    first.scaler.tick().await;
    first.scaler.tick().await;
    let name = first.scaler.state().records.instances[0].name.clone();
    first.world().queued.clear();
    let host = view(&name);
    first.world().hosts = vec![host.clone()];
    first.world().live = vec![allocation("j-busy", &name)];
    first.scaler.tick().await;
    first.advance(mins(60));
    first.scaler.tick().await;
    assert!(first.log().contains(&format!("Retire:{name}")));
    let written = first.world().saved.clone().unwrap();
    assert_eq!(written.instances[0].phase, Phase::Draining);

    let second = rig_with(
        written,
        bounded(),
        Box::new(js_registry::SequentialSessions::new()),
    );
    second.world().hosts = vec![host];
    second.advance(mins(60));
    second.scaler.tick().await;
    assert!(
        second.log().contains(&format!("Retire:{name}")),
        "{:?}",
        second.log()
    );
    assert_eq!(second.world().hosts[0].mark, Some(HostMark::Draining));
}
