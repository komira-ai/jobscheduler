//! The caps of design section 6.

/// Whether a host counts toward the capable hosts of a pool's ordinary class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Capable.
    Capable,
    /// Lost: counted, since it may come back.
    Lost,
    /// Tainted: excluded.
    Tainted,
    /// Denied: excluded.
    Denied,
    /// Releasing (leaving the pool): excluded.
    Releasing,
}

/// The number of hosts that count as capable: tainted, denied and releasing excluded,
/// lost counted.
pub fn capable_hosts(hosts: &[Capability]) -> usize {
    hosts
        .iter()
        .filter(|c| matches!(c, Capability::Capable | Capability::Lost))
        .count()
}

/// `min(max_whole_node, capable_hosts - keep_for_ordinary)`, saturating at zero.
pub fn ceiling(max_whole_node: usize, capable_hosts: usize, keep_for_ordinary: usize) -> usize {
    max_whole_node.min(capable_hosts.saturating_sub(keep_for_ordinary))
}

/// The limits a reservation is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `max_whole_node`.
    pub max_whole_node: usize,
    /// The capable hosts, as [`capable_hosts`] counts them.
    pub capable_hosts: usize,
    /// `keep_for_ordinary`.
    pub keep_for_ordinary: usize,
    /// `max_draining`.
    pub max_draining: usize,
    /// `max_whole_node_per_consumer`; `None` never blocks.
    pub per_consumer: Option<usize>,
}

/// The counts a reservation is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Hosts whole: reserved plus held.
    pub whole: usize,
    /// Hosts reserved and not yet held.
    pub reserved: usize,
    /// Hosts the asking consumer has whole.
    pub consumer_whole: usize,
}

/// The cap that stops a reservation, so a status view can say why a request waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// Whole hosts are at `max_whole_node`, or at the capable hosts when nothing is kept
    /// for ordinary work.
    Ceiling,
    /// Whole hosts are at what the pool keeps for ordinary work: capable hosts minus
    /// `keep_for_ordinary`.
    KeepForOrdinary,
    /// Reserved hosts are at `max_draining`.
    MaxDraining,
    /// The consumer is at its limit.
    PerConsumer,
}

/// `Ok` when a reservation may start, else the first cap that blocks it, checked in the
/// order ceiling, keep for ordinary, draining, consumer.
pub fn may_reserve(counts: Counts, limits: Limits) -> Result<(), Blocked> {
    if counts.whole >= limits.max_whole_node {
        return Err(Blocked::Ceiling);
    }
    if counts.whole
        >= ceiling(
            limits.max_whole_node,
            limits.capable_hosts,
            limits.keep_for_ordinary,
        )
    {
        return Err(if limits.keep_for_ordinary == 0 {
            Blocked::Ceiling
        } else {
            Blocked::KeepForOrdinary
        });
    }
    if counts.reserved >= limits.max_draining {
        return Err(Blocked::MaxDraining);
    }
    match limits.per_consumer {
        Some(limit) if counts.consumer_whole >= limit => Err(Blocked::PerConsumer),
        _ => Ok(()),
    }
}

/// Whether a mark is a reservation or a held lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    /// Reserved, not yet held.
    Reserved,
    /// Held: a lease is committed.
    Held,
}

/// A host mark, as [`ceiling_excess`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// The request id the mark belongs to.
    pub request: String,
    /// Reserved or held.
    pub kind: MarkKind,
    /// When the mark was made, on the scheduler's clock.
    pub at: u64,
}

/// The requests whose reservations to drop now, newest first (a later `at`, then the
/// greater request id), so that whole hosts fall to the `ceiling`. A held mark is never
/// returned: when held marks alone exceed the ceiling, every reservation is dropped and
/// the held ones stay.
///
/// `marks` must hold ALL the whole marks of the pool, reserved and held, not just the
/// reservations: the excess is the count of every mark over the ceiling, and a partial
/// list undercounts it and drops too few.
pub fn ceiling_excess(marks: &[Mark], ceiling: usize) -> Vec<String> {
    let whole = marks.len();
    let mut reserved: Vec<&Mark> = marks
        .iter()
        .filter(|m| m.kind == MarkKind::Reserved)
        .collect();
    reserved.sort_by(|a, b| (b.at, &b.request).cmp(&(a.at, &a.request)));
    reserved
        .into_iter()
        .take(whole.saturating_sub(ceiling))
        .map(|m| m.request.clone())
        .collect()
}

/// An ordinary request queued in the pool, as the last-capable rule reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrdinaryRequest {
    /// Queue position: smaller is older.
    pub position: u64,
    /// The capable hosts of the request that no reservation holds.
    pub unreserved_capable: Vec<String>,
}

/// Whether a reservation by the whole-node request at `whole_position` may not start on
/// `host`: some ordinary request queued ahead of it has `host` as its only unreserved
/// capable host. Never blocks when `allow_last_capable`.
pub fn last_capable_blocks(
    host: &str,
    whole_position: u64,
    ordinary: &[OrdinaryRequest],
    allow_last_capable: bool,
) -> bool {
    !allow_last_capable
        && ordinary.iter().any(|r| {
            r.position < whole_position
                && matches!(r.unreserved_capable.as_slice(), [only] if only == host)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capable_count_excludes_tainted_denied_releasing_and_counts_lost() {
        // Kills: excluding lost, or counting any of the three excluded.
        use Capability::*;
        let hosts = [Capable, Lost, Tainted, Denied, Releasing, Capable];
        assert_eq!(capable_hosts(&hosts), 3);
        assert_eq!(capable_hosts(&[]), 0);
    }

    #[test]
    fn ceiling_is_the_smaller_of_the_two() {
        // Kills: max for min, and forgetting keep_for_ordinary.
        assert_eq!(ceiling(2, 10, 1), 2);
        assert_eq!(ceiling(10, 4, 1), 3);
        assert_eq!(ceiling(3, 4, 1), 3);
        assert_eq!(ceiling(4, 4, 0), 4);
    }

    #[test]
    fn ceiling_saturates_at_zero() {
        // Kills: underflow when keep exceeds the capable hosts.
        assert_eq!(ceiling(5, 1, 3), 0);
        assert_eq!(ceiling(5, 0, 1), 0);
    }

    fn limits() -> Limits {
        Limits {
            max_whole_node: 3,
            capable_hosts: 10,
            keep_for_ordinary: 1,
            max_draining: 2,
            per_consumer: Some(2),
        }
    }

    fn counts(whole: usize, reserved: usize, consumer_whole: usize) -> Counts {
        Counts {
            whole,
            reserved,
            consumer_whole,
        }
    }

    #[test]
    fn below_every_limit_is_allowed() {
        assert_eq!(may_reserve(counts(2, 1, 1), limits()), Ok(()));
    }

    #[test]
    fn ceiling_blocks_at_the_limit_and_not_below() {
        // Kills: `>` for `>=` on whole hosts.
        assert_eq!(
            may_reserve(counts(3, 0, 0), limits()),
            Err(Blocked::Ceiling)
        );
        assert_eq!(may_reserve(counts(2, 0, 0), limits()), Ok(()));
    }

    #[test]
    fn keep_for_ordinary_blocks_when_it_is_what_binds() {
        // capable 4, keep 1: three hosts may be whole although max_whole_node is 10.
        let l = Limits {
            max_whole_node: 10,
            capable_hosts: 4,
            keep_for_ordinary: 1,
            ..limits()
        };
        assert_eq!(
            may_reserve(counts(3, 0, 0), l),
            Err(Blocked::KeepForOrdinary)
        );
        assert_eq!(may_reserve(counts(2, 0, 0), l), Ok(()));
    }

    #[test]
    fn keeping_more_than_the_capable_hosts_blocks_everything_as_keep_for_ordinary() {
        // capable 1, keep 3: the ceiling saturates at zero, so even the first whole host
        // is refused, and the cap named is the keep.
        let l = Limits {
            max_whole_node: 10,
            capable_hosts: 1,
            keep_for_ordinary: 3,
            ..limits()
        };
        assert_eq!(
            may_reserve(counts(0, 0, 0), l),
            Err(Blocked::KeepForOrdinary)
        );
    }

    #[test]
    fn with_nothing_kept_the_capable_hosts_bound_is_the_ceiling() {
        // Kills: reporting keep_for_ordinary when keep is zero.
        let l = Limits {
            max_whole_node: 10,
            capable_hosts: 4,
            keep_for_ordinary: 0,
            ..limits()
        };
        assert_eq!(may_reserve(counts(4, 0, 0), l), Err(Blocked::Ceiling));
        assert_eq!(may_reserve(counts(3, 0, 0), l), Ok(()));
    }

    #[test]
    fn max_whole_node_wins_the_name_when_both_bind() {
        // Kills: swapping the order of the two ceiling checks.
        let l = Limits {
            max_whole_node: 3,
            capable_hosts: 4,
            keep_for_ordinary: 1,
            ..limits()
        };
        assert_eq!(may_reserve(counts(3, 0, 0), l), Err(Blocked::Ceiling));
    }

    #[test]
    fn max_draining_blocks_at_the_limit_and_not_below() {
        // Kills: `>` for `>=` on reserved hosts, and checking whole instead.
        assert_eq!(
            may_reserve(counts(2, 2, 0), limits()),
            Err(Blocked::MaxDraining)
        );
        assert_eq!(may_reserve(counts(2, 1, 0), limits()), Ok(()));
    }

    #[test]
    fn per_consumer_blocks_at_the_limit_and_not_below() {
        // Kills: `>` for `>=` on the consumer's hosts.
        assert_eq!(
            may_reserve(counts(2, 0, 2), limits()),
            Err(Blocked::PerConsumer)
        );
        assert_eq!(may_reserve(counts(2, 0, 1), limits()), Ok(()));
    }

    #[test]
    fn no_per_consumer_limit_never_blocks() {
        // Kills: reading None as zero.
        let l = Limits {
            per_consumer: None,
            ..limits()
        };
        assert_eq!(may_reserve(counts(2, 0, 1_000), l), Ok(()));
    }

    fn mark(request: &str, kind: MarkKind, at: u64) -> Mark {
        Mark {
            request: request.to_string(),
            kind,
            at,
        }
    }

    #[test]
    fn excess_drops_newest_reservations_first() {
        // Kills: oldest first, and dropping more than the excess.
        use MarkKind::*;
        let marks = [
            mark("r1", Reserved, 10),
            mark("r3", Reserved, 30),
            mark("r2", Reserved, 20),
            mark("h", Held, 1),
        ];
        assert_eq!(ceiling_excess(&marks, 2), ["r3", "r2"]);
        assert_eq!(ceiling_excess(&marks, 3), ["r3"]);
    }

    #[test]
    fn no_excess_drops_nothing() {
        use MarkKind::*;
        let marks = [mark("r1", Reserved, 10), mark("h", Held, 1)];
        assert!(ceiling_excess(&marks, 2).is_empty());
        assert!(ceiling_excess(&marks, 5).is_empty());
        assert!(ceiling_excess(&[], 0).is_empty());
    }

    #[test]
    fn excess_never_returns_a_held_mark_when_held_alone_exceed_the_ceiling() {
        // W5, both forms: the ceiling under the held leases drops every reservation
        // and revokes nothing.
        use MarkKind::*;
        let marks = [
            mark("h1", Held, 1),
            mark("h2", Held, 2),
            mark("h3", Held, 3),
            mark("r", Reserved, 9),
        ];
        assert_eq!(ceiling_excess(&marks, 1), ["r"]);
        assert_eq!(ceiling_excess(&marks, 0), ["r"]);
        let held_only = [mark("h1", Held, 1), mark("h2", Held, 2)];
        assert!(ceiling_excess(&held_only, 0).is_empty());
    }

    #[test]
    fn equal_times_drop_the_greater_request_id_first() {
        // Kills: an unstable order on a tie.
        use MarkKind::*;
        let marks = [mark("a", Reserved, 5), mark("b", Reserved, 5)];
        assert_eq!(ceiling_excess(&marks, 1), ["b"]);
    }

    fn ord(position: u64, hosts: &[&str]) -> OrdinaryRequest {
        OrdinaryRequest {
            position,
            unreserved_capable: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn older_request_with_only_this_host_blocks() {
        assert!(last_capable_blocks("h", 5, &[ord(2, &["h"])], false));
    }

    #[test]
    fn newer_request_with_only_this_host_does_not_block() {
        // Kills: protecting newer work (a stream of it could starve the whole request),
        // and `<=` for `<` at the same position.
        assert!(!last_capable_blocks("h", 5, &[ord(7, &["h"])], false));
        assert!(!last_capable_blocks("h", 5, &[ord(5, &["h"])], false));
    }

    #[test]
    fn an_older_request_with_another_host_to_go_to_does_not_block() {
        // Kills: blocking on any older request that could use the host.
        assert!(!last_capable_blocks("h", 5, &[ord(2, &["h", "g"])], false));
    }

    #[test]
    fn an_older_request_whose_only_host_is_another_does_not_block() {
        // Kills: blocking on any older request with a single capable host.
        assert!(!last_capable_blocks("h", 5, &[ord(2, &["g"])], false));
        assert!(!last_capable_blocks("h", 5, &[ord(2, &[])], false));
    }

    #[test]
    fn allow_last_capable_never_blocks() {
        // Kills: ignoring the switch.
        assert!(!last_capable_blocks("h", 5, &[ord(2, &["h"])], true));
    }

    #[test]
    fn one_blocking_request_among_others_blocks() {
        let ordinary = [ord(9, &["h"]), ord(3, &["g", "h"]), ord(1, &["h"])];
        assert!(last_capable_blocks("h", 5, &ordinary, false));
        assert!(!last_capable_blocks("h", 5, &[], false));
    }
}
