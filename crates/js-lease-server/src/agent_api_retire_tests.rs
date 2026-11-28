//! The operator's retirement of a host id ([`crate::agents::Agents::retire`]) over a server
//! that holds the job tables: a fixed pool at its cap refuses a new id until the lost old id is
//! retired, and then admits it; a live host, and one that holds a job or a cordon, is refused
//! with nothing written; one that owes only the stops of ended jobs is retired and those
//! allocations freed; a retirement whose store fails is the same call's to finish. Hellos go
//! through the route; two servers share one store and one manual clock, and nothing sleeps.

use std::time::Duration;

use js_jobs::{AllocationStep, EndReason, JobEnd, Kept, StartFailure};
use js_types::HostId;
use js_types::cordon::Asker;

use crate::agent_api::job_rig::WHOLE_GRACE;
use crate::agent_api::pool_tests::{hello, refusal, two_servers};
use crate::agents::retire::RetireFault;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Past the grace of the rigs' pool: every host not heard since is lost.
const PAST_GRACE: Duration = WHOLE_GRACE.saturating_add(Duration::from_secs(1));

/// Asserts that a pool of four full of hosts that joined one server refuses a fifth id
/// `pool-full` on either server; that the retirement of one of its hosts is refused
/// `Kept::Live` while that host is live, writing nothing; that once the host is lost its
/// retirement deletes its row and the server's record of it, after which the other server
/// admits the new id at epoch 1; and that a second retirement of the old id answers `false`
/// and writes nothing. Catches a full pool that a lost host's id fills for good (the case of a
/// host that changed its id), a retirement of a host that is still live, and a repeat that
/// fails or writes.
#[tokio::test]
async fn a_full_pool_admits_a_new_id_once_the_lost_old_id_is_retired() {
    let (mut first, second) = two_servers("retire_room");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.join(name, false).await;
    }
    for server in [&first, &second] {
        let refused = hello(server, "host-5", None).await;
        assert_eq!(refusal(refused), (403, "pool-full"));
    }
    let old = host("host-1");
    let commits = first.commits();
    let live = first.agents.retire(&old).await;
    assert_eq!(live, Err(RetireFault::Kept(Kept::Live)));
    assert_eq!(first.commits(), commits, "a refusal wrote");
    assert!(first.jobs.read_node(&old).unwrap().is_some());

    first.advance(PAST_GRACE);
    assert_eq!(first.agents.retire(&old).await, Ok(true));
    assert_eq!(first.jobs.read_node(&old).unwrap(), None, "the row");
    assert!(first.agents.registry.own_host(&old).is_none(), "the record");
    let admitted = hello(&second, "host-5", None).await.unwrap();
    assert_eq!(admitted.epoch, 1);

    let commits = first.commits();
    assert_eq!(first.agents.retire(&old).await, Ok(false), "a repeat");
    assert_eq!(first.commits(), commits, "a repeat wrote");
}

/// Asserts that, once both hosts are lost, the retirement of a host that holds a job's live
/// allocation is refused `Kept::HoldsJobs` and that of a cordoned host `Kept::Cordoned`, and
/// that neither writes anything: both rows and both records stay. Catches a retirement that
/// deletes the row under a job the host still holds (every server would lose the host from
/// its placement and the job from the pool's count), and one that drops an operator's cordon
/// with the id.
#[tokio::test]
async fn a_retire_of_a_host_holding_a_job_or_a_cordon_is_refused_and_writes_nothing() {
    let (mut first, _second) = two_servers("retire_holds");
    first.join("host-1", false).await;
    first.join("host-2", false).await;
    first.place("job-a", "host-1");
    let (busy, cordoned) = (host("host-1"), host("host-2"));
    let lease = Duration::from_secs(3600);
    first
        .agents
        .cordon(&cordoned, lease, Asker::Unnamed)
        .await
        .unwrap();
    first.advance(PAST_GRACE);

    let commits = first.commits();
    let held = first.agents.retire(&busy).await;
    assert_eq!(held, Err(RetireFault::Kept(Kept::HoldsJobs)));
    let held = first.agents.retire(&cordoned).await;
    assert_eq!(held, Err(RetireFault::Kept(Kept::Cordoned)));
    assert_eq!(first.commits(), commits, "a refusal wrote");
    for kept in [&busy, &cordoned] {
        assert!(first.jobs.read_node(kept).unwrap().is_some(), "{kept}");
        assert!(first.agents.registry.own_host(kept).is_some(), "{kept}");
    }
}

/// Asserts that a lost host whose jobs have all ended with their stops unconfirmed (one lost to
/// the host's silence, one whose start timed out), their allocations releasing, is retired, and
/// that the retirement frees both allocations as the host's absence does. Catches a dead host
/// that can never be retired, its room booked for good, and a retirement that leaves booked the
/// room of stops no host will ever confirm.
#[tokio::test]
async fn a_retire_of_a_host_owing_only_the_stops_of_ended_jobs_frees_them() {
    let (mut first, _second) = two_servers("retire_given_up");
    first.join("host-1", false).await;
    let lost = first.place("job-lost", "host-1");
    let timed_out = first.place("job-timeout", "host-1");
    first.jobs.end_job(&lost, JobEnd::HostSilent).unwrap();
    let start_timeout = JobEnd::FailedToStart(StartFailure::StartTimeout);
    first.jobs.end_job(&timed_out, start_timeout).unwrap();
    first.advance(PAST_GRACE);

    let old = host("host-1");
    assert_eq!(first.agents.retire(&old).await, Ok(true));
    assert_eq!(first.jobs.read_node(&old).unwrap(), None, "the row");
    for id in [&lost, &timed_out] {
        let freed = first.allocation(id);
        let absent = (AllocationStep::Freed, Some(EndReason::HostAbsent));
        assert_eq!((freed.step, freed.end_reason), absent, "{id}");
    }
}

/// Asserts that a retirement whose delete of the row fails is `RetireFault::Unsaved` and
/// leaves the row and the server's record of the host as they were, and that the same call
/// again retires the host. Catches a failed delete answered as done, and one that forgets the
/// record while the row still counts toward the pool.
#[tokio::test]
async fn a_retire_whose_store_fails_is_unsaved_and_the_same_call_again_retires() {
    let (mut first, _second) = two_servers("retire_fails");
    first.join("host-1", false).await;
    first.advance(PAST_GRACE);
    let old = host("host-1");
    first.store.fail_next();
    assert_eq!(first.agents.retire(&old).await, Err(RetireFault::Unsaved));
    assert!(first.jobs.read_node(&old).unwrap().is_some(), "the row");
    assert!(first.agents.registry.own_host(&old).is_some(), "the record");
    assert_eq!(first.agents.retire(&old).await, Ok(true));
    assert_eq!(first.jobs.read_node(&old).unwrap(), None);
}
