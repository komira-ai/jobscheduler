//! The wire form of a node's load, as the plain values the usage seam takes.
//!
//! The conversion is a free function and not a `From`: both types would be foreign to
//! this crate's callers on one side or the other. It is pure. A lease id on the wire is
//! the text `<term>-<seq>` ([`js_agent_wire::lease_parts`] is its one parser); a record
//! whose id does not parse is left out and counted, never an error.

use js_agent_wire::{NodeLoad, lease_parts};
use js_types::{HostId, LeaseId};

use crate::usage::{ClockIn, HostLimits, LoadIn, MinuteIn};

/// A load as plain values, and how many of its records were left out because their lease
/// id is not `<term>-<seq>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapped {
    pub(crate) load: LoadIn,
    pub(crate) unparsable: u64,
}

/// The lease a wire id names, if it is of the form `<term>-<seq>`.
pub(crate) fn lease_of(text: &str) -> Option<LeaseId> {
    lease_parts(text).map(|(term, seq)| LeaseId::new(term, seq))
}

/// `load` as plain values. Records with an id that does not parse are dropped and
/// counted in [`Mapped::unparsable`].
pub(crate) fn load_in(load: &NodeLoad) -> Mapped {
    let covered: Vec<LeaseId> = load.covered.iter().filter_map(|id| lease_of(id)).collect();
    let minutes: Vec<MinuteIn> = load
        .minutes
        .iter()
        .filter_map(|m| {
            let lease = lease_of(&m.lease_id)?;
            Some(MinuteIn {
                lease,
                minute: m.minute,
                mean_cores_milli: m.cpu_mean_millis,
                peak_window_milli: m.cpu_peak_window_millis,
                working_set_max_bytes: m.ws_max_bytes,
            })
        })
        .collect();
    let clocks: Vec<ClockIn> = load
        .clocks
        .iter()
        .filter_map(|c| {
            let lease = lease_of(&c.lease_id)?;
            Some(ClockIn {
                lease,
                started_mono_usec: c.started_mono_usec,
            })
        })
        .collect();
    let kept = covered.len() + minutes.len() + clocks.len();
    let sent = load.covered.len() + load.minutes.len() + load.clocks.len();
    Mapped {
        unparsable: u64::try_from(sent - kept).unwrap_or(u64::MAX),
        load: LoadIn {
            seq: load.seq,
            mono_usec: load.mono_usec,
            cpu_usage_usec: load.cpu_usage_usec,
            working_set_bytes: load.working_set_bytes,
            actions_cpu_limited: load.actions_cpu_limited,
            covered,
            minutes,
            clocks,
        },
    }
}

/// What the pool books for a host, as the limits the sink checks a reading against: one
/// thread for each started thousandth-block of a core (rounded up, at least one) and the
/// memory in bytes.
pub(crate) fn limits_of(cpu_millis: u64, memory_bytes: u64) -> HostLimits {
    let threads = cpu_millis.div_ceil(1000).max(1);
    HostLimits {
        threads: u32::try_from(threads).unwrap_or(u32::MAX),
        memory_bytes,
    }
}

/// The host a heartbeat names, if its id is one.
pub(crate) fn host_of(text: &str) -> Option<HostId> {
    HostId::new(text).ok()
}

#[cfg(test)]
mod tests {
    use js_agent_wire::{LeaseClock, LeaseMinute, LoadDrops};

    use super::*;

    fn wire() -> NodeLoad {
        NodeLoad {
            seq: 7,
            mono_usec: 6_000_000,
            cpu_usage_usec: 5_000_000,
            working_set_bytes: 1 << 30,
            active_file_bytes: 0,
            actions_cpu_limited: true,
            covered: vec!["3-4".into()],
            clocks: vec![LeaseClock {
                lease_id: "3-4".into(),
                started_mono_usec: 1_000_000,
            }],
            minutes: vec![LeaseMinute {
                lease_id: "3-4".into(),
                minute: 2,
                cpu_mean_millis: 1500,
                cpu_peak_window_millis: 2500,
                ws_max_bytes: 1 << 29,
            }],
            dropped: LoadDrops::default(),
        }
    }

    /// Asserts that every field of a load reaches the plain form unchanged, with the
    /// lease ids parsed to `LeaseId`, and that nothing is counted as dropped. It catches
    /// a field copied from its neighbour (the mean for the peak, the start for the
    /// reading) and an id parsed with its term and sequence swapped.
    #[test]
    fn every_field_crosses_and_the_ids_are_parsed() {
        let mapped = load_in(&wire());
        assert_eq!(mapped.unparsable, 0);
        let load = mapped.load;
        assert_eq!(
            (load.seq, load.mono_usec, load.cpu_usage_usec),
            (7, 6_000_000, 5_000_000)
        );
        assert_eq!(load.working_set_bytes, 1 << 30);
        assert!(load.actions_cpu_limited);
        assert_eq!(load.covered, vec![LeaseId::new(3, 4)]);
        assert_eq!(
            load.clocks,
            vec![ClockIn {
                lease: LeaseId::new(3, 4),
                started_mono_usec: 1_000_000
            }]
        );
        assert_eq!(
            load.minutes,
            vec![MinuteIn {
                lease: LeaseId::new(3, 4),
                minute: 2,
                mean_cores_milli: 1500,
                peak_window_milli: 2500,
                working_set_max_bytes: 1 << 29,
            }]
        );
    }

    /// Asserts that a record whose lease id is not `<term>-<seq>` in canonical decimal is
    /// left out of its list, the others stay, and each one left out is counted: in all
    /// three lists. It catches a bad id that fails the whole load, one that is kept with a
    /// guessed id, and a count that misses a list.
    #[test]
    fn an_unparsable_id_drops_its_record_and_is_counted() {
        let mut load = wire();
        for bad in [
            "", "3", "3-", "-4", "03-4", "3-04", "a-b", "3.4", "3-4-5", "+3-4",
        ] {
            load.covered.push(bad.to_string());
        }
        load.clocks.push(LeaseClock {
            lease_id: "x".into(),
            started_mono_usec: 1,
        });
        load.minutes.push(LeaseMinute {
            lease_id: "y".into(),
            minute: 0,
            cpu_mean_millis: 0,
            cpu_peak_window_millis: 0,
            ws_max_bytes: 0,
        });
        let mapped = load_in(&load);
        assert_eq!(mapped.unparsable, 12);
        assert_eq!(mapped.load.covered, vec![LeaseId::new(3, 4)]);
        assert_eq!(mapped.load.clocks.len(), 1);
        assert_eq!(mapped.load.minutes.len(), 1);
    }

    /// Asserts that the ids of the root form read as the lease the table prints, so a
    /// minute is filed under the lease a result names, and that a term and sequence at
    /// the top of the range survive.
    #[test]
    fn a_lease_id_reads_as_the_table_prints_it() {
        assert_eq!(lease_of("0-0"), Some(LeaseId::new(0, 0)));
        let top = format!("{0}-{0}", u64::MAX);
        assert_eq!(lease_of(&top), Some(LeaseId::new(u64::MAX, u64::MAX)));
        assert_eq!(lease_of("1.9"), None);
        assert_eq!(LeaseId::new(1, 9).to_string(), "1-9");
    }

    /// Asserts how the pool's shape becomes limits: a started core is a thread, a shape
    /// with less than a core still has one thread, and a shape beyond what a `u32` holds
    /// saturates. It catches a floor that gives a small host zero threads (every reading
    /// would then be a lie) and a cast that wraps.
    #[test]
    fn limits_round_cores_up_and_never_to_zero() {
        assert_eq!(limits_of(8000, 7).threads, 8);
        assert_eq!(limits_of(8001, 7).threads, 9);
        assert_eq!(limits_of(500, 7).threads, 1);
        assert_eq!(limits_of(0, 7).threads, 1);
        assert_eq!(limits_of(u64::MAX, 7).threads, u32::MAX);
        assert_eq!(limits_of(1, 7).memory_bytes, 7);
    }

    /// Asserts that a host id is read as the registry reads it, and text that is not one
    /// is no host.
    #[test]
    fn a_host_id_is_read_as_the_registry_reads_it() {
        assert_eq!(host_of("burst-1"), Some(HostId::new("burst-1").unwrap()));
        assert_eq!(host_of("UPPER"), None);
        assert_eq!(host_of(""), None);
    }
}
