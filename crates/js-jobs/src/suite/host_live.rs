//! The hello of a host whose row is live ([`crate::JobStore::hello_node_in_pool`]), judged on
//! the row in the hello's own write (design `multi-instance.md`, sections 3.3 and 5, G5): a
//! hello of another agent process than the row's is refused while the row was heard within the
//! grace by the store's clock, and the row's own process takes the host over at once.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError};

use super::pools::hello_in;
use super::{host, rig};
use crate::interfere::INJECTED;
use crate::{JobStoreError, NodeHello};

/// The grace the hellos of this module are judged with.
const GRACE: Duration = Duration::from_secs(10);

/// The cap of the pool the hellos of this module say hello to: never reached.
const CAP: usize = 100;

/// The hello of a host of `pool-a` by the agent process `process` (`None`: an agent that names
/// none).
fn hello_by(process: Option<&str>) -> NodeHello {
    NodeHello {
        agent_process: process.map(str::to_owned),
        ..hello_in("pool-a")
    }
}

/// The refusal of the host `name`'s hello as live.
fn live(name: &str) -> JobStoreError {
    JobStoreError::HostLive { host: host(name) }
}

/// A hello of a host whose row was heard within the grace is refused, writing nothing, when it
/// names another agent process than the row's, or none, and when the row names none; a hello
/// that names the row's process is written at once with the next epoch; at exactly the grace
/// after the row was heard the host is still live, and a moment later any process is admitted.
/// A failed clock or commit on the way writes nothing. Catches a hello written over a live
/// host's row (two agent processes holding one host), a re-hello of the same process refused
/// until the grace runs out, an agent that names no process matched to a row that names none,
/// an off-by-one at the grace, and a liveness judged by any clock but the store's.
pub fn a_hello_of_a_live_host_is_refused_unless_it_names_the_rows_process<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_hello_of_a_live_host_is_refused_unless_it_names_the_rows_process";
    let rig = rig(backend);
    let node = host("host-1");
    let hello = |process: Option<&str>| {
        rig.jobs
            .hello_node_in_pool(&node, &hello_by(process), CAP, GRACE)
    };

    let first = hello(Some("proc-a")).expect("a host with no row");
    assert_eq!(first.record.epoch, 1, "{name}: the first hello");
    let before = rig.snapshot().0;
    assert_eq!(
        hello(Some("proc-b")).map(drop),
        Err(live("host-1")),
        "{name}: another process on a live host"
    );
    assert_eq!(
        hello(None).map(drop),
        Err(live("host-1")),
        "{name}: a hello that names no process on a live host"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a refusal wrote");

    let again = hello(Some("proc-a")).expect("the row's own process");
    assert_eq!(
        (again.record.epoch, again.record.agent_process.as_deref()),
        (2, Some("proc-a")),
        "{name}: the row's process takes the host over at once"
    );

    let heard = again.record.heard_at;
    let now = rig.store.now().expect("the store's clock");
    rig.clock.advance(heard.saturating_add(GRACE).since(now));
    assert_eq!(
        hello(Some("proc-b")).map(drop),
        Err(live("host-1")),
        "{name}: heard the grace ago"
    );
    rig.clock.advance(Duration::from_nanos(1));
    let taken = hello(Some("proc-b")).expect("past the grace");
    assert_eq!(
        (taken.record.epoch, taken.record.agent_process.as_deref()),
        (3, Some("proc-b")),
        "{name}: another process past the grace"
    );

    let unnamed = host("host-2");
    rig.jobs
        .hello_node_in_pool(&unnamed, &hello_by(None), CAP, GRACE)
        .expect("a host with no row");
    for process in [None, Some("proc-a")] {
        assert_eq!(
            rig.jobs
                .hello_node_in_pool(&unnamed, &hello_by(process), CAP, GRACE)
                .map(drop),
            Err(live("host-2")),
            "{name}: a live row that names no process, a hello naming {process:?}"
        );
    }

    rig.clock.advance(GRACE * 2);
    let before = rig.snapshot().0;
    rig.store.fail_clock(true);
    assert!(
        hello(Some("proc-c")).is_err(),
        "{name}: a hello of a known host without the clock"
    );
    rig.store.fail_clock(false);
    rig.store.fail_next();
    assert_eq!(
        hello(Some("proc-c")).map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a failed commit of a known host's hello"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a failure wrote");
}

/// A hello whose write loses the row to another server's hello, of a host known or new, reads
/// the row again and is judged on it: refused when the other's hello was of another agent
/// process (the host is live now), written with the epoch after the other's when it was of the
/// same process. Catches a judgement made only on the row read before the write, so that two
/// servers each admit a hello of one host by two agent processes at once.
pub fn a_hello_that_loses_the_row_to_another_process_is_refused<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_hello_that_loses_the_row_to_another_process_is_refused";
    let rig = rig(backend);
    let known = host("host-known");
    rig.jobs
        .hello_node_in_pool(&known, &hello_by(Some("proc-a")), CAP, GRACE)
        .expect("a host with no row");
    rig.clock.advance(GRACE * 2);

    for (node, label) in [(known, "a known host"), (host("host-new"), "a new host")] {
        let other = rig.other();
        let theirs = node.clone();
        rig.store.before_next(move || {
            other
                .hello_node_in_pool(&theirs, &hello_by(Some("proc-b")), CAP, GRACE)
                .expect("the other server's hello lands first");
        });
        assert_eq!(
            rig.jobs
                .hello_node_in_pool(&node, &hello_by(Some("proc-c")), CAP, GRACE)
                .map(drop),
            Err(JobStoreError::HostLive { host: node.clone() }),
            "{name}: {label}, another process won the row"
        );
        let row = rig.jobs.read_node(&node).expect("reads").expect("a row");
        assert_eq!(
            row.record.agent_process.as_deref(),
            Some("proc-b"),
            "{name}: {label}, the row is the winner's"
        );
    }

    let same = host("host-same");
    let other = rig.other();
    let theirs = same.clone();
    rig.store.before_next(move || {
        other
            .hello_node_in_pool(&theirs, &hello_by(Some("proc-d")), CAP, GRACE)
            .expect("the other server's hello lands first");
    });
    let row = rig
        .jobs
        .hello_node_in_pool(&same, &hello_by(Some("proc-d")), CAP, GRACE)
        .expect("the same process takes the host over");
    assert_eq!(
        row.record.epoch, 2,
        "{name}: the same process, the epoch after the other's"
    );
}
