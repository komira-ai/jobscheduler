//! The server-wide totals: what they count (every pool of the scope, over the day, the
//! rolling 30 days, or since the pool's go), the edges of their windows, and that both the
//! server's and the provider kind's must hold.
//!
//! The pool's one offer costs 60 cents an hour, a cent a minute, and `boot_timeout` is 15
//! minutes, so a launch costs at least 15 cents against a window. Each test drives the pure
//! `step` on a time it names.

mod common;

use common::*;
use js_scaler::{Blocked, Budgets, Go, Phase, Settings, Totals};

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

/// Makes the totals of one window from its figure.
type Make = fn(u64) -> Totals;

fn since_go(total: u64) -> Totals {
    Totals {
        since_go: Some(total),
        ..Totals::default()
    }
}

/// A rig over the pools `names` (each with the kind named beside it), all live with room.
fn pools(names: &[(&str, &str)], budgets: Budgets) -> Rig {
    let first = Settings {
        kind: names[0].1.to_owned(),
        ..settings(names[0].0)
    };
    let mut rig = Rig::new(first);
    for (name, kind) in &names[1..] {
        let each = Settings {
            kind: (*kind).to_owned(),
            ..settings(name)
        };
        rig.input.pools.push(pool(each));
    }
    rig.input.budgets = budgets;
    rig
}

/// The pools the acquires of `actions` are for.
fn launched_in(actions: &[js_scaler::Action]) -> Vec<String> {
    acquires(actions)
        .into_iter()
        .map(|plan| plan.pool)
        .collect()
}

/// Asserts the case the totals exist for: two pools, each far under its own caps, and a
/// server total that the second launch would pass. The first pool launches (15 cents of a
/// total of 30) and the second launches exactly at the edge (30 of 30) but not a cent
/// under it (30 of 29), where it shows `MaxSpendTotal` and the first shows nothing. With no
/// total both launch. It catches a total that counts only the pool being launched into, and
/// one that misses the launch the first pool made in the same step.
#[test]
fn two_pools_each_under_their_own_cap_but_over_the_total_block_the_second() {
    let cases = [
        (None, vec!["a", "b"], None),
        (Some(30), vec!["a", "b"], None),
        (Some(29), vec!["a"], Some(Blocked::MaxSpendTotal)),
    ];
    for (total, expected, blocked_b) in cases {
        let budgets = Budgets {
            server: Totals {
                per_day: total,
                ..Totals::default()
            },
            ..Budgets::default()
        };
        let mut rig = pools(&[("a", ""), ("b", "")], budgets);
        rig.input.waiting = vec![request("r1"), request("r2")];
        let actions = rig.tick(at(0));
        assert_eq!(launched_in(&actions), expected, "total {total:?}");
        assert_eq!(rig.state.blocked.get("b").copied(), blocked_b);
        assert_eq!(rig.state.blocked.get("a"), None);
    }
}

/// Asserts that a total counts the records of other pools, not only those the step stages:
/// a machine of pool "a" cost 40 cents and ended an hour ago, and a launch into pool "b"
/// (15 cents) fits a day total of 55 and not 54. Pool "a" is not among the pools of the
/// input, as a pool taken out of the file is not, and its record still counts. It catches a
/// total that filters the records by the pool being launched into.
#[test]
fn the_total_counts_the_records_of_the_other_pools() {
    for (total, expected) in [(55, 1), (54, 0)] {
        let mut rig = pools(&[("b", "")], server(day(total)));
        let mut spent = record(Phase::Gone, 60, 0);
        spent.pool = "a".to_owned();
        spent.registered = true;
        spent.gone_at = Some(at(40 * MINUTE));
        rig.state.records.instances = vec![spent];
        rig.input.waiting = vec![request("r1")];
        let launches = acquires(&rig.tick(at(HOUR))).len();
        assert_eq!(launches, expected, "total {total}");
    }
}

/// Asserts that a kind's total counts only the pools of the kind, and that both the
/// server's total and the kind's must hold: pools a and b are of kind x, c of kind y. A
/// month total of 29 on x lets a launch (15 of 29) and blocks b (30 of 29), while c, of the
/// other kind, launches; and with the server and the kind each in turn the tight one the
/// second launch into the kind is blocked, and with both at 30 it is not.
#[test]
fn a_kinds_total_counts_its_own_pools_and_both_totals_must_hold() {
    let kinds = Budgets {
        kinds: [("x".to_owned(), month(29))].into(),
        ..Budgets::default()
    };
    let mut rig = pools(&[("a", "x"), ("b", "x"), ("c", "y")], kinds);
    rig.input.waiting = vec![request("r1"), request("r2")];
    let actions = rig.tick(at(0));
    assert_eq!(launched_in(&actions), ["a", "c"]);
    assert_eq!(rig.state.blocked["b"], Blocked::MaxSpendTotal);

    let cases = [
        (Some(100), Some(29), 1),
        (Some(29), Some(100), 1),
        (Some(30), Some(30), 2),
        (None, None, 2),
    ];
    for (server, kind, expected) in cases {
        let budgets = Budgets {
            server: Totals {
                per_month: server,
                ..Totals::default()
            },
            kinds: [(
                "x".to_owned(),
                Totals {
                    per_month: kind,
                    ..Totals::default()
                },
            )]
            .into(),
        };
        let mut rig = pools(&[("a", "x"), ("b", "x")], budgets);
        rig.input.waiting = vec![request("r1"), request("r2")];
        let launches = acquires(&rig.tick(at(0))).len();
        assert_eq!(launches, expected, "server {server:?} kind {kind:?}");
    }
}

/// A rig of the pool "gpu" holding one machine that was billed 60 cents (launched at the
/// start, gone an hour later, registered so its least bill is a minute), with the pool's go
/// at `go`, one request waiting and `budgets`. A machine too small to cover the request is
/// not needed: a gone one covers nothing.
fn spent_an_hour(budgets: Budgets, go: u64, settled: u64) -> Rig {
    let mut rig = Rig::new(settings("gpu"));
    let mut done = record(Phase::Gone, 60, 0);
    done.registered = true;
    done.gone_at = Some(at(HOUR));
    rig.state.records.instances = vec![done];
    rig.state.records.go.insert(
        "gpu".to_owned(),
        Go {
            at: at(go),
            settled,
        },
    );
    rig.input.budgets = budgets;
    rig.input.waiting = vec![request("r1")];
    rig
}

fn server(totals: Totals) -> Budgets {
    Budgets {
        server: totals,
        ..Budgets::default()
    }
}

/// Asserts the edge of each window, to the millisecond, for a machine that cost 60 cents
/// between the start and one hour in, against a launch of 15 cents. The day window holds it
/// until 24 hours after it ended; the month window until 30 days after; the budget since the
/// go for ever. At each instant the matching total of 75 admits the launch and 74 does not,
/// and a window that has let the machine go admits it at a total of 15. It catches a window
/// of the wrong length, an off-by-one at the edge, and a total read from the wrong window.
#[test]
fn each_window_holds_a_machine_until_its_billed_time_has_left_it() {
    let hour = HOUR;
    let cases: [(&str, u64, Make, u64); 7] = [
        // The machine is inside every window an hour after it ended.
        ("day, inside", 2 * hour, day, 75),
        ("month, inside", 2 * hour, month, 75),
        ("since go, long after", 90 * DAY, since_go, 75),
        // 24 hours after it ended, the day has let go of it (1 ms short, it has not).
        ("day, edge", DAY + hour, day, 15),
        ("day, just short", DAY + hour - 1, day, 16),
        // The month window.
        ("month, edge", 30 * DAY + hour, month, 15),
        ("month, just short", 30 * DAY + hour - 1, month, 16),
    ];
    for (label, now, make, held) in cases {
        for (total, expected) in [(held, 1), (held - 1, 0)] {
            let mut rig = spent_an_hour(server(make(total)), 0, 0);
            let launches = acquires(&rig.tick(at(now))).len();
            assert_eq!(launches, expected, "{label}, total {total}");
        }
    }
}

/// Asserts the budget since the go: it counts only the part of a machine's billed time after
/// the go (a go half an hour into its life leaves 30 cents of 60) and adds what was settled
/// from forgotten machines. Totals of 45, 175 and 145 admit a
/// launch and one cent less does not. It catches a budget counted from the machine's launch
/// and not the go, and one that drops what was settled.
#[test]
fn the_budget_since_the_go_counts_from_the_go_and_adds_what_was_settled() {
    let cases = [
        // (go, settled, total that admits)
        (30 * MINUTE, 0, 45),
        (0, 100, 175),
        (30 * MINUTE, 100, 145),
    ];
    for (go, settled, held) in cases {
        for (total, expected) in [(held, 1), (held - 1, 0)] {
            let mut rig = spent_an_hour(server(since_go(total)), go, settled);
            let launches = acquires(&rig.tick(at(2 * HOUR))).len();
            assert_eq!(
                launches, expected,
                "go {go} settled {settled} total {total}"
            );
        }
    }
}

/// Asserts that a machine forgotten when its billed time leaves the month is not lost to
/// the budget since the go: 31 days after the machine (60 cents) ended, the step drops its
/// record, the go holds 60 as settled and the step writes it; then totals of 74 and 75 admit
/// a launch of 15 cents at 60 and 75, and the record is not in the month window (a month
/// total of 15 admits it). It catches a record dropped with its cost, which would give the
/// budget back to a pool that spent it.
#[test]
fn a_forgotten_machine_is_settled_into_the_go() {
    let mut rig = spent_an_hour(Budgets::default(), 0, 0);
    rig.input.waiting = Vec::new();
    let actions = rig.tick(at(31 * DAY));
    assert_eq!(kinds(&actions), ["Save"]);
    assert!(rig.state.records.instances.is_empty());
    assert_eq!(
        rig.state.records.go["gpu"],
        Go {
            at: at(0),
            settled: 60
        }
    );

    rig.input.waiting = vec![request("r1")];
    for (total, expected) in [(74, 0), (75, 1)] {
        rig.input.budgets = Budgets {
            server: Totals {
                since_go: Some(total),
                per_month: Some(15),
                ..Totals::default()
            },
            ..Budgets::default()
        };
        let launches = acquires(&rig.tick(at(31 * DAY + MINUTE))).len();
        assert_eq!(launches, expected, "total {total}");
        if expected == 0 {
            assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpendTotal);
        }
    }
}

/// Asserts that a machine still inside the month is kept in the records for as long as its
/// billed time is, not forgotten at a day as before: 29 days after it ended it is still
/// there; at 30 days (the instant its billed time leaves the window) it is gone. It catches
/// the day retention that would let a month total forget a day-old machine.
#[test]
fn records_are_kept_for_the_month_window() {
    let mut rig = spent_an_hour(Budgets::default(), 0, 0);
    rig.input.waiting = Vec::new();
    rig.tick(at(29 * DAY));
    assert_eq!(rig.state.records.instances.len(), 1);
    rig.tick(at(30 * DAY + HOUR - 1));
    assert_eq!(rig.state.records.instances.len(), 1);
    rig.tick(at(30 * DAY + HOUR));
    assert!(rig.state.records.instances.is_empty());
}

/// Asserts that a forgotten machine of a pool that has no go (here one the input does not
/// have) settles nothing into anyone's go: the record goes, and the live pool's go is as it
/// was, with no entry made for the other pool. It catches a settle that creates a go, which
/// would give a pool that is not live a budget.
#[test]
fn a_forgotten_machine_of_a_pool_without_a_go_settles_nothing() {
    let mut rig = spent_an_hour(Budgets::default(), 0, 0);
    rig.state.records.instances[0].pool = "elsewhere".to_owned();
    rig.input.waiting = Vec::new();
    rig.tick(at(31 * DAY));
    assert!(rig.state.records.instances.is_empty());
    assert_eq!(rig.state.records.go.len(), 1);
    assert_eq!(
        rig.state.records.go["gpu"],
        Go {
            at: at(0),
            settled: 0
        }
    );
}
