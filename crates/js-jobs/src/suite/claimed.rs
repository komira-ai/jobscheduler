//! A placement decided on the rows its own transaction has locked
//! ([`crate::JobStore::place_claimed`]): the capacity check and the booking are one
//! transaction, a job is placed by one of two writers, and the host's reservation row moves.

use js_state::conformance::Backend;

use super::{cancelled, host, rig, share};
use crate::{Booked, Capacity, EndReason, JobState, JobStoreError, Placement};

/// A host that holds two [`share`]s and no more.
fn two_shares() -> Capacity {
    Capacity {
        cpu_millis: 4000,
        memory_bytes: 2 << 30,
        disk_bytes: 2 << 32,
        accelerators: 2,
    }
}

/// Placements on one host from two writers, each decided on what the host holds when it
/// commits: two shares fit and book (the second beside one the other writer made), the third is
/// `HostFull` and its job stays queued with nothing written, another host and a placement given
/// no capacity are not held to this one, and a free gives the room back. Catches a placement that
/// books past the capacity on a stale picture, a refusal that still moves the job, and room that
/// a free does not give back.
pub fn a_claimed_placement_books_only_what_the_host_has_room_for<B: Backend>(backend: &B) {
    let name = "a_claimed_placement_books_only_what_the_host_has_room_for";
    let rig = rig(backend);
    let capacity = two_shares();
    let a = host("host-a");
    let first = rig.queue("c-first");
    let second = rig.queue("c-second");
    let third = rig.queue("c-third");
    let plain = Placement::default();

    rig.other()
        .place_claimed(&first, &a, share(), &plain, Some(&capacity))
        .expect("the first share fits");
    rig.jobs
        .place_claimed(&second, &a, share(), &plain, Some(&capacity))
        .expect("the second share fits beside the first, which another writer made");
    let before = rig.snapshot().0;
    let refused = rig
        .other()
        .place_claimed(&third, &a, share(), &plain, Some(&capacity))
        .map(drop);
    assert_eq!(
        refused,
        Err(JobStoreError::HostFull {
            id: third.clone(),
            host: a.clone()
        }),
        "{name}: a third share was booked on a host with room for two"
    );
    assert_eq!(
        rig.snapshot().0,
        before,
        "{name}: a refused placement wrote"
    );
    let queued = rig.jobs.get_job(&third).expect("reads").expect("a job");
    assert_eq!(
        queued.state,
        JobState::Queued,
        "{name}: the job that was refused left the queue"
    );
    let booked = rig.jobs.booked_on(&a).expect("sums");
    assert_eq!(
        (booked.allocations, booked.cpu_millis),
        (2, 4000),
        "{name}: the host books more than its two shares"
    );

    rig.jobs
        .place_claimed(&third, &host("host-b"), share(), &plain, Some(&capacity))
        .expect("another host has room");
    let fourth = rig.queue("c-fourth");
    rig.jobs
        .place_claimed(&fourth, &a, share(), &plain, None)
        .expect("a placement given no capacity is not held to one");

    let fifth = rig.queue("c-fifth");
    rig.jobs
        .end_and_free(&first, cancelled(), EndReason::Cancelled)
        .expect("the first job's allocation is freed");
    let still = rig
        .jobs
        .place_claimed(&fifth, &a, share(), &plain, Some(&capacity))
        .map(drop);
    assert_eq!(
        still,
        Err(JobStoreError::HostFull {
            id: fifth.clone(),
            host: a.clone()
        }),
        "{name}: the room of a freed share was not counted (the fourth, made with no capacity, holds it)"
    );
    rig.jobs
        .end_and_free(&fourth, cancelled(), EndReason::Cancelled)
        .expect("the fourth job's allocation is freed");
    rig.jobs
        .place_claimed(&fifth, &a, share(), &plain, Some(&capacity))
        .expect("the room two frees gave back is booked");
}

/// A whole host books only a host nothing is live on, and a share only a host no allocation
/// books whole. Catches a whole-host placement beside a share, and a share beside a whole host.
pub fn a_whole_host_and_a_share_exclude_each_other_in_the_placement<B: Backend>(backend: &B) {
    let name = "a_whole_host_and_a_share_exclude_each_other_in_the_placement";
    let rig = rig(backend);
    let capacity = two_shares();
    let a = host("host-a");
    let plain = Placement::default();
    let (shared, whole, other) = (
        rig.queue("w-shared"),
        rig.queue("w-whole"),
        rig.queue("w-other"),
    );

    rig.jobs
        .place_claimed(&shared, &a, share(), &plain, Some(&capacity))
        .expect("a share fits an empty host");
    let beside = rig
        .jobs
        .place_claimed(&whole, &a, Booked::WholeHost, &plain, Some(&capacity))
        .map(drop);
    assert_eq!(
        beside,
        Err(JobStoreError::HostFull {
            id: whole.clone(),
            host: a.clone()
        }),
        "{name}: a whole host was booked beside a share"
    );
    rig.jobs
        .end_and_free(&shared, cancelled(), EndReason::Cancelled)
        .expect("freed");
    rig.jobs
        .place_claimed(&whole, &a, Booked::WholeHost, &plain, Some(&capacity))
        .expect("a whole host books an empty host");
    let under = rig
        .jobs
        .place_claimed(&other, &a, share(), &plain, Some(&capacity))
        .map(drop);
    assert_eq!(
        under,
        Err(JobStoreError::HostFull {
            id: other.clone(),
            host: a
        }),
        "{name}: a share was booked on a host a job books whole"
    );
}

/// A job another writer placed between this writer's read and its placement is not placed
/// twice: the decision reads the job as the claim finds it, so the second writer is told the
/// job is no longer queued, writes nothing, and the job stays where the first put it. Catches a
/// job placed on two hosts and a second placement that overwrites the first.
pub fn a_job_another_writer_placed_is_not_placed_again<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_job_another_writer_placed_is_not_placed_again";
    let rig = rig(backend);
    let id = rig.queue("r-once");
    let plain = Placement::default();
    let other = rig.other();
    let (placing, on) = (id.clone(), host("host-a"));
    rig.store.before_next(move || {
        other
            .place_claimed(&placing, &on, share(), &Placement::default(), None)
            .expect("the other writer places the job first");
    });
    let lost = rig
        .jobs
        .place_claimed(&id, &host("host-b"), share(), &plain, None)
        .map(drop);
    assert_eq!(
        lost,
        Err(JobStoreError::UnexpectedState {
            id: id.clone(),
            expected: JobState::Queued,
            found: JobState::Placed
        }),
        "{name}: a job placed by one writer was placed again by another"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("a job");
    assert_eq!(
        job.host,
        Some(host("host-a")),
        "{name}: the first placement did not stand"
    );
    assert_eq!(
        rig.jobs
            .booked_on(&host("host-b"))
            .expect("sums")
            .allocations,
        0,
        "{name}: the host the loser named holds the job"
    );
}

/// A claimed placement writes the host's reservation row (`hostbook/<host>`) in its own
/// transaction, with the allocation's id, and every placement on the host moves it; a placement
/// on another host does not touch it. Catches a placement that leaves the row alone, which a
/// reader working from a picture of the host would not see move.
pub fn a_claimed_placement_moves_the_hosts_reservation_row<B: Backend>(backend: &B) {
    let name = "a_claimed_placement_moves_the_hosts_reservation_row";
    let rig = rig(backend);
    let plain = Placement::default();
    let (a, b) = (host("host-a"), host("host-b"));
    let first = rig.queue("b-first");
    let second = rig.queue("b-second");
    let third = rig.queue("b-third");
    assert!(
        rig.raw("hostbook/host-a").is_none(),
        "{name}: a row before any placement"
    );

    let (_, one) = rig
        .jobs
        .place_claimed(&first, &a, share(), &plain, None)
        .expect("placed");
    let row = rig
        .raw("hostbook/host-a")
        .expect("the first placement writes the row");
    assert_eq!(
        row.value,
        one.allocation_id.to_string().into_bytes(),
        "{name}: the row does not name the allocation the placement made"
    );
    let (_, two) = rig
        .other()
        .place_claimed(&second, &a, share(), &plain, None)
        .expect("placed");
    let moved = rig.raw("hostbook/host-a").expect("the row stays");
    assert!(
        moved.version > row.version,
        "{name}: a second placement did not move the row"
    );
    assert_eq!(moved.value, two.allocation_id.to_string().into_bytes());
    rig.jobs
        .place_claimed(&third, &b, share(), &plain, None)
        .expect("placed");
    assert_eq!(
        rig.raw("hostbook/host-a").expect("the row stays").version,
        moved.version,
        "{name}: a placement on another host moved this host's row"
    );
}

/// A job that does not exist is not claimed: the placement answers `Stale` and writes nothing.
/// Catches a placement of a missing job that creates rows.
pub fn a_claimed_placement_of_a_missing_job_is_stale<B: Backend>(backend: &B) {
    let name = "a_claimed_placement_of_a_missing_job_is_stale";
    let rig = rig(backend);
    let ghost = super::job_id("ghost");
    let before = rig.snapshot().0;
    let missing = rig
        .jobs
        .place_claimed(
            &ghost,
            &host("host-a"),
            share(),
            &Placement::default(),
            None,
        )
        .map(drop);
    assert_eq!(
        missing,
        Err(JobStoreError::Stale { id: ghost }),
        "{name}: a job that does not exist was claimed"
    );
    assert_eq!(
        rig.snapshot().0,
        before,
        "{name}: a missing job's placement wrote"
    );
}
