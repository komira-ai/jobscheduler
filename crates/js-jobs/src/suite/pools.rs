//! The admission of a host to a pool of a bounded size
//! ([`crate::JobStore::hello_node_in_pool`]): the pool's rows are counted under the pool's
//! admission row, so two servers that admit new hosts to a pool one slot from its cap admit
//! one.

use std::sync::Barrier;
use std::thread;

use js_agent_wire::Phase;
use js_state::StoreError;
use js_state::conformance::Backend;

use super::{host, rig};
use crate::codec::{FORMAT, LATER_FORMAT};
use crate::interfere::INJECTED;
use crate::{JobStoreError, NodeHello};

/// The hello of a host of the pool `pool`.
pub(super) fn hello_in(pool: &str) -> NodeHello {
    NodeHello {
        session: None,
        pool: pool.to_owned(),
        claim: None,
        shape: None,
        labels: std::collections::BTreeMap::new(),
        phase: Phase::Serving,
        agent_process: None,
    }
}

/// The refusal of the host `name`'s hello to the pool `pool`.
fn full(name: &str, pool: &str) -> JobStoreError {
    JobStoreError::PoolFull {
        host: host(name),
        pool: pool.to_owned(),
    }
}

/// A pool admits a host new to it while it holds fewer than `cap` rows and refuses it at the
/// cap, writing nothing; a host that has a row is admitted again whatever the count (its
/// epoch moves on); a cap of 0 admits no new host; the rows of another pool and the rows this
/// build cannot read are not counted; a failed commit writes nothing. Catches a cap counted
/// off by one, a count that includes another pool's rows or rows that cannot be read, a
/// re-hello of a known host refused because its pool is full, and a refusal that leaves a row
/// behind.
pub fn a_pool_admits_new_hosts_up_to_its_cap_and_a_known_host_always<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_pool_admits_new_hosts_up_to_its_cap_and_a_known_host_always";
    let rig = rig(backend);
    let hello = hello_in("pool-a");
    let first = rig
        .jobs
        .hello_node_in_pool(&host("host-1"), &hello, 2)
        .expect("below the cap");
    assert_eq!(
        (first.record.epoch, first.record.pool.as_str()),
        (1, "pool-a"),
        "{name}: the first admission"
    );
    assert_eq!(
        rig.jobs.read_node(&host("host-1")),
        Ok(Some(first)),
        "{name}: the row written is the row returned"
    );
    rig.jobs
        .hello_node_in_pool(&host("host-2"), &hello, 2)
        .expect("one below the cap");

    let before = rig.snapshot().0;
    assert_eq!(
        rig.jobs
            .hello_node_in_pool(&host("host-3"), &hello, 2)
            .map(drop),
        Err(full("host-3", "pool-a")),
        "{name}: at the cap"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");

    let again = rig
        .jobs
        .hello_node_in_pool(&host("host-1"), &hello, 2)
        .expect("a host with a row is admitted at the cap");
    assert_eq!(again.record.epoch, 2, "{name}: the known host's epoch");
    let again = rig
        .jobs
        .hello_node_in_pool(&host("host-2"), &hello, 0)
        .expect("a host with a row is admitted whatever the cap");
    assert_eq!(again.record.epoch, 2, "{name}: the known host, cap 0");

    assert_eq!(
        rig.jobs
            .hello_node_in_pool(&host("host-4"), &hello_in("pool-empty"), 0)
            .map(drop),
        Err(full("host-4", "pool-empty")),
        "{name}: a cap of 0"
    );
    rig.jobs
        .hello_node_in_pool(&host("host-5"), &hello_in("pool-b"), 1)
        .expect("another pool's rows are not counted");
    assert_eq!(
        rig.jobs
            .hello_node_in_pool(&host("host-6"), &hello_in("pool-b"), 1)
            .map(drop),
        Err(full("host-6", "pool-b")),
        "{name}: the other pool at its cap"
    );

    rig.put_raw("node/host-later", vec![LATER_FORMAT]);
    rig.put_raw("node/host-junk", vec![FORMAT]);
    rig.jobs
        .hello_node_in_pool(&host("host-7"), &hello_in("pool-c"), 1)
        .expect("rows that cannot be read are not counted");

    let before = rig.snapshot().0;
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .hello_node_in_pool(&host("host-8"), &hello_in("pool-d"), 5)
            .map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a failed commit"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a failed commit wrote");
}

/// A hello whose commit loses the pool's admission row to another server's admission counts the
/// pool again: with the other's row there and the pool at its cap it is refused, and with room
/// left it is admitted on the second round. Catches an admission that commits on a count made
/// before the other's row, and one that gives up when it loses the race instead of counting
/// again.
pub fn an_admission_that_loses_the_pool_row_counts_again<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "an_admission_that_loses_the_pool_row_counts_again";
    let rig = rig(backend);
    let hello = hello_in("pool-a");
    rig.jobs
        .hello_node_in_pool(&host("host-1"), &hello, 3)
        .expect("admitted");

    let other = rig.other();
    let theirs = hello.clone();
    rig.store.before_next(move || {
        other
            .hello_node_in_pool(&host("host-2"), &theirs, 3)
            .expect("the other server admits first");
    });
    rig.jobs
        .hello_node_in_pool(&host("host-3"), &hello, 3)
        .expect("room is left after the other's admission");
    assert_eq!(
        rig.jobs.nodes().map(|(_, rows)| rows.len()),
        Ok(3),
        "{name}: the pool's rows"
    );

    let other = rig.other();
    let theirs = hello.clone();
    rig.store.before_next(move || {
        other
            .hello_node_in_pool(&host("host-4"), &theirs, 4)
            .expect("the other server admits first");
    });
    let before = rig.jobs.nodes().expect("reads").1;
    assert_eq!(
        rig.jobs
            .hello_node_in_pool(&host("host-5"), &hello, 4)
            .map(drop),
        Err(full("host-5", "pool-a")),
        "{name}: the pool filled while the hello committed"
    );
    assert_eq!(
        rig.jobs.nodes().map(|(_, rows)| rows.len()),
        Ok(before.len() + 1),
        "{name}: only the other's row was added"
    );
}

/// Two servers that admit new hosts to a pool one slot from its cap at the same moment admit
/// exactly one, whatever order their commits land in: one hello is a row, the other is
/// refused as full, and the pool holds its cap and no more. Run over many pools, with the two
/// hellos released together. Catches an admission that counts the pool without the admission
/// row's version, so that both commit and the pool passes its cap.
pub fn two_servers_admitting_hosts_one_slot_from_the_cap_admit_exactly_one<B: Backend>(
    backend: &B,
) {
    let name = "two_servers_admitting_hosts_one_slot_from_the_cap_admit_exactly_one";
    let rig = rig(backend);
    let other = rig.other();
    for round in 0..40_u32 {
        let pool = format!("pool-{round}");
        let hello = hello_in(&pool);
        for member in 0..2 {
            rig.jobs
                .hello_node_in_pool(&host(&format!("m-{round}-{member}")), &hello, 3)
                .expect("below the cap");
        }
        let (one, two) = (host(&format!("n-{round}-1")), host(&format!("n-{round}-2")));
        let barrier = Barrier::new(2);
        let answers: Vec<Result<(), JobStoreError>> = thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                rig.jobs.hello_node_in_pool(&one, &hello, 3).map(drop)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                other.hello_node_in_pool(&two, &hello, 3).map(drop)
            });
            [first, second]
                .into_iter()
                .map(|answer| answer.join().expect("a hello"))
                .collect()
        });
        let admitted = answers.iter().filter(|answer| answer.is_ok()).count();
        let refused = answers
            .iter()
            .filter(|answer| matches!(answer, Err(JobStoreError::PoolFull { .. })))
            .count();
        assert_eq!(
            (admitted, refused),
            (1, 1),
            "{name}: round {round}: {answers:?}"
        );
        let held = rig
            .jobs
            .nodes()
            .expect("reads")
            .1
            .iter()
            .filter(|row| row.record.pool == pool)
            .count();
        assert_eq!(held, 3, "{name}: round {round}: the pool's rows");
    }
}

/// [`crate::JobStore::pool_admits`] answers what [`crate::JobStore::hello_node_in_pool`] would,
/// writing nothing: a host new to the pool while the pool holds fewer than `cap` rows, not at
/// the cap, and a host that has a row whatever the count; the rows of another pool are not
/// counted, and a read that fails is the store's error. Catches a judge that admits a new host
/// at the cap (the hello would then write a binding and be refused only at its row), one that
/// refuses a known host's re-hello, and one that writes.
pub fn pool_admits_judges_as_the_admission_does_and_writes_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "pool_admits_judges_as_the_admission_does_and_writes_nothing";
    let rig = rig(backend);
    let hello = hello_in("pool-a");
    let admits =
        |host_name: &str, cap: usize| rig.jobs.pool_admits(&host(host_name), "pool-a", cap);
    assert_eq!(admits("host-1", 1), Ok(true), "{name}: an empty pool");
    assert_eq!(admits("host-1", 0), Ok(false), "{name}: a cap of 0");
    rig.jobs
        .hello_node_in_pool(&host("host-1"), &hello, 1)
        .expect("below the cap");
    rig.jobs
        .hello_node_in_pool(&host("elsewhere-1"), &hello_in("pool-b"), 9)
        .expect("another pool");
    let before = rig.snapshot();
    assert_eq!(admits("host-2", 1), Ok(false), "{name}: at the cap");
    assert_eq!(admits("host-2", 2), Ok(true), "{name}: one below the cap");
    assert_eq!(
        admits("host-1", 1),
        Ok(true),
        "{name}: a known host at the cap"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a judgement wrote");
    rig.store.fail_read_after(0);
    assert!(admits("host-2", 2).is_err(), "{name}: a failed read");
}
