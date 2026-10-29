//! Which waiting requests get a machine: what covers a request, what does not, the choice
//! among offers, and the refusal of an offer of another platform.

mod common;

use std::collections::BTreeSet;

use common::*;
use js_elastic::Offer;
use js_scaler::{
    Action, Blocked, HostFacts, HostState, LaunchMode, Mark, Phase, Settings, State, StoredOffer,
    step,
};
use js_types::{ExecMode, RequestId};

fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

/// Asserts the first launch writes the records, then mints, then acquires; that the record
/// holds the stored price, the time of launch and a token that lives `boot_timeout` plus
/// its margin; and that the same step on the same state and input gives the same answer.
/// It catches an acquire that is not preceded by its records, and a step that reads
/// anything but its arguments.
#[test]
fn one_waiting_request_gets_one_launch_in_the_order_record_mint_acquire() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    rig.input.entropy = vec![draw(1)];
    let (again_state, again_actions) = step(State::default(), &rig.input, at(0));
    let actions = rig.tick(at(0));
    assert_eq!(actions, again_actions);
    assert_eq!(rig.state, again_state);
    assert_eq!(kinds(&actions), ["Save", "Mint", "Acquire"]);
    let plan = acquires(&actions).remove(0);
    assert_eq!(plan.pool, "gpu");
    assert_eq!(plan.offer.id, "a10");
    assert_eq!(plan.offer.price_cents_per_hour, Some(60));
    assert_eq!(
        actions[0],
        Action::Save(Box::new(rig.state.records.clone()))
    );
    assert_eq!(plan.acquire.as_str(), format!("gpu-{}", draw_text(1)));
    assert_eq!(
        actions[1],
        Action::Mint {
            host: plan.name.clone(),
            valid_for: minutes(20)
        }
    );
    assert_eq!(rig.phase(0), Phase::Launching);
    assert_eq!(rig.record(0).price_cents_per_hour, 60);
    assert_eq!(rig.record(0).launched_at, at(0));
    assert_eq!(rig.record(0).token_expires, Some(at(20 * MINUTE)));
}

/// Asserts that the same request presented again, with the record of its launch in the
/// state and no answer yet, launches nothing and writes nothing, however many steps pass.
/// It catches a second machine for one request.
#[test]
fn a_repeated_step_does_not_launch_again() {
    let mut rig = Rig::new(settings("gpu"));
    rig.input.waiting = vec![request("r1")];
    rig.tick(at(0));
    for step_at in [5, 10, 15] {
        assert!(rig.tick(at(step_at * SECOND)).is_empty());
    }
    assert_eq!(rig.state.records.instances.len(), 1);
    assert_eq!(rig.state.refused, 0);
}

/// Asserts that a hundred waiting requests with `max_instances` of 3 never make a fourth
/// launch: with a step that may launch many, three in the first step and none after; with
/// the default step of one, one a step until the cap shows as the blocked reason. It
/// catches a cap checked against the records of earlier steps only.
#[test]
fn a_hundred_requests_never_exceed_the_instance_cap() {
    for max_step in [100, 1] {
        let mut limits = settings("gpu");
        limits.max_instances = 3;
        limits.max_step = max_step;
        let mut rig = Rig::new(limits);
        rig.input.waiting = (0..100).map(|n| request(&format!("r{n}"))).collect();
        let mut launched = 0;
        for step_at in 0..8 {
            launched += acquires(&rig.tick(at(step_at * 5 * SECOND))).len();
        }
        assert_eq!(launched, 3, "max_step {max_step}");
        assert_eq!(rig.state.records.instances.len(), 3);
        assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);
    }
}

/// Asserts that launches of one step have different acquire ids and host names, and that
/// the ids carry the pool.
#[test]
fn every_launch_has_its_own_acquire_id_and_name() {
    let mut limits = settings("gpu");
    limits.max_step = 3;
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1"), request("r2"), request("r3")];
    let plans = acquires(&rig.tick(at(0)));
    assert_eq!(plans.len(), 3);
    let ids: BTreeSet<String> = plans.iter().map(|p| p.acquire.to_string()).collect();
    let names: BTreeSet<String> = plans.iter().map(|p| p.name.clone()).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(names.len(), 3);
    assert!(ids.iter().all(|each| each.starts_with("gpu-")));
}

/// Asserts that a request holding a reservation gets no machine, that the same request
/// gets one once the reservation is given up, and that with two requests and one
/// reservation only the one without it is served. It catches buying a machine for a
/// request that already has a host coming.
#[test]
fn a_reserved_request_is_covered_until_the_reservation_is_given_up() {
    let mut rig = Rig::new(settings("gpu"));
    let mut busy = host("h1", "gpu");
    busy.busy = true;
    busy.mark = Some(Mark::Reserved);
    rig.input.hosts = vec![busy];
    rig.input.waiting = vec![reserved("r1", "h1")];
    assert!(acquires(&rig.tick(at(0))).is_empty());

    rig.input.hosts[0].mark = None;
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&rig.tick(at(5 * SECOND))).len(), 1);

    let mut limits = settings("gpu");
    limits.max_step = 5;
    let mut two = Rig::new(limits);
    two.input.waiting = vec![reserved("r1", "h1"), request("r2")];
    assert_eq!(acquires(&two.tick(at(0))).len(), 1);
}

/// Asserts that a free host that fits covers a request, whether or not the registry has
/// set it aside for it, and covers one request only. It catches a launch for a request a
/// free host can serve.
#[test]
fn a_free_host_covers_one_request() {
    let mut limits = settings("gpu");
    limits.max_step = 5;
    let mut rig = Rig::new(limits);
    rig.input.hosts = vec![host("h1", "gpu")];
    rig.input.waiting = vec![request("r1")];
    assert!(acquires(&rig.tick(at(0))).is_empty());

    rig.input.waiting = vec![request("r1"), request("r2")];
    assert_eq!(acquires(&rig.tick(at(5 * SECOND))).len(), 1);

    let mut assigned = host("h2", "gpu");
    assigned.mark = Some(Mark::Reserved);
    rig.input.hosts = vec![assigned];
    rig.input.waiting = vec![reserved("r3", "h2")];
    assert!(acquires(&rig.tick(at(10 * SECOND))).is_empty());
}

/// Asserts that a host that is busy, marked for another purpose, silent, in another pool,
/// too small, or of another operating system or architecture does not cover a request. It
/// catches a free host being counted that cannot serve the request.
#[test]
fn a_host_that_cannot_serve_does_not_cover() {
    let breakers: [fn(&mut HostFacts); 7] = [
        |h| h.busy = true,
        |h| h.mark = Some(Mark::Held),
        |h| h.state = HostState::Lost,
        |h| h.pool = "other".to_owned(),
        |h| h.shape.cpu_millis = 1000,
        |h| h.shape.os = "macos".to_owned(),
        |h| h.shape.arch = "aarch64".to_owned(),
    ];
    for break_it in breakers {
        let mut rig = Rig::new(settings("gpu"));
        let mut broken = host("h1", "gpu");
        break_it(&mut broken);
        rig.input.hosts = vec![broken];
        rig.input.waiting = vec![request("r1")];
        assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
    }
}

/// Asserts that a machine on its way covers one request that fits it, that a second
/// request needs its own, and that a request the machine's offer cannot fit is not
/// covered by it. It catches a launch repeated every step for one request, and a machine
/// credited to a request it cannot serve.
#[test]
fn a_machine_on_its_way_covers_one_request_that_fits() {
    let mut limits = settings("gpu");
    limits.max_step = 5;
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1")];
    rig.tick(at(0));
    rig.input.waiting = vec![request("r1"), request("r2")];
    assert_eq!(acquires(&rig.tick(at(5 * SECOND))).len(), 1);

    let mut big = request("r3");
    big.demand.resources =
        js_types::Resources::new(4000, 16 << 30, 2, Some("nvidia-a10".to_owned())).unwrap();
    rig.input.waiting = vec![big];
    assert!(acquires(&rig.tick(at(10 * SECOND))).is_empty());
    assert_eq!(rig.state.no_offer[&id("r3")], js_elastic::Dimension::Gpus);
}

/// Asserts that a machine on its way whose offer is of another operating system or
/// architecture does not cover a request: it catches a machine credited to a request it
/// cannot serve, which would then wait for a host that never fits.
#[test]
fn a_machine_on_its_way_of_another_platform_does_not_cover() {
    let breakers: [fn(&mut StoredOffer); 2] = [
        |offer| offer.os = "macos".to_owned(),
        |offer| offer.arch = "aarch64".to_owned(),
    ];
    for break_it in breakers {
        let mut rig = Rig::new(settings("gpu"));
        rig.input.waiting = vec![request("r1")];
        assert_eq!(acquires(&rig.tick(at(0))).len(), 1);
        break_it(&mut rig.state.records.instances[0].offer);
        assert_eq!(acquires(&rig.tick(at(5 * SECOND))).len(), 1);
    }
}

/// Asserts the choice among offers that fit: the cheaper first; at one price, fewer
/// gpus; then the lower id; then the lower zone. An offer that is not available, or has
/// no price, is never chosen.
#[test]
fn the_cheapest_fitting_offer_is_chosen_then_gpus_id_and_zone() {
    let zone = |name: &str, o: Offer| Offer {
        zone: Some(name.to_owned()),
        ..o
    };
    let cases = [
        (vec![offer("pricey", 90), offer("cheap", 60)], "cheap", "z1"),
        (
            vec![
                Offer {
                    gpus: 2,
                    ..offer("a", 75)
                },
                offer("z", 75),
            ],
            "z",
            "z1",
        ),
        (vec![offer("b", 75), offer("a", 75)], "a", "z1"),
        (
            vec![zone("z2", offer("a", 75)), zone("z1", offer("a", 75))],
            "a",
            "z1",
        ),
        (
            vec![
                Offer {
                    available: false,
                    ..offer("gone", 10)
                },
                Offer {
                    price_cents_per_hour: None,
                    ..offer("free", 10)
                },
                offer("real", 90),
            ],
            "real",
            "z1",
        ),
    ];
    for (offers, expected, expected_zone) in cases {
        let mut rig = Rig::new(settings("gpu"));
        rig.pool().offers = offers;
        rig.input.waiting = vec![request("r1")];
        let plan = acquires(&rig.tick(at(0))).remove(0);
        assert_eq!(plan.offer.id, expected);
        assert_eq!(plan.offer.zone.as_deref(), Some(expected_zone));
    }
}

/// Asserts that offers that are only unavailable or unpriced leave the request waiting
/// with no launch and no `no_offer`, since they fit.
#[test]
fn offers_that_fit_but_cannot_be_chosen_leave_the_request_waiting() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = vec![
        Offer {
            available: false,
            ..offer("gone", 10)
        },
        Offer {
            price_cents_per_hour: None,
            ..offer("free", 10)
        },
    ];
    rig.input.waiting = vec![request("r1")];
    assert!(rig.tick(at(0)).is_empty());
    assert!(rig.state.no_offer.is_empty());
    assert!(rig.state.blocked.is_empty());
}

/// Asserts that an offer of another operating system or architecture is never launched
/// for a request that names a platform, and that `no_offer` names the nearest miss:
/// the operating system when that is all that differs, the architecture when an offer
/// has the right system, and nothing when there are no offers.
#[test]
fn an_offer_of_another_platform_is_refused_and_the_miss_is_named() {
    let macos = Offer {
        os: "macos".to_owned(),
        ..offer("m", 60)
    };
    let arm = Offer {
        arch: "aarch64".to_owned(),
        ..offer("arm", 60)
    };
    let sandbox = Offer {
        exec: ExecMode::SandboxExec,
        ..offer("sb", 60)
    };
    let cases = [
        (vec![macos.clone()], Some(js_elastic::Dimension::Os)),
        (vec![arm.clone()], Some(js_elastic::Dimension::Arch)),
        (vec![macos, arm], Some(js_elastic::Dimension::Arch)),
        (vec![sandbox], Some(js_elastic::Dimension::Exec)),
        (Vec::new(), None),
    ];
    for (offers, miss) in cases {
        let mut rig = Rig::new(settings("gpu"));
        rig.pool().offers = offers;
        rig.input.waiting = vec![request("r1")];
        assert!(rig.tick(at(0)).is_empty());
        assert_eq!(rig.state.no_offer.get(&id("r1")).copied(), miss);
        assert!(rig.state.records.instances.is_empty());
    }
}

fn two_pools(first: Settings) -> Rig {
    let mut rig = Rig::new(first);
    rig.input.pools.push(pool(settings("second")));
    rig
}

/// Asserts that a request falls through the pools in order: to the second when the first
/// has no offer that fits, when its checks fail, and when its cap is reached; and that
/// the answers to one pool's acquires are matched to that pool's records and not to the
/// other's.
#[test]
fn a_request_falls_through_to_the_next_pool_and_answers_find_their_own_record() {
    let mut no_fit = two_pools(settings("first"));
    no_fit.input.pools[0].offers = vec![Offer {
        os: "macos".to_owned(),
        ..offer("m", 60)
    }];
    no_fit.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&no_fit.tick(at(0)))[0].pool, "second");

    let mut failing = two_pools(settings("first"));
    failing.input.pools[0].health = js_scaler::Health::Failing;
    failing.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&failing.tick(at(0)))[0].pool, "second");
    assert_eq!(failing.state.blocked["first"], Blocked::Unavailable);

    let mut full = two_pools(Settings {
        max_instances: 0,
        ..settings("first")
    });
    full.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&full.tick(at(0)))[0].pool, "second");
    assert_eq!(full.state.blocked["first"], Blocked::MaxInstances);

    let mut both = two_pools(settings("first"));
    both.input.waiting = vec![request("r1"), request("r2")];
    let plans = acquires(&both.tick(at(0)));
    assert_eq!(plans.len(), 2);
    both.input.acquired = vec![
        accepted(&plans[0], "i-first"),
        accepted(&plans[1], "i-second"),
        answered(&plans[0], js_elastic::Acquired::Unknown),
    ];
    both.input.acquired.push(js_scaler::AcquireResult {
        acquire: stranger(),
        attempt: 0,
        answer: js_elastic::Acquired::Unknown,
    });
    both.tick(at(SECOND));
    assert_eq!(
        both.state.records.instances[0].cloud_id.as_deref(),
        Some("i-first")
    );
    assert_eq!(
        both.state.records.instances[1].cloud_id.as_deref(),
        Some("i-second")
    );
    assert_eq!(both.phase(0), Phase::Booting);
    assert_eq!(both.phase(1), Phase::Booting);
    assert_eq!(both.state.refused, 1);
}

/// Asserts that a pool whose name cannot be part of an acquire id launches nothing and
/// says why, and does not panic.
#[test]
fn a_pool_name_that_is_not_an_id_launches_nothing() {
    let mut rig = Rig::new(settings("not a pool"));
    rig.input.waiting = vec![request("r1")];
    assert!(rig.tick(at(0)).is_empty());
    assert_eq!(rig.state.blocked["not a pool"], Blocked::PoolName);
    assert!(rig.state.records.instances.is_empty());
}

/// Asserts that a pool in plan mode launches nothing, keeps no record, shows what it
/// would launch within its caps, and is blocked as `Plan`.
#[test]
fn a_plan_pool_shows_its_launches_and_makes_none() {
    let mut limits = Settings {
        launch: LaunchMode::Plan,
        ..settings("gpu")
    };
    limits.max_instances = 1;
    limits.max_step = 5;
    let mut rig = Rig::new(limits);
    rig.input.waiting = vec![request("r1"), request("r2")];
    assert!(rig.tick(at(0)).is_empty());
    assert!(rig.state.records.instances.is_empty());
    assert_eq!(rig.state.blocked["gpu"], Blocked::Plan);
    assert_eq!(rig.state.plan.len(), 1);
    assert_eq!(rig.state.plan[0].request, id("r1"));
    assert_eq!(rig.state.plan[0].offer, "a10");
    assert_eq!(rig.state.plan[0].price_cents_per_hour, 60);
    assert_eq!(rig.state.plan[0].pool, "gpu");
    assert_eq!(rig.state.plan[0].zone.as_deref(), Some("z1"));
}

/// Asserts that a pool whose checks have not finished or fail, or whose list failed,
/// launches nothing and says which; and that the first reason in that order wins.
#[test]
fn a_pool_that_is_not_ready_launches_nothing() {
    let cases = [
        (js_scaler::Health::Checking, true, Blocked::Checking),
        (js_scaler::Health::Failing, true, Blocked::Unavailable),
        (js_scaler::Health::Available, false, Blocked::ListFailed),
        (js_scaler::Health::Failing, false, Blocked::Unavailable),
    ];
    for (health, listed_ok, expected) in cases {
        let mut rig = Rig::new(settings("gpu"));
        rig.pool().health = health;
        rig.pool().held = listed_ok.then(Vec::new);
        rig.input.waiting = vec![request("r1")];
        assert!(rig.tick(at(0)).is_empty());
        assert_eq!(rig.state.blocked["gpu"], expected);
    }
}
