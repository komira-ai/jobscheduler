//! The per-job half of a heartbeat when the store does not know what a host runs: a server on
//! an empty table, one behind another, an orphan past its bound or ended by an operator, and
//! the guard holding a record's stop. Each beat is served on the job tables alone, the host's
//! row given as a beat reads it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{Command, HeldJob, HeldState, NetworkCeiling, Outcome, StopReason};
use js_jobs::{JobId, Orphan, Retention};
use js_state::{Clock as _, ManualClock as StoreClock};
use js_types::HostId;

use super::{BeatContext, JobReply, RowState, serve, unknown_of};
use crate::agent_api::job_rig::{BOOKED, placed, result, spec, tables_on};
use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;
use crate::metrics::NoMetrics;

const HOUR: Duration = Duration::from_secs(3600);

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

fn running(job_id: &str) -> HeldJob {
    HeldJob {
        job_id: job_id.to_owned(),
        state: HeldState::Running,
        result: None,
    }
}

/// The jobs `reply` tells the host to stop, each with its reason.
fn stops_of(reply: &JobReply) -> Vec<(String, Option<StopReason>)> {
    let each = reply.commands.iter().filter_map(|command| match command {
        Command::JobStop { job_id, reason } => Some((job_id.clone(), *reason)),
        _ => None,
    });
    each.collect()
}

/// One beat of `host-a` listing `listing`, its row holding `orphans`, orphans ended after
/// `after`.
fn beat(
    jobs: &ServerJobs,
    listing: &[HeldJob],
    orphans: &BTreeMap<String, Orphan>,
    after: Duration,
) -> JobReply {
    let ceiling = NetworkCeiling::default();
    let context = BeatContext {
        host: &host(),
        ceiling: &ceiling,
        host_jobs: false,
        shape: None,
        logs: None,
        polls_dispatch: false,
        metrics: &NoMetrics,
        layered_rootfs: false,
        mount_providers: &[],
        xcodes: &[],
        limits: JobLimits {
            orphan_after: after,
            ..JobLimits::DEFAULT
        },
    };
    let row = RowState {
        missed: None,
        orphans: Some(orphans),
    };
    serve(jobs, &context, (listing, &[]), 8, row).unwrap()
}

/// Places `id` on `host-a` and has it run there.
fn running_on_host(jobs: &ServerJobs, id: &str) -> JobId {
    let (job, _) = placed(jobs, id, "host-a", spec(), BOOKED);
    beat(jobs, &[running(id)], &BTreeMap::new(), HOUR);
    job.id
}

fn tables() -> (Arc<StoreClock>, ServerJobs) {
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_secs(1000));
    let (_, jobs) = tables_on(clock.clone(), Retention::DEFAULT);
    (clock, jobs)
}

/// Asserts that a server that came up on an empty job table (a restore, a wrong database, a
/// failed migration) stops none of the jobs a host runs: the reply names them all in
/// `unknown_jobs`, the host's row keeps each as an orphan first listed now, and the listing
/// is not clean, so the host takes no job; the beat after keeps their age. It catches the
/// server that told every host of a farm to stop every job within one heartbeat.
#[test]
fn a_server_on_an_empty_table_stops_none_of_the_jobs_a_host_runs() {
    let (clock, jobs) = tables();
    let listing = [running("job-1"), running("job-2"), running("job-3")];
    let now = clock.now();
    let reply = beat(&jobs, &listing, &BTreeMap::new(), HOUR);
    assert!(reply.commands.is_empty());
    assert_eq!(reply.unknown_jobs, ["job-1", "job-2", "job-3"]);
    assert!(!reply.clean & reply.judged.guarded);
    assert_eq!(reply.orphans.len(), 3);
    let first = Orphan {
        first_seen: now,
        end_asked: false,
    };
    assert!(reply.orphans.values().all(|orphan| *orphan == first));
    clock.advance(Duration::from_secs(30));
    let again = beat(&jobs, &listing, &reply.orphans, HOUR);
    assert!(again.commands.is_empty());
    assert_eq!(again.orphans, reply.orphans);
    for id in ["job-1", "job-2", "job-3"] {
        assert_eq!(jobs.get_job(&JobId::parse(id).unwrap()).unwrap(), None);
    }
}

/// Asserts that a job its client cancelled is told to stop on the first beat after the
/// cancel, with the reason `cancelled`, beside an orphan of the host (the store records the
/// cancel, and the listing is mostly known). It catches a guard that holds the stop of a job
/// the store records a cancel of on a host whose work it knows.
#[test]
fn a_cancelled_job_is_told_to_stop_within_one_beat_beside_an_orphan() {
    let (_, jobs) = tables();
    let job = running_on_host(&jobs, "job-c");
    running_on_host(&jobs, "job-k");
    jobs.request_cancel(&job).unwrap();
    let listing = [running("job-c"), running("job-k"), running("ghost")];
    let reply = beat(&jobs, &listing, &BTreeMap::new(), HOUR);
    let told = stops_of(&reply);
    assert_eq!(told, [("job-c".to_owned(), Some(StopReason::Cancelled))]);
    assert_eq!(reply.unknown_jobs, ["ghost"]);
}

/// Asserts what two servers over two copies of the store, one behind the other (a replica
/// that lags, a backup taken before the last placements), tell one host that lists the same
/// work: the server behind, which holds neither the cancel of `job-a` nor the placement of
/// `job-b` on this host (it holds `job-b` on another host), stops nothing; the server that is
/// current stops `job-a` for its cancel and nothing else. It catches a server behind that
/// stops a job placed after its copy was taken (a placement elsewhere is no fence), and a
/// current one that does not stop a cancelled job.
#[test]
fn of_two_servers_the_one_behind_stops_nothing_and_the_current_one_stops_the_cancel() {
    let (_, behind) = tables();
    let (_, current) = tables();
    for jobs in [&behind, &current] {
        running_on_host(jobs, "job-a");
    }
    placed(&behind, "job-b", "host-b", spec(), BOOKED);
    running_on_host(&current, "job-b");
    let cancelled = JobId::parse("job-a").unwrap();
    current.request_cancel(&cancelled).unwrap();

    let listing = [running("job-a"), running("job-b")];
    assert_eq!(
        unknown_of(&behind, &host(), &listing).unwrap(),
        BTreeMap::from([("job-b".to_owned(), false)])
    );
    let late = beat(&behind, &listing, &BTreeMap::new(), HOUR);
    assert!(late.commands.is_empty(), "{:?}", late.commands);
    assert_eq!(late.unknown_jobs, ["job-b"]);
    let now = beat(&current, &listing, &late.orphans, HOUR);
    let told = stops_of(&now);
    assert_eq!(told, [("job-a".to_owned(), Some(StopReason::Cancelled))]);
    assert!(now.unknown_jobs.is_empty() & now.orphans.is_empty());
}

/// Asserts the bound end to end over the host's row: two orphans beside two known jobs are
/// kept until they have been listed for the bound, then the one listed first is told to stop
/// (`orphan_expired`) on every beat until it is no longer listed, and only then the other.
/// It catches an orphan ended before the bound, two ended in one beat, and an age that starts
/// again at each beat.
#[test]
fn an_orphan_past_the_bound_is_ended_one_at_a_time() {
    let (clock, jobs) = tables();
    running_on_host(&jobs, "job-1");
    running_on_host(&jobs, "job-2");
    let mut listing = vec![running("job-1"), running("job-2"), running("orphan-a")];
    let first = beat(&jobs, &listing, &BTreeMap::new(), HOUR);
    clock.advance(Duration::from_secs(60));
    listing.push(running("orphan-b"));
    let second = beat(&jobs, &listing, &first.orphans, HOUR);
    assert!(second.commands.is_empty());
    clock.advance(HOUR - Duration::from_secs(61));
    let early = beat(&jobs, &listing, &second.orphans, HOUR);
    assert!(early.commands.is_empty());
    clock.advance(Duration::from_secs(1));
    let due = beat(&jobs, &listing, &early.orphans, HOUR);
    let expired = Some(StopReason::OrphanExpired);
    let told = stops_of(&due);
    assert_eq!(told, [("orphan-a".to_owned(), expired)]);
    clock.advance(HOUR);
    let still = beat(&jobs, &listing, &due.orphans, HOUR);
    let told = stops_of(&still);
    assert_eq!(told, [("orphan-a".to_owned(), expired)]);
    listing.retain(|job| job.job_id != "orphan-a");
    let next = beat(&jobs, &listing, &still.orphans, HOUR);
    let told = stops_of(&next);
    assert_eq!(told, [("orphan-b".to_owned(), expired)]);
}

/// Asserts the guard against what the store records: on a host whose listing is mostly
/// orphans, a job the store records a cancel of is not told to stop, while an orphan an
/// operator ended is (`operator`); once the orphans have ended on the host (their results
/// listed, answered stale), the cancel's stop goes out. It catches a guard that a record gets
/// past (a store rolled back to before a placement can hold a stale cancel), and one that
/// holds an operator's end.
#[test]
fn the_guard_holds_a_cancel_and_lets_an_operators_end_through() {
    let (_, jobs) = tables();
    let job = running_on_host(&jobs, "job-c");
    jobs.request_cancel(&job).unwrap();
    let listing = [running("job-c"), running("orphan-a"), running("orphan-b")];
    let first = beat(&jobs, &listing, &BTreeMap::new(), HOUR);
    assert!(first.commands.is_empty() & first.judged.guarded);
    let mut asked = first.orphans.clone();
    asked.get_mut("orphan-b").unwrap().end_asked = true;
    let ended = beat(&jobs, &listing, &asked, HOUR);
    let told = stops_of(&ended);
    assert_eq!(told, [("orphan-b".to_owned(), Some(StopReason::Operator))]);
    assert!(ended.orphans["orphan-b"].end_asked);

    let done = |id: &str| HeldJob {
        state: HeldState::ExitedUnacked,
        result: Some(result(id, "00", Outcome::Killed, None)),
        ..running(id)
    };
    let gone = [running("job-c"), done("orphan-a"), done("orphan-b")];
    let after = beat(&jobs, &gone, &ended.orphans, HOUR);
    let told = stops_of(&after);
    assert_eq!(told, [("job-c".to_owned(), Some(StopReason::Cancelled))]);
    assert!(after.orphans.is_empty() & !after.judged.guarded);
    assert_eq!(after.unknown_jobs, ["orphan-a", "orphan-b"]);
}

/// Asserts that a job its client cancelled is told to stop on a host the store holds no
/// allocation of it on (the store placed it on another host, or never placed it), with the
/// reason `cancelled`, and is no orphan: a cancel is a record that holds whatever the host.
/// It catches a cancelled job kept running as an orphan by the host that runs it.
#[test]
fn a_cancelled_job_is_stopped_on_a_host_the_store_did_not_place_it_on() {
    let (_, jobs) = tables();
    running_on_host(&jobs, "job-k");
    running_on_host(&jobs, "job-l");
    let (elsewhere, _) = placed(&jobs, "job-c", "host-b", spec(), BOOKED);
    jobs.request_cancel(&elsewhere.id).unwrap();
    let listing = [running("job-k"), running("job-l"), running("job-c")];
    let reply = beat(&jobs, &listing, &BTreeMap::new(), HOUR);
    let told = stops_of(&reply);
    assert_eq!(told, [("job-c".to_owned(), Some(StopReason::Cancelled))]);
    assert!(reply.unknown_jobs.is_empty() & reply.orphans.is_empty() & !reply.clean);
}
