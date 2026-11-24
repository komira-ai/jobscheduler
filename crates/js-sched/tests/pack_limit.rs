//! The pack limit through the scheduler: a host is packed (the tightest fit first) only
//! while its bookings with the new lease stay within 0.8x of its CPU and of its memory;
//! once no host stays within it, the lease goes to the least full by booked share. Logical
//! time only; no assertion reads a clock.

#[path = "load_rig/mod.rs"]
mod load_rig;

use std::collections::BTreeMap;
use std::time::Duration;

use js_sched::Scheduler;
use js_types::sched::{LeaseKind, OperationId, Resources, StartLease};
use js_whole::WholeNodeConfig;

use load_rig::{GIB, Rig, caps, w, warm};

/// A host of ten cores: 8 000 millicores is its 0.8x pack limit, 10 000 its capacity.
const CPU: u64 = 10_000;
const LIMIT: u64 = 8_000;
const CORE: u64 = 1_000;

fn node() -> Resources {
    Resources::new(CPU, 64 * GIB)
}

/// A scheduler with the default placement settings and the workers `names` up.
fn farm(names: &[&str]) -> Rig {
    let mut r = Rig::on();
    for name in names {
        r.up(name, node());
    }
    r
}

/// Puts `count` actions of `cpu` millicores on `worker`: the others are cordoned for the
/// round, so the placement is forced there.
fn load(r: &mut Rig, worker: &str, others: &[&str], count: u64, cpu: u64) {
    for other in others {
        r.feed(js_sched::Event::Cordon { worker: w(other) });
    }
    for _ in 0..count {
        r.action(cpu, GIB, Some(warm(cpu, 0)));
    }
    let mut placed = 0;
    for _ in 0..8 {
        let starts = r.tick();
        assert!(starts.iter().all(|s| s.worker.as_str() == worker));
        placed += starts.len() as u64;
    }
    assert_eq!(placed, count);
    for other in others {
        r.feed(js_sched::Event::Uncordon { worker: w(other) });
    }
}

/// The CPU booked on `worker`, in millicores.
fn booked(r: &Rig, worker: &str) -> u64 {
    r.s.booked(&w(worker)).expect("registered").cpu_millis
}

/// Submits one action of `cpu` millicores, runs a round and says where it went.
fn place(r: &mut Rig, cpu: u64) -> String {
    r.action(cpu, GIB, Some(warm(cpu, 0)));
    let starts = r.tick();
    assert_eq!(starts.len(), 1, "the action was placed");
    starts[0].worker.as_str().to_owned()
}

/// Behaviour: a burst of sixty one-core jobs submitted together over six empty hosts of ten
/// cores. The first eight go to one host (its 0.8x limit is eight cores, reached exactly);
/// the ninth goes to another host (this one would pass the limit with it) and that host is
/// filled to the limit the same way, and so on until every host is at the limit (48 jobs).
/// From then on no host is within the limit with another job, so each job goes to the least
/// full host: the next six take every host to nine cores, the last six to ten. At each step
/// the job went to the host the rule names, and no host was booked above its 1.0x while
/// another host could still take the job within it; at the end every host is full at 1.0x.
///
/// Catches: best fit with no pack limit, which packs the first host to its whole ten cores
/// before the second is touched, so the ninth job lands on the first host.
#[test]
fn a_burst_fills_each_host_to_the_limit_then_goes_to_the_least_full() {
    const HOSTS: [&str; 6] = ["h1", "h2", "h3", "h4", "h5", "h6"];
    let mut r = farm(&HOSTS);
    let ops: Vec<OperationId> = (0..60)
        .map(|_| r.action(CORE, GIB, Some(warm(CORE, 0))))
        .collect();
    let mut starts: Vec<StartLease> = r.tick();
    assert_eq!(starts.len(), 60, "every job is placed in the burst");
    starts.sort_by_key(|s| ops.iter().position(|op| *op == s.operation));

    let mut model: BTreeMap<&str, u64> = HOSTS.iter().map(|h| (*h, 0)).collect();
    let mut order = Vec::new();
    for (n, start) in starts.iter().enumerate() {
        let host = HOSTS
            .iter()
            .copied()
            .find(|h| *h == start.worker.as_str())
            .expect("a known host");
        order.push(host);
        let after = |h: &&str| model[h] + CORE;
        let within: Vec<&str> = HOSTS
            .iter()
            .copied()
            .filter(|h| after(h) <= LIMIT)
            .collect();
        if within.is_empty() {
            let least = model.values().min().copied().expect("hosts");
            assert_eq!(
                model[host], least,
                "job {n}: all past the limit, least full"
            );
        } else {
            let fullest = within.iter().map(|h| model[h]).max().expect("a host");
            assert!(within.contains(&host), "job {n}: went past the limit");
            assert_eq!(model[host], fullest, "job {n}: packs the fullest within it");
        }
        let roomy = HOSTS.iter().any(|h| after(h) <= CPU);
        *model.get_mut(host).expect("host") += CORE;
        assert!(
            !roomy || model[host] <= CPU,
            "job {n}: {host} booked past 1.0x while another host fit the job within it"
        );
    }
    assert!(order[..8].iter().all(|h| *h == order[0]), "{order:?}");
    assert_ne!(order[8], order[0], "the ninth job leaves the full host");
    for (i, chunk) in order[..48].chunks(8).enumerate() {
        assert!(chunk.iter().all(|h| *h == chunk[0]), "block {i}: {chunk:?}");
    }
    // The scheduler's own bookings are what the replay says, host by host.
    for host in HOSTS {
        assert_eq!(booked(&r, host), model[host], "{host}");
        assert_eq!(model[host], CPU, "{host} is full at 1.0x");
    }
}

/// Behaviour: one host booked at 85% and one at 30% of equal capacity (twenty cores, so
/// that jobs of a core, the least a lease is charged, make the shares): a new job goes to
/// the 30% host, since with it the 85% host would be at 90%, past the limit. Run both ways
/// round (the names swapped), so the answer is not the tie-break's (first by name or hash).
///
/// Catches: best fit with no pack limit, which takes the fuller host.
#[test]
fn a_job_goes_to_the_30_percent_host_not_the_85_percent_one() {
    for (heavy, light) in [("a", "b"), ("b", "a")] {
        let mut r = Rig::on();
        for name in ["a", "b"] {
            r.up(name, Resources::new(2 * CPU, 64 * GIB));
        }
        load(&mut r, heavy, &[light], 17, CORE);
        load(&mut r, light, &[heavy], 6, CORE);
        assert_eq!((booked(&r, heavy), booked(&r, light)), (17_000, 6_000));
        assert_eq!(place(&mut r, CORE), light, "heavy is {heavy}");
        assert_eq!((booked(&r, heavy), booked(&r, light)), (17_000, 7_000));
    }
}

/// Behaviour: two hosts under the limit, at 50% and 20%: a job that leaves the fuller one
/// within the limit (60%) still goes to it, as before: packing is kept below the limit.
/// Run both ways round.
///
/// It pins the packing within the limit, which the pack limit keeps.
#[test]
fn below_the_limit_the_fuller_host_is_still_packed() {
    for (full, light) in [("a", "b"), ("b", "a")] {
        let mut r = farm(&["a", "b"]);
        load(&mut r, full, &[light], 5, CORE);
        load(&mut r, light, &[full], 2, CORE);
        assert_eq!(place(&mut r, CORE), full, "fuller is {full}");
        assert_eq!((booked(&r, full), booked(&r, light)), (6_000, 2_000));
    }
}

/// Behaviour: the boundary of the packing: a job that leaves the fuller host at exactly 80%
/// goes to it; a job that would leave it at 90% goes to the 20% host instead. Run both ways
/// round.
///
/// Catches (the 90% half): best fit with no pack limit, where the fuller host is packed
/// whatever it is booked to.
#[test]
fn a_job_that_would_take_the_fuller_host_past_the_limit_goes_to_the_other() {
    for (full, light) in [("a", "b"), ("b", "a")] {
        let mut r = farm(&["a", "b"]);
        load(&mut r, full, &[light], 7, CORE);
        load(&mut r, light, &[full], 2, CORE);
        // 70% + 10% = 80% exactly: within.
        assert_eq!(
            place(&mut r, CORE),
            full,
            "to exactly the limit, fuller {full}"
        );
        assert_eq!(booked(&r, full), LIMIT);
        // 80% + 10% = 90%: past; the other host (20%, 30% with it) is within.
        assert_eq!(place(&mut r, CORE), light, "past the limit, fuller {full}");
        assert_eq!((booked(&r, full), booked(&r, light)), (LIMIT, 3_000));
    }
}

/// Three workers, each with leases that may run 100 s on `a` and 900 s on `b` and `c`; `a`
/// holds the most leases (two), `b` and `c` one each. A whole-machine request queued after
/// them is blocked, and holds a worker: with whole-node settings the one cheapest to drain
/// (the earliest latest end bound of its leases), here `a`, though it holds more leases than
/// the others. The leases of `a`, and the request.
fn held_farm() -> (Rig, Vec<StartLease>, OperationId) {
    let whole = caps("container").with_drivers(["container", "native-whole-machine"]);
    let mut r = Rig::on();
    let given = std::mem::replace(&mut r.s, Scheduler::new(1));
    r.s = given.with_whole_node(WholeNodeConfig::dedicated(4));
    for name in ["a", "b", "c"] {
        r.up_with(name, node(), whole.clone());
    }
    let mut a_leases = Vec::new();
    for (worker, others, count, wall) in [
        ("a", ["b", "c"], 2, 100),
        ("b", ["a", "c"], 1, 900),
        ("c", ["a", "b"], 1, 900),
    ] {
        for other in others {
            r.feed(js_sched::Event::Cordon { worker: w(other) });
        }
        let ops: Vec<OperationId> = (0..count)
            .map(|_| r.action(CORE, GIB, Some(warm(CORE, 0))))
            .collect();
        for op in &ops {
            r.s.set_max_wall(*op, Duration::from_secs(wall));
        }
        let starts = r.tick();
        assert_eq!(starts.len(), count as usize);
        assert!(starts.iter().all(|s| s.worker.as_str() == worker));
        if worker == "a" {
            a_leases = starts;
        }
        for other in others {
            r.feed(js_sched::Event::Uncordon { worker: w(other) });
        }
    }
    let mut whole_request = r.request(LeaseKind::WholeMachine, CPU, 64 * GIB);
    whole_request.resources = node();
    let head = r.submit(whole_request);
    assert!(r.round(1_000).is_empty(), "no worker is empty");
    (r, a_leases, head)
}

/// Behaviour: a queued whole-machine request still holds the worker with the least time
/// left on its leases, not the one with the fewest leases, and starts on it once its leases
/// end; the placement score does not touch the reservation.
///
/// It pins the reservation, which the pack limit leaves unchanged.
#[test]
fn a_whole_machine_request_still_holds_the_worker_with_the_least_remaining_time() {
    let (mut r, a_leases, head) = held_farm();
    assert_eq!(r.s.reservation(head), Some(&w("a")));
    for lease in &a_leases {
        r.finish(lease);
    }
    let starts = r.round(1_000);
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].operation, head);
    assert_eq!(starts[0].worker.as_str(), "a");
}

/// Behaviour: while `a` is held, small jobs are placed on `b` and `c` by the pack limit and
/// never on `a`: `b` and `c` (one core booked each) take jobs of a core one round at a time
/// until a host is at 80% (eight cores), the next job goes to the other host (still within
/// with it), and when both are at the limit the following jobs go to the least full. At
/// each job the chosen host is the one the rule names, `a` holds only its two leases, and
/// the reservation is kept.
///
/// Catches: best fit with no pack limit, which packs one of the two past 80%, to its full
/// ten cores, before the other is touched.
#[test]
fn small_jobs_beside_a_held_worker_follow_the_pack_limit_on_the_others() {
    let (mut r, _, head) = held_farm();
    let mut model: BTreeMap<&str, u64> = [("b", CORE), ("c", CORE)].into_iter().collect();
    let mut order = Vec::new();
    for n in 0..16 {
        let host = place(&mut r, CORE);
        let host = ["b", "c"]
            .into_iter()
            .find(|h| *h == host)
            .unwrap_or_else(|| panic!("job {n} went to {host}, not to b or c"));
        order.push(host);
        let within: Vec<&str> = model
            .keys()
            .copied()
            .filter(|h| model[h] + CORE <= LIMIT)
            .collect();
        if within.is_empty() {
            assert_eq!(
                model[host],
                *model.values().min().expect("hosts"),
                "job {n}"
            );
        } else {
            let fullest = within.iter().map(|h| model[h]).max().expect("a host");
            assert!(within.contains(&host), "job {n}: went past the limit");
            assert_eq!(model[host], fullest, "job {n}: packs the fullest within it");
        }
        *model.get_mut(host).expect("host") += CORE;
        assert_eq!(booked(&r, host), model[host], "job {n}");
        assert_eq!(booked(&r, "a"), 2 * CORE, "job {n}: nothing went to a");
        assert_eq!(r.s.reservation(head), Some(&w("a")), "job {n}");
    }
    // One host to 8 cores (7 jobs), the other to 8 (7 more), then one job each (9 cores).
    assert!(order[..7].iter().all(|h| *h == order[0]), "{order:?}");
    assert_ne!(order[7], order[0], "{order:?}");
    assert_eq!((model["b"], model["c"]), (9_000, 9_000));
}
