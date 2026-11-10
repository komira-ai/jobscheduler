//! The machine provider of each open host in a pass over the job tables
//! ([`OpenHost::rank`]): a queued job goes to a free host of the first provider in order, and
//! a job that needs the whole host to a quiet host of the first provider. The job tables on
//! the in-memory fake, the placer on a clock the test moves by hand.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::JobId;
use js_registry::Placer;
use js_sched::ProviderRank;
use js_types::{HostId, ProviderId};
use js_types::{ManualClock, Timestamp};

use super::{JobHosts, NoStarts, OpenHost, PassMetrics};
use crate::engine::Settings;
use crate::engine::rig::{Tables, at, every, host, hosts, settings};
use crate::place::SchedPlacer;

/// Metrics that keep nothing.
struct Unmetered;

impl PassMetrics for Unmetered {
    fn job_placed(&self, _: Duration) {}

    fn store_write_failed(&self) {}
}

/// A placer over the quiet hosts `a` and `b` and job tables, both at 1 s.
fn quiet() -> (SchedPlacer, Tables) {
    let clock = ManualClock::new(Timestamp::from_millis(1_000));
    let placer = SchedPlacer::new(Settings::default(), clock);
    for candidate in every(&[("a", &[]), ("b", &[])]) {
        placer
            .engine()
            .host_up(&candidate.host, &candidate.shape, at(0));
    }
    let tables = Tables::new();
    tables.set_ms(1_000);
    (placer, tables)
}

/// A pass with the pool's settings after both hosts said hello, each host of the provider
/// `ranks` gives it: the host the job `job` is on after it.
fn pass(
    (placer, tables): &(SchedPlacer, Tables),
    ranks: [(&str, &str, u32); 2],
    job: &JobId,
) -> Option<HostId> {
    for name in ["a", "b"] {
        placer.heard(&host(name), None);
    }
    let open: BTreeMap<HostId, OpenHost> = ranks
        .iter()
        .map(|(name, provider, position)| {
            let rank = ProviderRank::new(ProviderId::new(provider).unwrap(), *position);
            let offer = OpenHost {
                rank: Some(rank),
                ..OpenHost::default()
            };
            (host(name), offer)
        })
        .collect();
    let hello = hosts(&["a", "b"]);
    let whole = settings();
    let on = JobHosts {
        open: &open,
        hello: &hello,
        whole: Some(&whole),
    };
    let placed = placer.place_jobs_with(&tables.jobs, &on, (&NoStarts, &Unmetered));
    assert_eq!(placed.unwrap(), std::slice::from_ref(job));
    tables.jobs.get_job(job).unwrap().unwrap().host
}

/// Catches: the provider of an open host not given to the scheduler, so a pass places on
/// the host its score prefers whatever the order. Of two quiet hosts, the job goes to the
/// one of the provider listed first, in either order.
#[test]
fn a_queued_job_goes_to_the_open_host_of_the_first_provider() {
    for (a_at, b_at, first) in [(1, 0, "b"), (0, 1, "a")] {
        let rig = quiet();
        let job = rig.1.share("j").id;
        let on = pass(&rig, [("a", "cloud", a_at), ("b", "static", b_at)], &job);
        assert_eq!(on, Some(host(first)));
    }
}

/// Catches: the providers given to the scheduler only for the walk, after the whole-host
/// jobs are decided, so a quiet host is taken by its name alone. Of two quiet hosts, the
/// whole-host job is committed on the one of the provider listed first, in either order.
#[test]
fn a_whole_host_job_takes_the_quiet_host_of_the_first_provider() {
    for (a_at, b_at, first) in [(1, 0, "b"), (0, 1, "a")] {
        let rig = quiet();
        let job = rig.1.whole("j").id;
        let on = pass(&rig, [("a", "cloud", a_at), ("b", "static", b_at)], &job);
        assert_eq!(on, Some(host(first)));
    }
}
