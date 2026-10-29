//! The boot deadline, the wall-clock cap and the spend cap, each forcing teardown on
//! the injected clock, and the bounded termination.

use std::time::Duration;

use js_elastic::HostStatus;
use js_provider_lambdalabs::{ApiError, Call, TerminateEffect};

use crate::rig::{FIRST, Rig};

fn never_active(rig: &Rig, extra: &[&str]) -> (i32, String) {
    let args = rig.go_args(extra);
    rig.exec(&args)
}

#[test]
fn the_boot_deadline_forces_teardown() {
    let rig = Rig::new();
    let (status, out) = never_active(&rig, &["--boot-deadline-secs", "60"]);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the boot deadline passed"),
        "{out}"
    );
    assert_eq!(rig.clock_secs(), 60);
    assert_eq!(rig.status_of(FIRST), HostStatus::Terminating);
    assert!(out.contains("STOP: the run did not reach active; the instance is terminated"));
}

#[test]
fn the_wall_clock_cap_forces_teardown_before_a_later_boot_deadline() {
    let rig = Rig::new();
    let (status, out) = never_active(&rig, &["--wall-cap-secs", "30"]);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the wall-clock cap is reached"),
        "{out}"
    );
    assert_eq!(rig.clock_secs(), 30);
    assert_eq!(rig.status_of(FIRST), HostStatus::Terminating);
}

#[test]
fn the_spend_cap_from_the_api_price_forces_teardown() {
    let rig = Rig::new();
    // 75 cents an hour: one cent is reached after 48 s, and the poll that sees it is
    // the one at 50 s.
    let (status, out) = never_active(&rig, &["--spend-cap-cents", "1"]);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the spend cap is reached"),
        "{out}"
    );
    assert_eq!(rig.clock_secs(), 50);
    assert_eq!(rig.status_of(FIRST), HostStatus::Terminating);
}

#[test]
fn the_spend_cap_is_named_when_both_caps_are_reached() {
    let rig = Rig::new();
    let (_, out) = never_active(&rig, &["--spend-cap-cents", "1", "--wall-cap-secs", "40"]);
    assert!(
        out.contains("teardown forced: the wall-clock cap is reached"),
        "{out}"
    );
    let rig = Rig::new();
    let (_, out) = never_active(&rig, &["--spend-cap-cents", "1", "--wall-cap-secs", "50"]);
    assert!(
        out.contains("teardown forced: the spend cap is reached"),
        "{out}"
    );
}

#[test]
fn a_cap_reached_by_the_time_the_instance_is_active_still_forces_teardown() {
    let rig = Rig::new();
    let args = rig.go_args(&["--wall-cap-secs", "5"]);
    let (status, out) = rig.exec_active(&args, 1);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the wall-clock cap is reached"),
        "{out}"
    );
    assert_eq!(rig.status_of(FIRST), HostStatus::Terminating);
}

#[test]
fn a_termination_that_never_shows_is_tried_a_bounded_number_of_times() {
    let rig = Rig::new();
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    let (status, out) = never_active(&rig, &["--boot-deadline-secs", "10"]);
    assert_eq!(status, 1, "{out}");
    assert_eq!(rig.fake.count(Call::Terminate), 5);
    assert!(out.contains("result: TERMINATION NOT CONFIRMED"), "{out}");
    assert!(
        out.contains("STOP: AN INSTANCE MAY STILL BE RUNNING AND BILLING"),
        "{out}"
    );
    let pauses = rig
        .sleeps
        .borrow()
        .iter()
        .filter(|d| **d == Duration::from_secs(10))
        .count();
    // One poll pause before the deadline, then four pauses between five tries.
    assert_eq!(pauses, 1 + 4);
    assert_ne!(rig.status_of(FIRST), HostStatus::Terminating);
}

#[test]
fn a_termination_that_fails_twice_is_retried_and_confirmed_on_the_third_try() {
    let rig = Rig::new();
    let down = || ApiError::Transport("down".to_owned());
    rig.fake.fail_next(Call::Terminate, down());
    rig.fake.fail_next(Call::Terminate, down());
    let (status, out) = rig.exec_active(&rig.go_args(&[]), 1);
    assert_eq!(status, 0, "{out}");
    assert_eq!(rig.fake.count(Call::Terminate), 3);
    assert_eq!(rig.status_of(FIRST), HostStatus::Terminating);
}

#[test]
fn an_instance_the_api_reads_as_gone_counts_as_terminated() {
    let rig = Rig::new();
    rig.fake.set_terminate_effect(TerminateEffect::Removed);
    let (status, out) = rig.exec_active(&rig.go_args(&[]), 1);
    assert_eq!(status, 0, "{out}");
    assert_eq!(rig.fake.count(Call::Terminate), 1);
    assert!(out.contains("Gone"), "{out}");
}
