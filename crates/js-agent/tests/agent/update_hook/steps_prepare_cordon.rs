//! `prepare`'s cordon (host-updates design, 3.6 and 4.2): whose it is, how it is renewed, what a
//! rerun finds, and what a failing lease server does to the step. A cordon the operator placed
//! is waited on and never renewed or lifted. A `prepare` killed at any kill point and run again
//! ends where an uninterrupted one does.

use std::fs;

use js_agent::update_hook::{
    Class, Cordon, DirStat, Done, Held, Lease, Outcome, Reason, StateError, step, validate,
};
use serde_json::{Value, json};

use super::step_rig::{LEASE_SECS, Rig, START, Spy, as_root, killable, line};

fn cordon_calls(spy: &Spy<'_>) -> Vec<String> {
    spy.log()
        .into_iter()
        .filter(|call| call.starts_with("cordon("))
        .collect()
}

fn lease_unreachable(done: &Done) {
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert!(done.report.reversible);
}

#[test]
fn an_operators_cordon_is_waited_on_and_neither_renewed_nor_recorded_as_ours() {
    let rig = Rig::new("hook-prepare-operator");
    let operator_expiry = rig.operator_cordons();
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 2 {
            spy.world.alter_host(|host| host.leases = 0);
        }
    });
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    let record = rig.record().unwrap();
    assert_eq!(record.cordon, Cordon::default());
    assert_eq!(
        cordon_calls(&spy),
        [format!("cordon({LEASE_SECS})")],
        "asked once, never renewed"
    );
    assert_eq!(rig.cordon_expiry(), Some(operator_expiry), "not shortened");
}

#[test]
fn an_operators_cordon_is_not_lifted_when_the_bound_passes() {
    let rig = Rig::new("hook-prepare-operator-deferred");
    rig.operator_cordons();
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert!(!spy.log().contains(&"uncordon".to_owned()));
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
}

#[test]
fn a_cordon_the_operator_places_while_we_do_is_not_ours() {
    let rig = Rig::new("hook-prepare-operator-race");
    let spy = Spy::new(&rig);
    spy.operator_races.set(true);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    // The reply said the host was cordoned already: the record that was written before the
    // call, and said ours, is corrected.
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
    assert!(rig.cordon_expiry().is_some());
}

#[test]
fn a_renewal_never_runs_past_the_cap_and_stops_when_the_cap_is_reached() {
    let mut rig = Rig::with_cordon("hook-prepare-cap", 300, 25);
    rig.params.drain_wait_secs = 3600;
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 4 {
            spy.world.alter_host(|host| host.leases = 0);
        }
    });
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    // 25 s left at the start, 15 after the first pause, 5 after the second, none after.
    assert_eq!(
        cordon_calls(&spy),
        ["cordon(25)", "cordon(15)", "cordon(5)"]
    );
    let record = rig.record().unwrap();
    assert_eq!(
        record.cordon,
        Cordon {
            placed: true,
            provisional: false,
            first_ms: START,
            expires_ms: START + 25_000
        }
    );
    assert_eq!(validate(&record, &rig.declaration, &rig.params), Ok(()));
}

#[test]
fn a_rerun_places_a_cordon_that_lapsed_again_and_keeps_the_first_placement() {
    let rig = Rig::new("hook-prepare-rerun-lapsed");
    let mut record = rig.fresh_record();
    record.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: START - 60_000,
        expires_ms: START - 1_000,
    };
    rig.put_record(&record);
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    let end = rig.record().unwrap();
    assert_eq!(
        end.cordon,
        Cordon {
            placed: true,
            provisional: false,
            first_ms: START - 60_000,
            expires_ms: START + LEASE_SECS * 1000
        }
    );
    assert_eq!(cordon_calls(&spy).len(), 1);
}

#[test]
fn a_rerun_that_finds_its_own_cordon_still_in_force_keeps_it_ours() {
    let rig = Rig::new("hook-prepare-rerun-live");
    rig.world.cordon(LEASE_SECS).unwrap();
    let mut record = rig.fresh_record();
    record.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: START,
        expires_ms: START + LEASE_SECS * 1000,
    };
    rig.put_record(&record);
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert!(rig.record().unwrap().cordon.placed);
}

#[test]
fn a_recorded_expiry_never_goes_down() {
    let rig = Rig::new("hook-prepare-expiry-up");
    let mut record = rig.fresh_record();
    record.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: START,
        expires_ms: START + 500_000,
    };
    rig.put_record(&record);
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record().unwrap().cordon.expires_ms, START + 500_000);
}

#[test]
fn a_rerun_after_a_deferral_places_the_cordon_afresh() {
    let rig = Rig::new("hook-prepare-after-deferral");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Deferred);
    rig.set_leases(0);
    let again = Spy::new(&rig);
    assert_eq!(again.prepare(&rig).outcome, Outcome::Success);
    assert!(rig.record().unwrap().cordon.placed);
    assert!(rig.cordon_expiry().is_some());
}

#[test]
fn prepare_works_while_the_lease_server_awaits_the_agents_hello() {
    let rig = Rig::new("hook-prepare-awaiting");
    rig.world.forget_host();
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    let host = rig.host();
    assert_eq!((host.state.as_str(), host.epoch), ("awaiting", 0));
    assert!(host.cordon.is_some());
    assert!(rig.record().unwrap().cordon.placed);
}

#[test]
fn a_first_install_has_no_agent_to_drain_and_no_host_to_cordon() {
    let rig = Rig::first_install("hook-prepare-first");
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    let record = rig.record().unwrap();
    assert_eq!(record.update.previous, None);
    assert_eq!(record.update.previous_target, None);
    assert_eq!(record.cordon, Cordon::default());
    assert_eq!(spy.log().len(), 1, "the version check only");
    assert_eq!(rig.world.ticks(), 0);
    // A rerun finds the record and leaves it as it is.
    let before = rig.record_bytes();
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record_bytes(), before);
}

#[test]
fn a_lease_server_that_fails_is_an_environment_failure_and_a_server_that_forgot_the_host_too() {
    let rig = Rig::new("hook-prepare-down-first");
    let spy = Spy::new(&rig);
    spy.fail("host");
    lease_unreachable(&spy.prepare(&rig));
    assert_eq!(rig.record_bytes(), None, "nothing was recorded");

    let rig = Rig::new("hook-prepare-lost-state");
    rig.world.lose_state_file();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    lease_unreachable(&done);
    assert!(done.report.detail.unwrap().contains("unknown host"));
    assert_eq!(rig.record_bytes(), None);

    let rig = Rig::new("hook-prepare-cordon-down");
    let spy = Spy::new(&rig);
    spy.fail("cordon");
    lease_unreachable(&spy.prepare(&rig));
    assert_eq!(rig.cordon_expiry(), None);
}

#[test]
fn a_cap_with_nothing_left_to_ask_for_is_its_own_failure_and_no_lease_call() {
    // Nothing to ask for at the start.
    let mut rig = Rig::with_cordon("hook-prepare-cap-none", 300, 100);
    rig.params.cordon_lease_secs = 0;
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::CordonCapReached));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(cordon_calls(&spy), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), None);
    assert_eq!(rig.cordon_expiry(), None);

    // A rerun after the cap was used up.
    let mut rig = Rig::with_cordon("hook-prepare-cap-used", 300, 100);
    rig.params.cordon_max_secs = 20;
    let mut record = rig.fresh_record();
    record.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: START - 20_000,
        expires_ms: START - 1,
    };
    rig.put_record(&record);
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.report.reason, Some(Reason::CordonCapReached));
    assert_eq!(cordon_calls(&spy), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), before);
}

#[test]
fn a_lease_server_that_fails_during_the_wait_ends_the_step_there() {
    let rig = Rig::new("hook-prepare-down-reading");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 2 {
            spy.fail("host");
        }
    });
    lease_unreachable(&spy.prepare(&rig));
    assert_eq!(rig.world.ticks(), 2);
    assert!(
        rig.record().unwrap().cordon.placed,
        "left for rollback to lift"
    );

    let rig = Rig::new("hook-prepare-down-renewal");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 1 {
            spy.fail("cordon");
        }
    });
    lease_unreachable(&spy.prepare(&rig));
    // The first renewal is due when less than half of the 300 s lease is left: at 160 s.
    assert_eq!(rig.world.ticks(), 16);
}

#[test]
fn a_cordon_that_cannot_be_lifted_after_the_bound_is_a_failure_that_keeps_the_record() {
    let rig = Rig::new("hook-prepare-down-uncordon");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.fail("uncordon");
    lease_unreachable(&spy.prepare(&rig));
    assert!(
        rig.record().unwrap().cordon.placed,
        "a retry or a rollback lifts it"
    );
    assert!(rig.cordon_expiry().is_some());
}

/// A record that breaks a rule is refused with nothing changed.
fn refused(rig: &Rig, done: &Done, before: Option<Vec<u8>>, spy: &Spy<'_>) {
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(spy.log(), Vec::<String>::new(), "no port was touched");
    assert_eq!(rig.world.ticks(), 0);
    assert_eq!(rig.cordon_expiry(), None);
}

#[test]
fn a_record_that_is_not_this_updates_or_breaks_a_rule_is_refused_as_state_invalid() {
    type Plant = fn(&Rig);
    let cases: [(&str, Plant); 6] = [
        ("another-target", |rig| {
            let mut record = rig.fresh_record();
            record.update.target = "1.5.0".to_owned();
            rig.put_record(&record);
        }),
        ("undeclared-unit", |rig| {
            let mut record = rig.fresh_record();
            record.update.unit = "other.service".to_owned();
            rig.put_record(&record);
        }),
        ("cordon-past-the-cap", |rig| {
            let mut record = rig.fresh_record();
            record.cordon = Cordon {
                placed: true,
                provisional: false,
                first_ms: START,
                expires_ms: START + 20_000_000,
            };
            rig.put_record(&record);
        }),
        ("mark-without-its-cause", |rig| {
            let mut record = rig.fresh_record();
            record.link_moved = true;
            rig.put_record(&record);
        }),
        ("newer-schema", |rig| {
            fs::write(rig.record_path(), br#"{"schema":2}"#).unwrap();
        }),
        ("garbage", |rig| {
            fs::write(rig.record_path(), b"not json").unwrap();
        }),
    ];
    for (name, plant) in cases {
        let rig = Rig::new(&format!("hook-prepare-invalid-{name}"));
        plant(&rig);
        let before = rig.record_bytes();
        let spy = Spy::new(&rig);
        let done = spy.prepare(&rig);
        refused(&rig, &done, before, &spy);
    }
}

#[test]
fn an_order_the_record_rules_cannot_hold_is_refused_before_anything_is_written() {
    let mut cases = Vec::new();
    let mut same_release = Rig::new("hook-prepare-same-release");
    same_release.params.previous_release = Some(same_release.params.staged_release.clone());
    cases.push(same_release);
    let mut same_version = Rig::new("hook-prepare-same-version");
    same_version.params.previous_version = Some("1.4.0".to_owned());
    cases.push(same_version);
    for rig in cases {
        let spy = Spy::new(&rig);
        let done = spy.prepare(&rig);
        refused(&rig, &done, None, &spy);
    }
}

#[test]
fn a_state_directory_that_is_not_root_0700_is_refused_and_so_is_a_second_holder() {
    let rig = Rig::new("hook-prepare-bad-dir");
    let loose = |_: &std::path::Path, _: std::os::fd::BorrowedFd<'_>| {
        Ok(DirStat {
            is_symlink: false,
            is_dir: true,
            uid: 0,
            gid: 0,
            mode: 0o755,
        })
    };
    let spy = Spy::new(&rig);
    let done = step::prepare(&spy.env(&rig, &loose), &rig.params);
    refused(&rig, &done, None, &spy);
    assert!(done.report.detail.unwrap().contains("0755"));

    let rig = Rig::new("hook-prepare-locked");
    let _other = Held::open(&rig.state_dir(), &as_root).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::Locked));
    assert_eq!(rig.record_bytes(), None);
    assert_eq!(spy.log(), Vec::<String>::new());
    assert!(matches!(
        Held::open(&rig.state_dir(), &as_root),
        Err(StateError::Lock { .. })
    ));
}

/// Where a run ended, without the clock's stamps.
fn end_state(rig: &Rig, done: &Done) -> Value {
    let mut record: Value = serde_json::to_value(rig.record().unwrap()).unwrap();
    record["cordon"]["first_ms"] = json!(0);
    record["cordon"]["expires_ms"] = json!(0);
    // The directories differ from one rig to the next.
    record["update"] = json!(null);
    json!({
        "exit": done.outcome.code(),
        "report": line(done),
        "record": record,
        "cordoned": rig.cordon_expiry().is_some(),
        "leases": rig.host().leases,
    })
}

/// One run of `prepare` on a host whose lease ends at the `ends_at`th pause (never, if none).
fn attempt(rig: &Rig, ends_at: Option<usize>) -> Done {
    let spy = Spy::new(rig);
    spy.on_pause(move |spy, ticks| {
        if ends_at.is_some_and(|n| ticks >= n) {
            spy.world.alter_host(|host| host.leases = 0);
        }
    });
    spy.prepare(rig)
}

/// The host before the run: one lease, and the operator's cordon if `operator`.
fn setup(name: &str, operator: bool) -> Rig {
    let rig = Rig::new(name);
    rig.set_leases(1);
    if operator {
        rig.operator_cordons();
    }
    // The operator's cordon passed a kill point; the count is of the step's.
    rig.kill.disarm();
    rig
}

/// Kills a `prepare` at each of `kills` (all points when none) and runs it again: it ends as
/// the uninterrupted run does.
fn resumed_ends_as_uninterrupted(
    name: &str,
    ends_at: Option<usize>,
    operator: bool,
    leaks_at: Option<u32>,
    kills: impl FnOnce(u32) -> Vec<u32>,
) {
    let baseline = setup(&format!("{name}-base"), operator);
    let done = attempt(&baseline, ends_at);
    let points = baseline.kill.points();
    let expected = end_state(&baseline, &done);
    assert!(points >= 3, "a step that changes things passes kill points");
    for k in kills(points) {
        let rig = setup(&format!("{name}-{k}"), operator);
        rig.kill.arm(k);
        assert!(
            killable(|| attempt(&rig, ends_at)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let done = attempt(&rig, ends_at);
        // A kill after the server has our cordon and before the record confirms it leaves a
        // cordon that is not known to be ours: the rerun does not claim it, and it lapses.
        let mut expected = expected.clone();
        if leaks_at == Some(k) {
            expected["record"]["cordon"]["placed"] = json!(false);
        }
        assert_eq!(end_state(&rig, &done), expected, "killed at point {k}");
    }
}

#[test]
fn a_prepare_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    resumed_ends_as_uninterrupted("hook-prepare-kill-clean", Some(3), false, Some(4), |n| {
        (1..=n).collect()
    });
}

#[test]
fn a_prepare_killed_at_any_point_leaves_an_operators_cordon_alone() {
    resumed_ends_as_uninterrupted("hook-prepare-kill-operator", Some(2), true, None, |n| {
        (1..=n).collect()
    });
}

#[test]
fn a_deferred_prepare_killed_near_either_end_and_run_again_ends_deferred_with_no_cordon() {
    resumed_ends_as_uninterrupted("hook-prepare-kill-deferred", None, false, None, |n| {
        (1..=6).chain(n - 4..=n).collect()
    });
}

/// Marks the record as one `verify` passed, as `cleanup` finds it.
fn verified(rig: &Rig) {
    let mut record = rig.record().unwrap();
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    record.verify_ok = true;
    rig.put_record(&record);
}

#[test]
fn a_prepare_killed_before_its_cordon_reached_the_server_never_adopts_the_operators() {
    // Points: 1 the first record, 2 before the call, 3 before the server writes.
    for k in 1..=3 {
        for lease_runs in [false, true] {
            let rig = Rig::new(&format!("hook-prepare-adopt-{k}-{lease_runs}"));
            rig.kill.arm(k);
            assert!(killable(|| Spy::new(&rig).prepare(&rig)).is_none());
            rig.kill.disarm();
            if k >= 2 {
                let marked = rig.record().unwrap().cordon;
                assert!(marked.placed && marked.provisional, "{marked:?}");
            }
            // Someone else cordons the host, then the step is run again.
            let operator = rig.operator_cordons();
            rig.kill.disarm();
            rig.set_leases(u64::from(lease_runs));
            let spy = Spy::new(&rig);
            let done = spy.prepare(&rig);
            assert_eq!(
                done.outcome,
                if lease_runs {
                    Outcome::Deferred
                } else {
                    Outcome::Success
                },
                "{k}"
            );
            assert_eq!(rig.record().unwrap().cordon, Cordon::default(), "{k}");
            assert!(!spy.log().contains(&"uncordon".to_owned()), "{k}");
            if !lease_runs {
                assert_eq!(rig.cordon_expiry(), Some(operator), "not shortened, {k}");
                // And what `verify` passes, `cleanup` leaves alone.
                verified(&rig);
                let cleanup = Spy::new(&rig);
                assert_eq!(cleanup.cleanup(&rig).outcome, Outcome::Success);
                assert!(!cleanup.log().contains(&"uncordon".to_owned()), "{k}");
                assert_eq!(rig.cordon_expiry(), Some(operator), "{k}");
                assert_eq!(rig.record_bytes(), None);
            }
        }
    }
}

#[test]
fn a_cordon_that_lapsed_and_was_replaced_by_an_operators_is_not_adopted_on_a_rerun() {
    let rig = Rig::new("hook-prepare-adopt-lapsed");
    let mut record = rig.fresh_record();
    record.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: START - 60_000,
        expires_ms: START - 1_000,
    };
    rig.put_record(&record);
    let operator = rig.operator_cordons();
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
    assert_eq!(rig.cordon_expiry(), Some(operator));
    verified(&rig);
    let cleanup = Spy::new(&rig);
    assert_eq!(cleanup.cleanup(&rig).outcome, Outcome::Success);
    assert!(!cleanup.log().contains(&"uncordon".to_owned()));
    assert_eq!(rig.cordon_expiry(), Some(operator));
}

#[test]
fn a_cordon_a_killed_run_placed_and_never_confirmed_is_left_to_lapse_not_lifted() {
    let rig = Rig::new("hook-prepare-confirmed");
    rig.kill.arm(4);
    // Points 1-3 as above; 4 is the record that confirms the cordon.
    assert!(killable(|| Spy::new(&rig).prepare(&rig)).is_none());
    rig.kill.disarm();
    assert!(rig.cordon_expiry().is_some(), "the server has our cordon");
    assert!(
        rig.record().unwrap().cordon.provisional,
        "the record does not know yet"
    );
    // The rerun is told the host is cordoned and cannot tell whose it is: it is not lifted by
    // this update, and it lapses within one lease.
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    let cordon = rig.record().unwrap().cordon;
    assert!(!cordon.placed && !cordon.provisional, "{cordon:?}");
}
