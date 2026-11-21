//! The bars of a queued job that books a share of a host ([`js_jobs::JobRecord::prepare_bars`],
//! which a start a held host refused writes): the walk keeps the job off a host barred until
//! after the walk's time or for life, places it there once the bar has ended, and leaves a
//! back-off, which only a failed whole-host prepare sets, to the whole-host path.

use std::collections::BTreeMap;

use js_jobs::{Exclusion, JobRecord};
use js_types::HostId;

use super::OpenHost;
use crate::engine::rig::{Tables, at, engine, host, store};

/// Where a walk at `now_ms` over the open hosts `open` places a share job that `exclusion`
/// keeps off `a`: the host, or `None` with whether some open host could run it.
fn placed_with(exclusion: Exclusion, open: &[&str], now_ms: u64) -> Result<HostId, bool> {
    let tables = Tables::new();
    let mut job: JobRecord = tables.share("barred");
    job.prepare_bars.insert(host("a"), exclusion);
    let mut e = engine(&[("a", &[]), ("b", &[])]);
    let open: BTreeMap<HostId, OpenHost> = open
        .iter()
        .map(|name| (host(name), OpenHost::default()))
        .collect();
    let (decided, waiting) = e.walk_jobs(&[job], &open, at(now_ms));
    match (decided.first(), waiting.first()) {
        (Some(placed), None) => Ok(placed.1.clone()),
        (None, Some((_, capable))) => Err(*capable),
        other => panic!("one job walked, one answer: {other:?}"),
    }
}

/// Catches a bar the walk does not read, so a job a held host refused goes straight back to
/// it, and a bar read as closing every host. A job barred from `a` until 5 s is placed on `b`
/// at 1 s; with `a` the only open host it waits as a job no open host can run, and it is not
/// placed there.
#[test]
fn a_job_barred_from_a_host_is_placed_on_another() {
    let until = Exclusion::Until(store(5_000));
    assert_eq!(placed_with(until, &["a", "b"], 1_000), Ok(host("b")));
    assert_eq!(placed_with(until, &["a"], 1_000), Err(false));
    assert_eq!(
        placed_with(Exclusion::ForLife, &["a"], 1_000_000),
        Err(false)
    );
}

/// Catches a bar that outlives its end, which would keep a job off a host that is back in
/// service, and a back-off that holds a job of a share off its host. A bar ending at 5 s no
/// longer holds at 5 s (the end is not part of it); a back-off holds at no time.
#[test]
fn a_job_is_placed_on_a_host_once_its_bar_has_ended_and_a_back_off_never_bars_it() {
    let until = Exclusion::Until(store(5_000));
    assert_eq!(placed_with(until, &["a"], 4_999), Err(false));
    assert_eq!(placed_with(until, &["a"], 5_000), Ok(host("a")));
    let backoff = Exclusion::Backoff(store(5_000));
    assert_eq!(placed_with(backoff, &["a"], 1_000), Ok(host("a")));
}
