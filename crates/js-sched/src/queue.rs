//! The binding order of a queue of jobs (design section 5.4, "queue order is binding", and
//! section 8, rows 11 and 17): [`walk_in_order`].
//!
//! The walk decides only which entries may be offered which hosts. Whether an entry fits a
//! host, and which host it gets, is the caller's placement (on the lease server, one
//! placement round of the [`crate::Scheduler`] per entry, held to the hosts the walk leaves
//! open to it).

use std::collections::BTreeSet;

/// Walks `queue` front to back over `hosts`, and returns for each entry, in queue order, the
/// host it was placed on, or `None` for an entry that stays queued.
///
/// **Order is binding.** Each entry is offered the hosts still open to it: `place` places it
/// on one of them and answers that host, or answers `None` when it fits none of them now. An
/// entry placed nowhere stays queued, and every open host it could use (`can_use`: it fits
/// there when the host has room) is closed to every entry after it. So a later entry takes a
/// host only if no earlier entry still waiting could use it, either because that entry fits
/// nowhere on the host or because the host was never open to it (a host held for another
/// request under the whole-host rules is not in `hosts`). When a host frees, the front entry
/// that fits it gets it (row 11), and a later entry never takes capacity an earlier one waits
/// for. This function is the whole rule; nothing else orders the queue.
///
/// **No mark for a share job (row 17).** An entry that waits is given nothing: no host is
/// marked or held for it beyond this walk, and nothing that runs is stopped for it. Work
/// that does not walk behind it (a lease, which is placed on arrival, or an entry of higher
/// priority queued after it, which the queue order puts in front of it) can keep a host from
/// ever having room for a large share job. Such a job is placed the first time a walk finds
/// room for it, and by design not before: only a whole-host job is marked and drained for.
pub fn walk_in_order<J, H: Ord + Clone>(
    queue: &[J],
    hosts: &BTreeSet<H>,
    can_use: impl Fn(&J, &H) -> bool,
    mut place: impl FnMut(&J, &BTreeSet<H>) -> Option<H>,
) -> Vec<Option<H>> {
    let mut open = hosts.clone();
    let mut placed = Vec::with_capacity(queue.len());
    for entry in queue {
        let host = place(entry, &open);
        if host.is_none() {
            open.retain(|host| !can_use(entry, host));
        }
        placed.push(host);
    }
    placed
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::walk_in_order;

    /// Hosts with room left, entries asking for some, and hosts big enough for each entry
    /// once empty: places an entry on the first open host with room for it.
    struct Hosts {
        room: BTreeMap<&'static str, u32>,
        size: BTreeMap<&'static str, u32>,
    }

    impl Hosts {
        fn new(hosts: &[(&'static str, u32, u32)]) -> Self {
            Self {
                room: hosts.iter().map(|(name, room, _)| (*name, *room)).collect(),
                size: hosts.iter().map(|(name, _, size)| (*name, *size)).collect(),
            }
        }

        fn names(&self) -> BTreeSet<&'static str> {
            self.room.keys().copied().collect()
        }

        /// Walks `asks` and returns where each went.
        fn walk(&mut self, asks: &[u32]) -> Vec<Option<&'static str>> {
            let size = self.size.clone();
            let room = &mut self.room;
            walk_in_order(
                asks,
                &room.keys().copied().collect(),
                |ask, host| size[host] >= *ask,
                |ask, open| {
                    let host = open.iter().copied().find(|host| room[host] >= *ask)?;
                    *room.get_mut(host).unwrap() -= *ask;
                    Some(host)
                },
            )
        }
    }

    /// Asserts that when a host frees, the front entry that fits it gets it and an entry
    /// behind it that would also fit does not. It catches a walk that lets a later entry
    /// take a host the front one waits for.
    #[test]
    fn the_front_entry_that_fits_a_freed_host_gets_it() {
        let mut hosts = Hosts::new(&[("a", 4, 8)]);
        assert_eq!(hosts.walk(&[6, 2]), [None, None]);
        hosts.room.insert("a", 8);
        assert_eq!(hosts.walk(&[6, 2]), [Some("a"), Some("a")]);
    }

    /// Asserts that a waiting entry closes only the hosts it could use: an entry behind it
    /// still takes a host the waiting one fits nowhere on, and every host stays open when
    /// nothing waits. It catches a walk that stops at the first entry that waits.
    #[test]
    fn a_waiting_entry_closes_only_the_hosts_it_could_use() {
        let mut hosts = Hosts::new(&[("big", 2, 16), ("small", 4, 4)]);
        assert_eq!(hosts.names(), BTreeSet::from(["big", "small"]));
        assert_eq!(
            hosts.walk(&[12, 2, 2, 1]),
            [None, Some("small"), Some("small"), None]
        );
        let mut empty = Hosts::new(&[]);
        assert_eq!(empty.walk(&[1]), [None]);
        assert_eq!(empty.walk(&[]), Vec::<Option<&str>>::new());
    }
}
