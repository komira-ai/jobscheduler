//! The reservation row of a host ([`crate::JobStore::place_reserving`]): a placement made from
//! a picture of the host that another placement has since changed is stale, and the versions
//! the rows have are what [`crate::JobStore::host_books`] lists.

use std::collections::BTreeMap;

use js_state::conformance::Backend;

use super::{host, rig, share};
use crate::{JobState, JobStoreError, Placement};

/// A placement reserves its host in the transaction that books it. From one picture of a host
/// only one placement commits, whichever writer makes it: the second, made on the version the
/// picture had, is stale and its job stays queued; made on the version the first one left, it
/// commits. A host with no row is read as none and takes the first placement as `None`; a row
/// of another host does not stand in the way. Each placement answers the version its host's row
/// now has, the one [`crate::JobStore::host_books`] lists. Catches two placements from one
/// picture that both book the host, a placement that does not move the row, and a version
/// answered that is not the row's.
pub fn a_reserving_placement_loses_to_one_made_after_its_picture<B: Backend>(backend: &B) {
    let name = "a_reserving_placement_loses_to_one_made_after_its_picture";
    let rig = rig(backend);
    let (first, second, third, fourth) = (
        rig.queue("r-first"),
        rig.queue("r-second"),
        rig.queue("r-third"),
        rig.queue("r-fourth"),
    );
    let host_a = host("host-a");
    let picture = rig.jobs.host_books().expect("reads");
    assert!(picture.is_empty(), "{name}: a host never placed on has a row");

    let (_, _, v1) = rig
        .other()
        .place_reserving(
            &first,
            host_a.clone(),
            share(),
            Placement::default(),
            picture.get(&host_a).copied(),
        )
        .expect("the first placement from the picture commits");
    let lost = rig
        .jobs
        .place_reserving(&second, host_a.clone(), share(), Placement::default(), None)
        .map(|_| ());
    assert_eq!(
        lost,
        Err(JobStoreError::Stale { id: second.clone() }),
        "{name}: a second placement from the same picture committed"
    );
    let queued = rig.jobs.get_job(&second).expect("reads").expect("a job");
    assert_eq!(
        queued.state,
        JobState::Queued,
        "{name}: the job that lost left the queue"
    );
    assert_eq!(
        rig.jobs.host_books().expect("reads"),
        BTreeMap::from([(host_a.clone(), v1)]),
        "{name}: the version answered is not the host's row"
    );

    let (_, _, v2) = rig
        .jobs
        .place_reserving(
            &second,
            host_a.clone(),
            share(),
            Placement::default(),
            Some(v1),
        )
        .expect("a placement from the picture after the first commits");
    assert!(v2 > v1, "{name}: the placement did not move the row");
    let old = rig
        .jobs
        .place_reserving(
            &third,
            host_a.clone(),
            share(),
            Placement::default(),
            Some(v1),
        )
        .map(|_| ());
    assert_eq!(
        old,
        Err(JobStoreError::Stale { id: third.clone() }),
        "{name}: a placement from the version before the last one committed"
    );

    let (_, _, elsewhere) = rig
        .jobs
        .place_reserving(
            &fourth,
            host("host-b"),
            share(),
            Placement::default(),
            None,
        )
        .expect("another host's row is not in the way");
    assert_eq!(
        rig.jobs.host_books().expect("reads"),
        BTreeMap::from([(host_a, v2), (host("host-b"), elsewhere)]),
        "{name}: the rows are not one per host at the version of the last placement"
    );
}

/// The listing of the reservation rows leaves out a key whose part after `hostbook/` is not a
/// host id (a row of another build), and lists every host once.
pub fn host_books_leaves_out_a_key_that_is_not_a_host<B: Backend>(backend: &B) {
    let name = "host_books_leaves_out_a_key_that_is_not_a_host";
    let rig = rig(backend);
    let id = rig.queue("r-listed");
    let (_, _, version) = rig
        .jobs
        .place_reserving(&id, host("host-a"), share(), Placement::default(), None)
        .expect("placed");
    rig.put_raw("hostbook/Not A Host", vec![0]);
    assert_eq!(
        rig.jobs.host_books().expect("reads"),
        BTreeMap::from([(host("host-a"), version)]),
        "{name}: the listing is not the one host"
    );
}
