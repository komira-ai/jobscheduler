//! The offer a launch takes is pass 2's (`js_sched::acquire::pick`), not the scaler's own
//! choice; the caps, the share of the pool's provider and its `provider_limits` still bound
//! it; and a request is asked about only when no free host and no machine on its way can
//! serve it (`js_sched::acquire::needs_acquire`).

mod common;

use common::*;
use js_elastic::Offer;
use js_scaler::{Blocked, Phase, Settings, launchable};

/// Live settings of the pool "gpu" that may launch only the instance types `allowed`.
fn allowing(allowed: &[&str]) -> Settings {
    Settings {
        allowed_instance_types: Some(allowed.iter().map(|each| (*each).to_owned()).collect()),
        ..settings("gpu")
    }
}

/// The ids of the offers `actions` acquire, in order.
fn acquired(actions: &[js_scaler::Action]) -> Vec<String> {
    acquires(actions)
        .into_iter()
        .map(|plan| plan.offer.id)
        .collect()
}

/// A record of the pool "gpu" in `phase` whose machine is too small for any request, so
/// it covers none and only counts.
fn counted(phase: Phase) -> js_scaler::Record {
    let mut each = record(phase, 60, 0);
    each.offer.cpu_millis = 1000;
    each
}

/// Asserts that the scaler stages the offer pass 2 picks, not the one its old rule chose:
/// of two offers at one price, the one with less spare CPU although its id sorts later
/// (the old rule took the lower id); of two offers at different prices, the dearer one when
/// the pool's `allowed_instance_types` leaves out the cheaper; and when pass 2 picks
/// nothing (no fitting offer is of an allowed type), nothing is staged, nothing is shown,
/// and the request stays waiting and is launched for once the limit admits an offer.
/// Catches a scaler that still runs a choice of its own beside the pick.
#[test]
fn the_scaler_no_longer_chooses() {
    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = vec![
        Offer {
            cpu_millis: 16_000,
            ..offer("a", 75)
        },
        offer("b", 75),
    ];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquired(&rig.tick(at(0))), ["b"]);

    let mut rig = Rig::new(allowing(&["dear"]));
    rig.pool().offers = vec![offer("cheap", 60), offer("dear", 90)];
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    assert_eq!(plan.offer.id, "dear");
    assert_eq!(plan.offer.price_cents_per_hour, Some(90));
    assert_eq!(rig.record(0).price_cents_per_hour, 90);

    let mut rig = Rig::new(allowing(&["elsewhere"]));
    rig.input.waiting = vec![request("r1")];
    assert!(acquires(&rig.tick(at(0))).is_empty());
    assert!(rig.state.blocked.is_empty());
    assert!(rig.state.records.instances.is_empty());
    assert!(acquires(&rig.tick(at(SECOND))).is_empty());
    rig.pool().settings.allowed_instance_types = Some(vec!["a10".to_owned()]);
    assert_eq!(acquired(&rig.tick(at(2 * SECOND))), ["a10"]);
}

/// Asserts that an offer pass 2 picked is staged only when the caps allow it. The pool
/// may launch only `dear`, at 90 cents an hour, beside a cheaper `cheap` the old rule would
/// have taken: with `max_instances` reached the pick is not launched and the pool shows
/// `MaxInstances`; with the hourly cap one cent below 90 it shows `MaxSpend` (the cheaper
/// offer would have passed it), and at 90 exactly `dear` is launched. Catches a picked
/// offer that skips the caps, and caps checked against an offer other than the one picked.
#[test]
fn the_caps_still_apply_to_a_decided_offer() {
    let full = Settings {
        max_instances: 1,
        ..allowing(&["dear"])
    };
    let mut rig = Rig::new(full);
    rig.pool().offers = vec![offer("cheap", 60), offer("dear", 90)];
    rig.state.records.instances = vec![counted(Phase::Booting)];
    rig.input.waiting = vec![request("r1")];
    assert!(acquires(&rig.tick(at(0))).is_empty());
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);

    for (cap, launched) in [(89, false), (90, true)] {
        let spend = Settings {
            max_spend_per_hour: cap,
            ..allowing(&["dear"])
        };
        let mut rig = Rig::new(spend);
        rig.pool().offers = vec![offer("cheap", 60), offer("dear", 90)];
        rig.input.waiting = vec![request("r1")];
        let actions = rig.tick(at(0));
        match launched {
            true => {
                assert_eq!(acquired(&actions), ["dear"], "cap {cap}");
                assert!(rig.state.blocked.is_empty(), "cap {cap}");
            }
            false => {
                assert!(acquires(&actions).is_empty(), "cap {cap}");
                assert_eq!(rig.state.blocked["gpu"], Blocked::MaxSpend, "cap {cap}");
            }
        }
    }
}

/// Asserts that a machine on its way holds back a second acquire only for a request its
/// offer can serve. A request whose machine is launching gets no second one, step after
/// step; a request for two accelerators beside it, which the one-accelerator machine cannot
/// serve, gets a machine of the two-accelerator offer; and a request for another
/// architecture gets a machine of that architecture's offer although the first machine is
/// still on its way. Catches any machine on its way holding back every acquire, and one
/// holding back none.
#[test]
fn a_booting_machine_that_can_serve_suppresses_a_second_acquire() {
    let pair = Offer {
        accelerator: accel("nvidia-a10", 2),
        ..offer("pair", 120)
    };
    let arm = Offer {
        arch: "aarch64".to_owned(),
        ..offer("arm", 60)
    };
    let mut limits = settings("gpu");
    limits.max_step = 5;
    let mut rig = Rig::new(limits);
    rig.pool().offers = vec![offer("a10", 60), pair, arm];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquired(&rig.tick(at(0))), ["a10"]);
    assert!(acquires(&rig.tick(at(SECOND))).is_empty());
    assert!(acquires(&rig.tick(at(2 * SECOND))).is_empty());

    let mut two = request("r2");
    two.demand.resources =
        js_types::Resources::new(4000, 16 << 30, 2, Some("nvidia-a10".to_owned())).unwrap();
    two.demand.accelerator = js_types::AcceleratorNeed::booked(&two.demand.resources);
    let mut other_arch = request("r3");
    other_arch.demand.arch = Some("aarch64".to_owned());
    rig.input.waiting = vec![request("r1"), two, other_arch];
    assert_eq!(acquired(&rig.tick(at(3 * SECOND))), ["pair", "arm"]);
    assert_eq!(rig.phase(0), Phase::Launching);
}

/// Asserts that a free host that fits holds back every acquire, whichever machine provider
/// brought it: a host the scaler did not launch (another provider's, such as a static one)
/// and the host of a machine the scaler launched each leave the request with no machine,
/// although the pool's provider has a cheap fitting offer; with that host busy, or free but
/// of another architecture, the request gets one. Catches a gate that counts the hosts of
/// one provider only, and one that counts a free host that cannot serve the request.
#[test]
fn a_free_machine_of_any_provider_suppresses_every_acquire() {
    for busy in [false, true] {
        let mut rig = Rig::new(settings("gpu"));
        rig.pool().offers = vec![offer("cheap", 10)];
        let mut other = host("static-1", "gpu");
        other.busy = busy;
        rig.input.hosts = vec![other];
        rig.input.waiting = vec![request("r1")];
        let launched = acquired(&rig.tick(at(0)));
        match busy {
            false => assert!(launched.is_empty()),
            true => assert_eq!(launched, ["cheap"]),
        }
    }

    let mut rig = Rig::new(settings("gpu"));
    rig.pool().offers = vec![offer("cheap", 10)];
    let mut other_arch = host("static-2", "gpu");
    shape(&mut other_arch).arch = "aarch64".to_owned();
    rig.input.hosts = vec![other_arch];
    rig.input.waiting = vec![request("r1")];
    assert_eq!(acquired(&rig.tick(at(0))), ["cheap"]);

    for busy in [false, true] {
        let (mut rig, plan) = registered(settings("gpu"), "i-1", 2 * SECOND);
        rig.input.hosts[0].busy = busy;
        rig.input.waiting = vec![request("r2")];
        let launched = acquires(&rig.tick(at(3 * SECOND)));
        assert_eq!(launched.len(), usize::from(busy), "busy {busy}");
        assert_eq!(rig.state.records.instances[0].name, plan.name);
    }
}

/// Asserts the provider's share of the pool (its `provider_limits` `max_machines`): a
/// provider holding one machine with a share of one launches nothing and the pool shows
/// `MaxInstances`, though `max_instances` is four; with a share of two, the same machine
/// held and two requests waiting, one is launched; with a share of two and three requests
/// in one step, two machines are launched, the first counted against the share of the
/// second. Catches a share that is not read, one compared with the wrong operator, and one
/// that counts only the machines of earlier steps.
#[test]
fn a_provider_at_its_share_launches_nothing() {
    let shared = |share| Settings {
        provider_share: Some(share),
        max_step: 3,
        ..settings("gpu")
    };
    let mut rig = Rig::new(shared(1));
    rig.state.records.instances = vec![counted(Phase::Booting)];
    rig.input.waiting = vec![request("r1")];
    assert!(acquires(&rig.tick(at(0))).is_empty());
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);

    let mut rig = Rig::new(shared(2));
    rig.state.records.instances = vec![counted(Phase::Booting)];
    rig.input.waiting = vec![request("r1"), request("r2")];
    assert_eq!(acquired(&rig.tick(at(0))), ["a10"]);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);

    let mut rig = Rig::new(shared(2));
    rig.input.waiting = vec![request("r1"), request("r2"), request("r3")];
    assert_eq!(acquired(&rig.tick(at(0))), ["a10", "a10"]);
    assert_eq!(rig.state.blocked["gpu"], Blocked::MaxInstances);
}

/// Asserts that the registry's `launchable` answer comes from the same pick: no when the
/// pool may launch no type of its catalog, or when no offer is under its price limit (one
/// cent below the offer), yes at the limit; no when the provider holds its share, and yes
/// when the machine it holds is idle and would be released. Catches a second choice in
/// `launchable` that reads none of the pool's limits.
#[test]
fn launchable_asks_the_same_pick() {
    let can = |rig: &Rig| launchable(&rig.state, &rig.input, at(3 * SECOND), &demand());
    assert!(!can(&Rig::new(allowing(&["elsewhere"]))));
    assert!(can(&Rig::new(allowing(&["a10"]))));
    for (limit, answer) in [(59, false), (60, true)] {
        let priced = Settings {
            max_price_cents_per_hour: Some(limit),
            ..settings("gpu")
        };
        assert_eq!(can(&Rig::new(priced)), answer, "limit {limit}");
    }

    let shared = Settings {
        provider_share: Some(1),
        ..settings("gpu")
    };
    let mut rig = Rig::new(shared.clone());
    rig.state.records.instances = vec![counted(Phase::Booting)];
    assert!(!can(&rig));
    let (rig, _) = registered(shared, "i-1", 2 * SECOND);
    assert!(can(&rig));
    let mut busy = rig;
    busy.input.hosts[0].busy = true;
    assert!(!can(&busy));
}
