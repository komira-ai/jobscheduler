//! The unclean hosts as the node rows say them ([`crate::JobStore::unclean_nodes`]): kept between
//! calls, over a store that tells its head (from the feed) and over one that tells none (from the
//! job store's own writes).

use std::collections::BTreeSet;

use js_agent_wire::Phase;
use js_state::conformance::Backend;

use super::{host, rig};
use crate::{NodeHello, NodeState, SessionDigest};

/// What a hello announces in these checks.
fn hello() -> NodeHello {
    NodeHello {
        session: Some(SessionDigest::new([5; 32])),
        pool: "pool-u".to_owned(),
        claim: None,
        shape: None,
        labels: std::collections::BTreeMap::new(),
        phase: Phase::Serving,
        agent_process: None,
    }
}

/// A host's row says its last listing was unclean, then clean again, then the row is forgotten,
/// and a row written before the first call is read by it: each call answers what the rows say
/// then. The writes go through the job store that reads, on every backend: one with a head
/// brings its set up to date from the feed, one without from the job store's own writes. Catches
/// a set that is read once and kept (a host that listed a job it holds no allocation for would
/// take jobs again, or a clean one would never take one), and one that misses a write made before
/// it was first read.
pub fn the_unclean_hosts_follow_the_rows_between_calls<B: Backend>(backend: &B) {
    let name = "the_unclean_hosts_follow_the_rows_between_calls";
    let rig = rig(backend);
    let (early, late) = (host("host-early"), host("host-late"));
    let unclean = |listed_clean: bool, host: &js_types::HostId| {
        let row = rig.jobs.read_node(host).expect("reads").expect("a row");
        let state = NodeState {
            last_listing_clean: listed_clean,
            ..row.record.state.clone()
        };
        rig.jobs
            .settle_node(&row, state)
            .expect("settles")
            .expect("at the version");
    };
    rig.jobs.hello_node(&early, &hello()).expect("a hello");
    unclean(false, &early);
    let only =
        |hosts: &[&js_types::HostId]| hosts.iter().map(|h| (*h).clone()).collect::<BTreeSet<_>>();
    assert_eq!(
        rig.jobs.unclean_nodes(),
        Ok(only(&[&early])),
        "{name}: the first call"
    );
    assert_eq!(
        rig.jobs.unclean_nodes(),
        Ok(only(&[&early])),
        "{name}: nothing written"
    );

    rig.jobs.hello_node(&late, &hello()).expect("a hello");
    unclean(false, &late);
    assert_eq!(
        rig.jobs.unclean_nodes(),
        Ok(only(&[&early, &late])),
        "{name}: a new unclean"
    );
    unclean(true, &early);
    assert_eq!(
        rig.jobs.unclean_nodes(),
        Ok(only(&[&late])),
        "{name}: clean again"
    );
    let row = rig.jobs.read_node(&late).expect("reads").expect("a row");
    assert_eq!(rig.jobs.forget_node(&late, row.version), Ok(true));
    assert_eq!(
        rig.jobs.unclean_nodes(),
        Ok(BTreeSet::new()),
        "{name}: forgotten"
    );
}
