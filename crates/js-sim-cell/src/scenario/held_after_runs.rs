//! The expectation that an item was placed only after the runs on its host ended:
//! [`super::Expect::held_after_the_runs_on_its_host`], judged over the cell's observations.

use js_sim::text::list;

use crate::observe::{ClientEvent, ItemId, Observations};
use crate::text;

/// [`super::Expect::held_after_the_runs_on_its_host`]`(item)`: got is each run launched
/// on the item's host before it was placed that had not completed by then, or `not held`
/// for an item never placed. A run completed when a completion of it is in the ledger, or,
/// for a job's run (named `<job>/<allocation>`), when its agent made the job's result on
/// that host.
pub(super) fn judged(observations: &Observations, item: ItemId) -> [String; 3] {
    let record = observations.items.get(&item);
    let heard = record.into_iter().flat_map(|record| &record.heard);
    let mut placed = heard.filter_map(|heard| match &heard.event {
        ClientEvent::Placed { host } => Some((host, heard.at)),
        _ => None,
    });
    let got = match placed.next() {
        None => "not held".to_owned(),
        Some((host, at)) => {
            let ended = |run: &crate::observe::RunId| {
                let job = run.as_str().split('/').next().unwrap_or_default();
                let mut completions = observations.completions.iter();
                let mut made = observations.results_made.iter();
                completions.any(|done| (done.run == *run) & (done.at <= at))
                    || made.any(|result| {
                        (result.job.as_str() == job) & (result.host == *host) & (result.at <= at)
                    })
            };
            let before = observations.ledger.iter();
            let before = before.filter(|launch| (launch.host == *host) & (launch.at <= at));
            let open: Vec<String> = before
                .filter(|launch| !ended(&launch.run))
                .map(|launch| text::run(&launch.run))
                .collect();
            list(&open, String::clone)
        }
    };
    let name = format!("held-after-the-runs-on-its-host({})", text::item(item));
    [name, "[]".to_owned(), got]
}

#[cfg(test)]
mod tests {
    use js_types::HostId;
    use js_types::sched::FarmTime;

    use super::*;
    use crate::jobs::ResultMade;
    use crate::ledger::Completion;
    use crate::observe::RunId;
    use js_jobs::JobId;

    fn ms(n: u64) -> FarmTime {
        FarmTime::from_millis(n)
    }

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("a host id")
    }

    /// Asserts that an item placed on `h000` after the run launched there before
    /// completed meets the statement; that a run launched there that had not completed
    /// by the placement, or completed only after it, is named; that runs on other hosts and
    /// runs launched after the placement are not; and that an item never placed got `not
    /// held`. Catches a check that reads every host's runs, one that takes a completion
    /// after the placement as before it, and one that passes an item that was never held.
    #[test]
    fn a_grant_before_a_run_on_its_host_ended_is_named() {
        let placed = ClientEvent::Placed { host: host("h000") };
        let base = Observations::quiet()
            .with_event(ItemId(0), ms(100_000), placed)
            .with_launch(RunId::new("a"), host("h000"), ms(20_000))
            .with_launch(RunId::new("b"), host("h001"), ms(20_000))
            .with_launch(RunId::new("c"), host("h000"), ms(110_000));
        let name = "held-after-the-runs-on-its-host(item0)";
        let ended = |at: u64| Completion {
            run: RunId::new("a"),
            host: host("h000"),
            at: ms(at),
        };
        let mut done = base.clone();
        done.completions = vec![ended(90_000)];
        assert_eq!(judged(&done, ItemId(0)), [name, "[]", "[]"]);
        assert_eq!(judged(&base, ItemId(0)), [name, "[]", "[run \"a\"]"]);
        let mut later = base;
        later.completions = vec![ended(100_001)];
        assert_eq!(judged(&later, ItemId(0))[2], "[run \"a\"]");
        let job = |made: u64, on: &str| {
            let result = ResultMade {
                host: host(on),
                job: JobId::parse("job-a").expect("a job id"),
                at: ms(made),
            };
            let mut seen = Observations::quiet()
                .with_event(
                    ItemId(0),
                    ms(100_000),
                    ClientEvent::Placed { host: host("h000") },
                )
                .with_launch(RunId::new("job-a/alloc"), host("h000"), ms(20_000));
            seen.results_made = vec![result];
            judged(&seen, ItemId(0))[2].clone()
        };
        assert_eq!(job(90_000, "h000"), "[]");
        assert_eq!(job(100_001, "h000"), "[run \"job-a/alloc\"]");
        assert_eq!(job(90_000, "h001"), "[run \"job-a/alloc\"]");
        let never = judged(&Observations::quiet(), ItemId(3));
        assert_eq!(
            never,
            ["held-after-the-runs-on-its-host(item3)", "[]", "not held"]
        );
    }
}
