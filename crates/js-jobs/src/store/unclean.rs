//! The hosts whose last job listing was not clean, as their node rows say
//! ([`JobStore::unclean_nodes`]), kept between calls: a placement pass asks on every heartbeat,
//! and a scan of every host's row each time would cost the pass a read and a decode per host per
//! beat. The set is read whole once, and then brought up to date: over a store that tells its
//! head, from the store's feed (only the node rows written since the last call, by any server, are
//! read again); over one that tells none, which only this process writes, from this job store's
//! own writes of node rows ([`JobStore::note_node`]).

use std::collections::BTreeSet;
use std::sync::PoisonError;

use js_state::{Revision, Store};
use js_types::HostId;

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;

/// How many rows of the feed one call reads past the head it last saw: more than that since the
/// last call reads every row again.
const FEED_PAGE: usize = 512;

/// The unclean hosts as of a head of the store.
#[derive(Debug, Default)]
pub(super) struct Unclean {
    /// The store's head when the set was last brought up to date; `None` before the first call
    /// and for a store that tells no head.
    seen: Option<Revision>,
    /// Whether the set was read whole once: from then on this job store's own writes of node
    /// rows keep it up to date ([`JobStore::note_node`]).
    ready: bool,
    /// The hosts whose row said their last listing was not clean.
    hosts: BTreeSet<HostId>,
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Every host whose node row says its last job listing was not clean, in host order: such a
    /// host takes no job until a listing of it is clean. A row this build cannot read counts as
    /// no row. The first call reads every node row. Over a store that tells its head, a later
    /// call reads only the node rows the store's feed names since the call before, and reads every
    /// row again when the feed cannot tell (more rows than a page since, or a feed it cannot
    /// read). Over a store that tells none, which only this process writes, a later call reads
    /// nothing: this job store's writes of node rows kept the set ([`JobStore::note_node`]).
    ///
    /// # Errors
    /// [`JobStoreError::Store`]: the rows or the store's head could not be read.
    pub fn unclean_nodes(&self) -> Result<BTreeSet<HostId>, JobStoreError> {
        let head = self.store.head()?;
        let mut unclean = self.unclean.lock().unwrap_or_else(PoisonError::into_inner);
        if head.is_none() && unclean.ready {
            return Ok(unclean.hosts.clone());
        }
        let since = unclean.seen.zip(head);
        let changed = since.and_then(|(seen, head)| self.node_keys_since(seen, head));
        match changed {
            Some(hosts) => {
                for host in hosts {
                    // A row this build cannot read counts as no row, as in a scan of every row.
                    let row = self.read_node(&host).or_else(|error| match error {
                        JobStoreError::Store(_) => Err(error),
                        _ => Ok(None),
                    })?;
                    let clean = row.is_none_or(|row| row.record.state.last_listing_clean);
                    match clean {
                        true => unclean.hosts.remove(&host),
                        false => unclean.hosts.insert(host),
                    };
                }
            }
            None => unclean.hosts = self.every_unclean()?,
        }
        unclean.seen = head;
        unclean.ready = true;
        Ok(unclean.hosts.clone())
    }

    /// Notes that `host`'s node row now says its last listing was `clean` (`None`: the row is
    /// gone), once the set was read whole: what keeps it up to date over a store that tells no
    /// head, and changes nothing over one that does, which reads the row again from the feed.
    pub(super) fn note_node(&self, host: &HostId, clean: Option<bool>) {
        let mut unclean = self.unclean.lock().unwrap_or_else(PoisonError::into_inner);
        if !unclean.ready {
            return;
        }
        match clean {
            Some(false) => unclean.hosts.insert(host.clone()),
            _ => unclean.hosts.remove(host),
        };
    }

    /// The hosts of the node rows the feed names after `seen`, up to `head`; `None` when the feed
    /// cannot tell all of them (more rows than a page, or a feed that cannot be read).
    fn node_keys_since(&self, seen: Revision, head: Revision) -> Option<BTreeSet<HostId>> {
        let page = self.store.changes(seen, FEED_PAGE).ok()?;
        let whole = page.cursor >= head;
        let keys = page
            .rows
            .iter()
            .filter_map(|row| row.key.strip_prefix(keys::NODE));
        let hosts = keys.filter_map(|host| HostId::new(host).ok()).collect();
        whole.then_some(hosts)
    }

    /// Every host whose node row says its last listing was not clean, from a scan of every row.
    fn every_unclean(&self) -> Result<BTreeSet<HostId>, JobStoreError> {
        let rows = self.live_nodes(std::time::Duration::MAX)?;
        let unclean = rows.into_iter().map(|row| row.record);
        let unclean = unclean.filter(|record| !record.state.last_listing_clean);
        Ok(unclean.map(|record| record.host).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Store, Txn};
    use js_types::HostId;

    use super::FEED_PAGE;
    use crate::error::JobStoreError;
    use crate::ids::RandomIds;
    use crate::interfere::Interfere;
    use crate::{JobStore, NodeHello, NodeState, keys};

    /// A job store over a fake store that tells its head, and the store.
    fn tables() -> (Arc<Interfere<Fake>>, JobStore<Interfere<Fake>, RandomIds>) {
        let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        let store = Arc::new(Interfere::new(fake));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        (store, jobs)
    }

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("an id")
    }

    /// Writes `host`'s row through another job store over `store` (another server), saying its
    /// last listing was `clean`.
    fn written_elsewhere(store: &Arc<Interfere<Fake>>, host: &HostId, clean: bool) {
        let other = JobStore::new(Arc::clone(store), RandomIds).expect("opens");
        let hello = NodeHello {
            session: None,
            pool: "pool".to_owned(),
            claim: None,
            shape: None,
            labels: std::collections::BTreeMap::new(),
            phase: js_agent_wire::Phase::Serving,
            agent_process: None,
            agent_version: None,
        };
        let row = other.hello_node(host, &hello).expect("a hello");
        let state = NodeState {
            last_listing_clean: clean,
            ..row.record.state.clone()
        };
        other
            .settle_node(&row, state)
            .expect("settles")
            .expect("at the version");
    }

    fn set(hosts: &[&HostId]) -> BTreeSet<HostId> {
        hosts.iter().map(|host| (*host).clone()).collect()
    }

    // Catches a set that misses another server's write: the rows the feed names since the last
    // call are read again, and only those.
    #[test]
    fn another_servers_write_is_read_from_the_feed() {
        let (store, jobs) = tables();
        assert_eq!(jobs.unclean_nodes(), Ok(BTreeSet::new()));
        let a = host("host-a");
        written_elsewhere(&store, &a, false);
        store.take_reads();
        assert_eq!(jobs.unclean_nodes(), Ok(set(&[&a])));
        let reads = store.take_reads();
        assert!(
            !reads
                .iter()
                .any(|read| format!("{read:?}").contains("Scan")),
            "the rows were scanned again: {reads:?}"
        );
    }

    // Catches a row this build cannot read counted as unclean, or failing the call, when the feed
    // names it: such a row counts as no row, as in a scan of every row.
    #[test]
    fn a_row_this_build_cannot_read_counts_as_no_row() {
        let (store, jobs) = tables();
        let a = host("host-a");
        written_elsewhere(&store, &a, false);
        assert_eq!(jobs.unclean_nodes(), Ok(set(&[&a])));
        let garbage = Txn::new().put(keys::node(&a), b"not a row".to_vec(), Cond::Any);
        store.txn(garbage).expect("the store").expect("no conflict");
        assert_eq!(jobs.unclean_nodes(), Ok(BTreeSet::new()));
    }

    // Catches a set brought up to date from part of the feed: more node rows since the last call
    // than one page holds, or a feed trimmed past the last call, reads every row again.
    #[test]
    fn a_long_or_trimmed_feed_reads_every_row_again() {
        let (store, jobs) = tables();
        assert_eq!(jobs.unclean_nodes(), Ok(BTreeSet::new()));
        let a = host("host-a");
        written_elsewhere(&store, &a, false);
        let many = (0..=FEED_PAGE).fold(Txn::new(), |txn, n| {
            txn.put(format!("{}other-{n}", keys::NODE), b"x".to_vec(), Cond::Any)
        });
        store.txn(many).expect("the store").expect("no conflict");
        assert_eq!(jobs.unclean_nodes(), Ok(set(&[&a])), "after a long feed");

        let b = host("host-b");
        written_elsewhere(&store, &b, false);
        let head = store.head().expect("reads").expect("a head");
        store.trim_feed(head).expect("trims");
        assert_eq!(
            jobs.unclean_nodes(),
            Ok(set(&[&a, &b])),
            "after a trimmed feed"
        );
    }

    // Catches a set answered from rows the store could not read: a failed read of the head or of
    // a row the feed names fails the call, and the next call reads what it missed.
    #[test]
    fn a_read_that_fails_fails_the_call() {
        let (store, jobs) = tables();
        assert_eq!(jobs.unclean_nodes(), Ok(BTreeSet::new()));
        store.fail_next_head();
        assert!(matches!(jobs.unclean_nodes(), Err(JobStoreError::Store(_))));
        let a = host("host-a");
        written_elsewhere(&store, &a, false);
        store.fail_read_after(0);
        assert!(matches!(jobs.unclean_nodes(), Err(JobStoreError::Store(_))));
        assert_eq!(jobs.unclean_nodes(), Ok(set(&[&a])));
    }
}
