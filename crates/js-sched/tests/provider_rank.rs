//! Pass 1 over the machine providers of a platform, in order (design section 5.6, steps 3 and
//! 4): of the workers that could take a request now, those of the first provider take it and
//! the score decides among them alone; a whole-machine request's mark is chosen by drain cost
//! across every provider, the provider telling apart only workers that cost the same, and a
//! quiet worker of the first provider takes one at once. Every time is farm time fed to the
//! scheduler.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[path = "load_rig/mod.rs"]
mod load_rig;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_sched::{Event, ProviderRank, Scheduler};
use js_types::ProviderId;
use js_types::sched::{FarmTime, NodePhase, OperationId, Resources, StartLease, WorkerId};
use js_whole::WholeNodeConfig;

use load_rig::{GIB, Rig, w, warm};
use whole_rig::{Farm, secs};

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

fn on(starts: &[StartLease]) -> Vec<&str> {
    starts.iter().map(|s| s.worker.as_str()).collect()
}

/// The rank of `provider`, listed at `position`.
fn rank(provider: &str, position: u32) -> Option<ProviderRank> {
    Some(ProviderRank::new(
        ProviderId::new(provider).unwrap(),
        position,
    ))
}

/// Loads `worker` with `count` one-core actions by cordoning the others for the round.
fn load(r: &mut Rig, worker: &str, others: &[&str], count: u64) {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    for _ in 0..count {
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    }
    assert_eq!(on(&r.tick()), vec![worker; count as usize]);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
}

/// `static-1` and `cloud-1`, both free, the one named `loaded` holding four one-core
/// actions: the one the score prefers (it fills before it opens).
fn two_free(loaded: &str) -> Rig {
    let mut r = Rig::on();
    r.up("static-1", node());
    r.up("cloud-1", node());
    let other = if loaded == "static-1" {
        "cloud-1"
    } else {
        "static-1"
    };
    load(&mut r, loaded, &[other], 4);
    r
}

/// Asserts that, with a free capable worker of each of two providers, the next action goes
/// to the worker of the provider listed first, whichever of the two the score prefers, and
/// that reversing the order reverses the choice. Without ranks the score picks the loaded
/// worker in both layouts, so a rank that the score could outweigh would lose one case of
/// each order. It catches a rank that is ignored, and one added to the score as a weight.
#[test]
fn the_first_provider_in_order_wins_among_free_hosts() {
    for loaded in ["static-1", "cloud-1"] {
        let mut unranked = two_free(loaded);
        unranked.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
        assert_eq!(on(&unranked.tick()), [loaded], "the score alone");
        for (static_at, cloud_at, first) in [(0, 1, "static-1"), (1, 0, "cloud-1")] {
            let mut r = two_free(loaded);
            r.s.set_provider_rank(&w("static-1"), rank("static", static_at));
            r.s.set_provider_rank(&w("cloud-1"), rank("cloud", cloud_at));
            r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
            assert_eq!(on(&r.tick()), [first], "{loaded} loaded, {first} first");
        }
    }
}

/// Asserts that a free worker of a later provider takes an action that no worker of the
/// first provider can take now: one full under the CPU slack, one draining, one
/// quarantined (tainted) and one too small for it. It catches an earlier provider that
/// keeps the action waiting for its own workers.
#[test]
fn a_free_worker_of_a_later_provider_takes_what_the_first_cannot_now() {
    type Setup = fn(&mut Rig);
    let cases: [(&str, Setup); 4] = [
        ("full", |r| load(r, "static-1", &["cloud-1"], 8)),
        ("draining", |r| {
            let deadline = FarmTime::from_millis(600_000);
            r.feed(Event::Drain {
                worker: w("static-1"),
                deadline,
            });
        }),
        ("tainted", |r| {
            let phase = NodePhase::Quarantined("taint".to_owned());
            r.feed(Event::Phase {
                worker: w("static-1"),
                phase,
            });
        }),
        ("too small", |_| {}),
    ];
    for (case, setup) in cases {
        let mut r = Rig::on();
        let small = Resources::new(500, 32 * GIB);
        r.up("static-1", if case == "too small" { small } else { node() });
        r.up("cloud-1", node());
        setup(&mut r);
        r.s.set_provider_rank(&w("static-1"), rank("static", 0));
        r.s.set_provider_rank(&w("cloud-1"), rank("cloud", 1));
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
        assert_eq!(on(&r.tick()), ["cloud-1"], "{case}");
    }
}

/// Asserts that within the first provider the score decides as it does with no ranks at
/// all: of two free workers of the first provider and a free worker of a later one that the
/// score prefers to both (it holds four actions, the fill closest to full), the action goes to
/// the first provider's worker the score prefers (`static-2`, which holds three); and with the
/// two equal, to the worker the unranked tie-break picks between those two alone. It catches a
/// score that crosses providers, and a changed order within one.
#[test]
fn within_one_provider_load_decides() {
    // `cloud-1` holds four actions and `static-2` holds `static_2`; ranked, or unranked with
    // `cloud-1` cordoned or not.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Ranked,
        Unranked,
        CloudCordoned,
    }
    let rig = |static_2: u64, mode: Mode| {
        let mut r = Rig::on();
        for name in ["static-1", "static-2", "cloud-1"] {
            r.up(name, node());
        }
        load(&mut r, "cloud-1", &["static-1", "static-2"], 4);
        load(&mut r, "static-2", &["static-1", "cloud-1"], static_2);
        if mode == Mode::Ranked {
            r.s.set_provider_rank(&w("static-1"), rank("static", 0));
            r.s.set_provider_rank(&w("static-2"), rank("static", 0));
            r.s.set_provider_rank(&w("cloud-1"), rank("cloud", 1));
        }
        if mode == Mode::CloudCordoned {
            r.feed(Event::Cordon {
                worker: w("cloud-1"),
            });
        }
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
        let starts = r.tick();
        on(&starts)
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>()
    };
    assert_eq!(rig(3, Mode::Unranked), ["cloud-1"], "the score alone");
    assert_eq!(rig(3, Mode::Ranked), ["static-2"]);
    // Equal: the tie-break between the two, the same lease asked, as the score gives it
    // with the later provider's worker out of the way.
    assert_eq!(rig(0, Mode::Ranked), rig(0, Mode::CloudCordoned));
}

/// Asserts that the scheduler keeps the rank it is told for a registered worker, across a
/// registration, clears it on `None`, and keeps none for a worker that never registered. It
/// catches a rank lost when the agent says hello again, which would let the score cross
/// providers after every restart of an agent.
#[test]
fn a_rank_is_kept_for_a_registered_worker_only() {
    let mut r = Rig::on();
    r.up("cloud-1", node());
    r.s.set_provider_rank(&w("cloud-1"), rank("cloud", 1));
    r.up("cloud-1", node());
    let kept = r.s.provider_rank(&w("cloud-1"));
    assert_eq!(kept.map(ProviderRank::position), Some(1));
    assert_eq!(
        kept.map(ProviderRank::provider),
        Some(&ProviderId::new("cloud").unwrap())
    );
    r.s.set_provider_rank(&w("cloud-1"), None);
    assert_eq!(r.s.provider_rank(&w("cloud-1")), None);
    r.s.set_provider_rank(&w("nobody"), rank("cloud", 1));
    assert_eq!(r.s.provider_rank(&w("nobody")), None);
}

fn with_settings() -> Farm {
    let cfg = WholeNodeConfig {
        max_draining: 1,
        ..WholeNodeConfig::dedicated(4)
    };
    Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(cfg),
    )
}

/// A farm of `a` and `b` with settings, each holding one lease of `walls` (seconds), ranked
/// as `ranks` says, the clock past the start.
fn busy(walls: [u64; 2], ranks: [Option<ProviderRank>; 2]) -> Farm {
    let mut f = with_settings();
    f.worker("a");
    f.worker("b");
    f.lease_on("a", secs(walls[0]));
    f.lease_on("b", secs(walls[1]));
    let [a, b] = ranks;
    f.s.set_provider_rank(&WorkerId::new("a"), a);
    f.s.set_provider_rank(&WorkerId::new("b"), b);
    f.at(5_000);
    f.beat();
    f
}

/// The worker the mark of `op` is on.
fn marked(f: &Farm, op: OperationId) -> Option<String> {
    f.s.whole_mark(op).map(|mark| mark.worker.to_string())
}

/// Asserts that a whole-machine request marks the worker with the least tail across every
/// provider: `b`, of the later provider, ends in a minute and `a`, of the first, in an hour,
/// and `b` is marked. With equal tails the worker of the earlier provider is marked, though
/// its name comes second, while with no ranks the first name is. With a quiet worker of each
/// provider, the first provider's takes the request at once. It catches provider order put
/// before the tail, and a tie broken by the name alone.
#[test]
fn whole_host_tail_cost_picks_across_providers() {
    let mut f = busy([3_600, 60], [rank("static", 0), rank("cloud", 1)]);
    let op = f.whole();
    assert_eq!(marked(&f, op).as_deref(), Some("b"));

    let mut f = busy([60, 60], [None, None]);
    let op = f.whole();
    assert_eq!(marked(&f, op).as_deref(), Some("a"), "no ranks: the name");
    let mut f = busy([60, 60], [rank("cloud", 1), rank("static", 0)]);
    let op = f.whole();
    assert_eq!(
        marked(&f, op).as_deref(),
        Some("b"),
        "equal tails: the provider"
    );

    for (ranks, quiet) in [
        ([None, None], "a"),
        ([rank("cloud", 1), rank("static", 0)], "b"),
    ] {
        let mut f = with_settings();
        f.worker("a");
        f.worker("b");
        let [a, b] = ranks;
        f.s.set_provider_rank(&WorkerId::new("a"), a);
        f.s.set_provider_rank(&WorkerId::new("b"), b);
        f.submit_whole(None);
        let grants = f.grants();
        let workers: Vec<&str> = grants.iter().map(|g| g.worker.as_str()).collect();
        assert_eq!(workers, [quiet]);
    }
}
