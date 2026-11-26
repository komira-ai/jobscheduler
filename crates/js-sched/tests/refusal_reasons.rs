//! Why a host refused a request: the kind of the predicate that refused and the figures
//! it compared, per host, through `Scheduler::take_refusals`, and the counts by kind in
//! `LoadCounters`.

#[path = "load_rig/mod.rs"]
mod load_rig;

use js_sched::{
    HostRefusal, LoadConfig, MAX_KEPT_REFUSALS, Refusal, RefusalKind, Refusals, Scheduler,
};
use js_types::sched::Resources;

use load_rig::{GIB, Rig, cold, leases, w, warm};

/// An 8-core worker with 32 GiB.
fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// A host with cores to spare and 32 GiB.
fn wide() -> Resources {
    Resources::new(100_000, 32 * GIB)
}

fn only(refusals: Refusals) -> HostRefusal {
    assert_eq!(refusals.dropped, 0);
    assert_eq!(refusals.kept.len(), 1, "{:?}", refusals.kept);
    refusals.kept.into_iter().next().expect("one")
}

/// Behaviour: a host with no reading, holding four leases predicted at two cores, refuses
/// a fifth for the CPU slack and says so with the figures: 8000 m charged, 2000 m new,
/// limit 8800 m (8 cores at the default slack of 1.1). Nothing is measured.
#[test]
fn a_host_refused_for_cpu_slack_reports_the_cpu_figures() {
    let mut r = Rig::on();
    r.up("a", node());
    for _ in 0..4 {
        r.action(2_000, GIB, Some(warm(2_000, 0)));
    }
    assert_eq!(r.tick().len(), 4);
    assert_eq!(
        r.s.take_refusals(),
        Refusals::default(),
        "nothing refused yet"
    );
    r.action(2_000, GIB, Some(warm(2_000, 0)));
    assert!(r.tick().is_empty());
    let refused = only(r.s.take_refusals());
    assert_eq!(refused.worker, w("a"));
    assert_eq!(
        refused.refusal,
        Refusal {
            kind: RefusalKind::Cpu,
            counted: 8_000,
            new: 2_000,
            limit: 8_800,
            measured_cpu_millis: None,
        }
    );
    let c = r.s.load_counters();
    assert_eq!((c.refused_cpu, c.blocked), (1, 1));
    assert_eq!(
        c.refused_predicted_memory + c.refused_booked_memory + c.refused_cold_cap,
        0
    );
}

/// Behaviour: on a host that measures, the CPU refusal carries the measurement: eight
/// leases predicted at 500 m, 9000 m measured over the window, a new lease of 1000 m
/// against 8800 m.
#[test]
fn a_measured_host_refused_for_cpu_reports_the_measurement() {
    let mut r = Rig::on();
    r.up("a", node());
    for _ in 0..8 {
        r.action(1_000, GIB, Some(warm(500, 0)));
    }
    let held = leases(&r.tick());
    assert_eq!(held.len(), 8);
    r.window("a", 0, 0, &held);
    r.window("a", 9_000, 0, &held);
    r.action(1_000, GIB, Some(warm(1_000, 0)));
    assert!(r.tick().is_empty());
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        Refusal {
            kind: RefusalKind::Cpu,
            counted: 9_000,
            new: 1_000,
            limit: 8_800,
            measured_cpu_millis: Some(9_000),
        }
    );
}

/// Behaviour: eight leases predicted at 4 GiB fill a 32 GiB host; a ninth is refused for
/// predicted memory (32 GiB counted, 4 GiB new, limit 32 GiB) and not for CPU.
#[test]
fn a_host_refused_by_predicted_memory_reports_the_memory_figures() {
    let mut r = Rig::on();
    r.up("a", wide());
    for _ in 0..9 {
        r.action(250, 8 * GIB, Some(warm(250, 4 * GIB)));
    }
    assert_eq!(r.tick().len(), 8);
    let refused = only(r.s.take_refusals());
    assert_eq!(refused.worker, w("a"));
    assert_eq!(
        refused.refusal,
        Refusal {
            kind: RefusalKind::PredictedMemory,
            counted: 32 * GIB,
            new: 4 * GIB,
            limit: 32 * GIB,
            measured_cpu_millis: None,
        }
    );
    let c = r.s.load_counters();
    assert_eq!((c.refused_predicted_memory, c.refused_cpu), (1, 0));
}

/// Behaviour: with the predicate's memory slack at 50% (limit 48 GiB), a lease of 16 GiB
/// beside one of 20 GiB passes the predicate and is refused by the flat memory guard,
/// which reports the flat sum held (20 GiB), the new 16 GiB and its limit of 32 GiB.
#[test]
fn a_host_refused_by_the_flat_guard_reports_booked_memory() {
    let mut r = Rig::new(LoadConfig {
        s_mem_permille: 500,
        ..LoadConfig::default()
    });
    r.up("a", Resources::new(100_000, 32 * GIB));
    r.action(1_000, 8 * GIB, Some(warm(1_000, 20 * GIB)));
    assert_eq!(r.tick().len(), 1);
    r.action(1_000, 8 * GIB, Some(warm(1_000, 16 * GIB)));
    assert!(r.tick().is_empty());
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        Refusal {
            kind: RefusalKind::BookedMemory,
            counted: 20 * GIB,
            new: 16 * GIB,
            limit: 32 * GIB,
            measured_cpu_millis: None,
        }
    );
    let c = r.s.load_counters();
    assert_eq!(
        (c.refused_booked_memory, c.refused_predicted_memory),
        (1, 0)
    );
}

/// Behaviour: an 8-core host admits one cold lease per window; the second is refused for
/// the cold cap (1 admitted, 1 new, cap 1) and counted as cold-cap alone.
#[test]
fn a_host_refused_by_the_cold_cap_reports_the_cap() {
    let mut r = Rig::on();
    r.up("a", node());
    for _ in 0..2 {
        r.action(500, GIB, Some(cold(500, GIB)));
    }
    assert_eq!(r.tick().len(), 1);
    assert_eq!(
        only(r.s.take_refusals()).refusal,
        Refusal {
            kind: RefusalKind::ColdCap,
            counted: 1,
            new: 1,
            limit: 1,
            measured_cpu_millis: None,
        }
    );
    let c = r.s.load_counters();
    assert_eq!((c.refused_cold_cap, c.cold_deferred, c.blocked), (1, 1, 1));
}

/// Behaviour: every host that refused is reported, each with its own figures: two hosts,
/// one full of CPU and one full of memory, refuse the same request, and each says why.
#[test]
fn each_refusing_host_is_reported_with_its_own_reason() {
    let mut r = Rig::on();
    r.up("cpu", node());
    r.up("mem", wide());
    r.feed(js_sched::Event::Cordon { worker: w("mem") });
    for _ in 0..4 {
        r.action(2_000, GIB, Some(warm(2_000, 0)));
    }
    assert_eq!(r.tick().len(), 4);
    r.feed(js_sched::Event::Uncordon { worker: w("mem") });
    r.feed(js_sched::Event::Cordon { worker: w("cpu") });
    for _ in 0..8 {
        r.action(250, GIB, Some(warm(250, 4 * GIB)));
    }
    assert_eq!(r.tick().len(), 8);
    r.feed(js_sched::Event::Uncordon { worker: w("cpu") });
    let _ = r.s.take_refusals();
    r.action(2_000, GIB, Some(warm(2_000, 4 * GIB)));
    assert!(r.tick().is_empty());
    let by_host: Vec<(String, RefusalKind)> =
        r.s.take_refusals()
            .kept
            .into_iter()
            .map(|h| (h.worker.as_str().to_owned(), h.refusal.kind))
            .collect();
    assert_eq!(
        by_host,
        vec![
            ("cpu".to_owned(), RefusalKind::Cpu),
            ("mem".to_owned(), RefusalKind::PredictedMemory),
        ]
    );
}

/// Behaviour: taking the refusals empties the buffer, so each is reported once.
#[test]
fn taking_the_refusals_empties_the_buffer() {
    let mut r = Rig::on();
    r.up("a", wide());
    for _ in 0..9 {
        r.action(250, 8 * GIB, Some(warm(250, 4 * GIB)));
    }
    r.tick();
    assert_eq!(r.s.take_refusals().kept.len(), 1);
    assert_eq!(r.s.take_refusals(), Refusals::default());
}

/// Behaviour: the buffer holds at most `MAX_KEPT_REFUSALS` between takes; what does not
/// fit is counted in `dropped`, and the counter by kind still counts every refusal.
/// One request refused by more hosts than the buffer holds: each host is full by memory.
#[test]
fn the_buffer_is_bounded_and_counts_what_it_drops() {
    let mut r = Rig::on();
    let hosts = MAX_KEPT_REFUSALS + 50;
    for n in 0..hosts {
        r.up(&format!("h{n:04}"), Resources::new(100_000, 4 * GIB));
    }
    for _ in 0..hosts {
        r.action(1, GIB, Some(warm(1, 4 * GIB)));
    }
    let mut placed = 0;
    while placed < hosts {
        let round = r.tick().len();
        assert!(round > 0, "a round places some");
        placed += round;
    }
    let _ = r.s.take_refusals();
    let before = r.s.load_counters().refused_predicted_memory;
    r.action(1, GIB, Some(warm(1, 4 * GIB)));
    assert!(r.tick().is_empty());
    let refusals = r.s.take_refusals();
    assert_eq!(refusals.kept.len(), MAX_KEPT_REFUSALS);
    assert_eq!(refusals.dropped, 50);
    assert_eq!(
        r.s.load_counters().refused_predicted_memory - before,
        u64::try_from(hosts).expect("fits")
    );
}

/// Behaviour: a scheduler never asked for refusals holds none.
#[test]
fn a_new_scheduler_has_no_refusals() {
    assert_eq!(Scheduler::new(1).take_refusals(), Refusals::default());
}

/// Behaviour: each kind has the name logs and metrics spell it with, and `ALL` lists each
/// once.
#[test]
fn each_kind_has_its_name() {
    let names: Vec<&str> = RefusalKind::ALL.iter().map(|kind| kind.as_str()).collect();
    assert_eq!(
        names,
        [
            "cpu",
            "predicted-memory",
            "booked-memory",
            "cold-cap",
            "cpu-curve"
        ]
    );
}
