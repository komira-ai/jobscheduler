//! The totals where they are easiest to get wrong: the first launches of a go with
//! `max_step` above one, the kind of a pool that has left the file, the registry's
//! `Launchable`, an adopted orphan, and every kind of total driven through real launches
//! across restarts. The pool's one offer costs 60 cents an hour, so a launch costs at least
//! 15 cents against a window.

mod common;

use common::*;
use js_elastic::{Held, HostRef, HostStatus};
use js_scaler::{Blocked, Budgets, Phase, Records, Settings, State, Totals, launchable};

fn server(totals: Totals) -> Budgets {
    Budgets {
        server: totals,
        ..Budgets::default()
    }
}

fn kind(name: &str, totals: Totals) -> Budgets {
    Budgets {
        kinds: [(name.to_owned(), totals)].into(),
        ..Budgets::default()
    }
}

fn day(total: u64) -> Totals {
    Totals {
        per_day: Some(total),
        ..Totals::default()
    }
}

fn month(total: u64) -> Totals {
    Totals {
        per_month: Some(total),
        ..Totals::default()
    }
}

/// Makes the budgets of one scope from its day total.
type Make = fn(u64) -> Budgets;

fn since_go(total: u64) -> Totals {
    Totals {
        since_go: Some(total),
        ..Totals::default()
    }
}

/// A scaler started from the records file that `rig` has written, given the input `rig` was
/// last given and continuing its random draws.
fn restart(rig: &Rig, settings: Settings) -> Rig {
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings);
    fresh.input = rig.input.clone();
    fresh.drawn = rig.drawn;
    fresh.state = State::new(Records::decode(&text).unwrap());
    fresh
}

/// Asserts that, in the step that begins a go, each launch is checked against the ones staged
/// before it in the same step: with `max_step` of 3 and a budget since the go of 30 cents,
/// three requests get two launches (15 and 15) and the third is blocked, and the go begins at
/// the step. A budget of 14 admits none and leaves no go behind. It catches a go that begins
/// after the staging, which reads zero spent for every launch of the first step.
#[test]
fn the_first_step_of_a_go_checks_each_launch_against_those_staged_before_it() {
    for (total, launches, go) in [(30, 2, true), (29, 1, true), (14, 0, false)] {
        let mut limits = settings("gpu");
        limits.max_step = 3;
        let mut rig = Rig::new(limits);
        rig.input.budgets = server(since_go(total));
        rig.input.waiting = vec![request("r1"), request("r2"), request("r3")];
        assert_eq!(
            acquires(&rig.tick(at(MINUTE))).len(),
            launches,
            "total {total}"
        );
        assert_eq!(
            rig.state.records.go.contains_key("gpu"),
            go,
            "total {total}"
        );
        if go {
            assert_eq!(rig.state.records.go["gpu"].at, at(MINUTE));
        }
        if launches < 3 {
            assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpendTotal);
        }
    }
}

/// A gone machine of the pool "a", billed 40 cents (launched at the start, gone 40 minutes
/// later), in the records of a scaler that remembers the kinds `kinds` for its pools.
fn left_behind(kinds: &[(&str, &str)]) -> Records {
    let mut spent = record(Phase::Gone, 60, 0);
    spent.pool = "a".to_owned();
    spent.registered = true;
    spent.gone_at = Some(at(40 * MINUTE));
    Records {
        instances: vec![spent],
        pool_kinds: kinds
            .iter()
            .map(|(pool, each)| ((*pool).to_owned(), (*each).to_owned()))
            .collect(),
        ..Records::default()
    }
}

/// Asserts that the spend of a pool that is no longer in the file counts in its own kind's
/// total, and not another's: the records of pool "a" (40 cents) say it was kind x, and a
/// launch of 15 cents into pool "b" of kind x fits a month total of 55 and not 54; if they
/// say kind y, it fits 15; if they say nothing the spend counts in every kind, the cautious
/// side. It catches a kind scope that forgets the pools the input no longer has.
#[test]
fn a_removed_pools_spend_still_counts_in_its_kinds_total() {
    let cases = [
        (&[("a", "x")][..], 54, 0),
        (&[("a", "x")][..], 55, 1),
        (&[("a", "y")][..], 15, 1),
        (&[][..], 54, 0),
    ];
    for (kinds, total, expected) in cases {
        let b = Settings {
            kind: "x".to_owned(),
            ..settings("b")
        };
        let mut rig = Rig::new(b);
        rig.state.records = left_behind(kinds);
        rig.input.budgets = kind("x", month(total));
        rig.input.waiting = vec![request("r1")];
        let launches = acquires(&rig.tick(at(HOUR))).len();
        assert_eq!(launches, expected, "kinds {kinds:?} total {total}");
    }
}

/// Asserts that a launch notes the kind of its pool in the records, once and unchanged by a
/// second launch, and that a pool with no kind notes none.
#[test]
fn a_launch_notes_the_kind_of_its_pool() {
    let mut rig = Rig::new(Settings {
        kind: "x".to_owned(),
        ..settings("gpu")
    });
    rig.input.waiting = vec![request("r1")];
    rig.tick(at(0));
    assert_eq!(rig.state.records.pool_kinds["gpu"], "x");
    rig.input.waiting = vec![request("r1"), request("r2")];
    rig.tick(at(MINUTE));
    assert_eq!(rig.state.records.pool_kinds.len(), 1);

    let mut bare = Rig::new(settings("gpu"));
    bare.input.waiting = vec![request("r1")];
    bare.tick(at(0));
    assert!(bare.state.records.pool_kinds.is_empty());
    assert!(!bare.state.records.encode().unwrap().contains("pool_kinds"));
}

/// Asserts the registry's `Launchable` under a total: a gone machine of 60 cents and a launch
/// of 15 cents are answered yes at a day total of 75 and no at 74, for the server's total and
/// for the kind's, and yes with none. It catches a total the registry does not ask, which
/// would accept a request the step then refuses for ever.
#[test]
fn launchable_answers_for_the_totals() {
    let budgets: [(&str, Make); 2] = [
        ("server", |total| server(day(total))),
        ("kind", |total| kind("x", day(total))),
    ];
    for (label, make) in budgets {
        for (total, expected) in [(75, true), (74, false)] {
            let mut rig = Rig::new(Settings {
                kind: "x".to_owned(),
                ..settings("gpu")
            });
            let mut spent = record(Phase::Gone, 60, 0);
            spent.registered = true;
            spent.gone_at = Some(at(HOUR));
            rig.state.records.instances = vec![spent];
            rig.input.budgets = make(total);
            let allowed = launchable(&rig.state, &rig.input, at(2 * HOUR), &demand());
            assert_eq!(allowed, expected, "{label} {total}");
        }
    }
    let mut rig = Rig::new(settings("gpu"));
    rig.state.records.instances = vec![record(Phase::Gone, 60, 0)];
    assert!(launchable(&rig.state, &rig.input, at(2 * HOUR), &demand()));
}

fn stray(id: &str) -> Held {
    Held {
        id: HostRef(id.to_owned()),
        acquire: "stray-1".to_owned(),
        name: None,
        offer: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        status: HostStatus::Active,
    }
}

/// Asserts that an orphan the scaler adopts is counted by the totals as it accrues: found at
/// the start and listed an hour later, it has cost 60 cents, and a launch of 15 fits a day
/// total of 75 and not 74. It catches an adopted machine that the totals do not see.
#[test]
fn an_adopted_orphan_counts_toward_the_totals() {
    for (total, expected) in [(75, 1), (74, 0)] {
        let mut rig = Rig::new(settings("gpu"));
        rig.input.budgets = server(day(total));
        rig.pool().held = Some(vec![stray("i-stray")]);
        rig.tick(at(0));
        assert_eq!(rig.phase(0), Phase::Orphan);
        rig.input.waiting = vec![request("r1")];
        let launches = acquires(&rig.tick(at(HOUR))).len();
        assert_eq!(launches, expected, "total {total}");
    }
}

/// Asserts that each kind of total keeps holding across restarts of the scaler, driven by
/// real launches and not injected records: totals of 30 cents (two launches of 15) in the
/// server's day, the server's budget since the go, and a kind's month. A launch is made,
/// the scaler is restarted from its file, another is made, it is restarted again, and the
/// third is blocked by the total. It catches a total, or the go it counts from, that is lost
/// or begun again by a restart.
#[test]
fn every_total_holds_across_restarts() {
    let cases: [(&str, Budgets); 3] = [
        ("day", server(day(30))),
        ("since go", server(since_go(30))),
        ("kind month", kind("x", month(30))),
    ];
    for (label, budgets) in cases {
        let limits = Settings {
            kind: "x".to_owned(),
            ..settings("gpu")
        };
        let mut rig = Rig::new(limits.clone());
        rig.input.budgets = budgets;
        rig.input.waiting = vec![request("r1")];
        assert_eq!(acquires(&rig.tick(at(MINUTE))).len(), 1, "{label}");

        let mut rig = restart(&rig, limits.clone());
        rig.input.waiting = vec![request("r1"), request("r2")];
        assert_eq!(acquires(&rig.tick(at(2 * MINUTE))).len(), 1, "{label}");

        let mut rig = restart(&rig, limits);
        rig.input.waiting = vec![request("r1"), request("r2"), request("r3")];
        assert_eq!(acquires(&rig.tick(at(3 * MINUTE))).len(), 0, "{label}");
        assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpendTotal, "{label}");
    }
}
