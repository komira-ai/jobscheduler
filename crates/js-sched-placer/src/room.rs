//! What each open host has room for in a pass, held to the rule the store holds a placement to
//! ([`js_jobs::JobStore::place_claimed`]): the engine never chooses a host whose capacity the
//! store would find full, which would leave the job queued and the same host chosen again on
//! every pass while another host has room.
//!
//! A host's capacity is the bound its agent enforces (the summed bookings of its jobs against
//! what its hello offers) or, for a host that reports load admission, the slacked capacity the
//! store holds it to. A job that names no disk books the disk its host's agent books for it,
//! the scratch quota's default ([`OpenHost::quota_disk_bytes`]), so the allocation the store
//! records books what the agent books.

use std::collections::BTreeMap;

use js_agent_wire::NetworkCeiling;
use js_jobs::{AllocationRecord, AllocationStep, Booked, Capacity, HostBooking};
use js_sched::{ProviderRank, Refusal, RefusalKind};
use js_types::HostId;

/// A host a pass may place on: the network ceiling its hello advertises, whether it reported
/// that it runs host jobs, and the machine provider it is of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenHost {
    /// The most network the host's agent gives a job.
    pub ceiling: NetworkCeiling,
    /// Whether the host's hello reported `host-jobs`.
    pub host_jobs: bool,
    /// Whether the host's hello reported `load-admission`
    /// ([`js_agent_wire::LOAD_ADMISSION_CAPABILITY`]). `false`, the default: the host is packed
    /// by the summed bookings of its jobs, as its agent counts them.
    pub load_admission: bool,
    /// The machine provider the host is of, and where the Default platform lists it (design
    /// section 5.6, step 4): a job goes to a free host of the first provider in that order
    /// that has one. `None`: of no provider the platform lists, ranked after every host that
    /// is (the server offers no such host).
    pub rank: Option<ProviderRank>,
    /// What the host can book, which a placement is held to in its own transaction against the
    /// allocations the store holds ([`js_jobs::JobStore::place_claimed`]), and the walk holds
    /// its choice of host to ([`Room`]); `None`: the engine's picture alone decides.
    pub capacity: Option<Capacity>,
    /// The disk the host's agent books for a job that names none: its scratch quota's default.
    /// `None`: the host has no such default, and such a job books no disk there.
    pub quota_disk_bytes: Option<u64>,
}

/// What a job of `booked` books on a host whose agent books `quota_disk_bytes` for a job that
/// names no disk: a share with no disk named books that quota (when the host has one); a share
/// that names its disk, a share on a host with no quota, and a whole host book as they are.
pub fn on_host(booked: Booked, quota_disk_bytes: Option<u64>) -> Booked {
    match booked {
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes: 0,
            accelerators,
        } => Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes: quota_disk_bytes.unwrap_or(0),
            accelerators,
        },
        other => other,
    }
}

/// What `booked` books on `host` of `open` ([`on_host`] with the host's quota); `booked` itself
/// for a host `open` does not list.
pub fn booked_on(open: &BTreeMap<HostId, OpenHost>, host: &HostId, booked: Booked) -> Booked {
    let quota = open.get(host).and_then(|offer| offer.quota_disk_bytes);
    on_host(booked, quota)
}

/// One host's line of the ledger.
#[derive(Debug, Clone)]
struct Line {
    capacity: Capacity,
    quota_disk_bytes: Option<u64>,
    /// What its live allocations book, and what the pass's placements on it have added.
    booking: HostBooking,
}

/// The ledger of a pass: for each open host that has a capacity, that capacity, its quota
/// default and the sum of what it books, which each placement of the walk adds to.
#[derive(Debug, Clone, Default)]
pub struct Room {
    lines: BTreeMap<HostId, Line>,
}

impl Room {
    /// The ledger of the hosts `open`, each with the sum of the allocations of `live` that book
    /// on it (those whose step is booked or releasing, as the store sums them).
    pub fn of(open: &BTreeMap<HostId, OpenHost>, live: &[AllocationRecord]) -> Self {
        let mut lines: BTreeMap<HostId, Line> = open
            .iter()
            .filter_map(|(host, offer)| {
                let line = Line {
                    capacity: offer.capacity?,
                    quota_disk_bytes: offer.quota_disk_bytes,
                    booking: HostBooking::default(),
                };
                Some((host.clone(), line))
            })
            .collect();
        for allocation in live {
            if allocation.step == AllocationStep::Freed {
                continue;
            }
            if let Some(line) = lines.get_mut(&allocation.host) {
                line.booking = plus(line.booking, &allocation.booked);
            }
        }
        Self { lines }
    }

    /// Whether `host` has room for `booked`, as the store's capacity check reads it; a host
    /// with no capacity always has.
    pub fn fits(&self, host: &HostId, booked: Booked) -> bool {
        self.lines.get(host).is_none_or(|line| {
            let booked = on_host(booked, line.quota_disk_bytes);
            line.capacity.has_room(&line.booking, &booked)
        })
    }

    /// Why `host` has no room for `booked` by memory, when that is why: the memory its
    /// allocations book, the memory `booked` adds and the memory it can book, as a booked-memory
    /// refusal. `None` when the host has no capacity, `booked` is a whole host or its memory fits.
    pub(crate) fn memory_refusal(&self, host: &HostId, booked: Booked) -> Option<Refusal> {
        let line = self.lines.get(host)?;
        let Booked::Shape { memory_bytes, .. } = booked else {
            return None;
        };
        let (counted, limit) = (line.booking.memory_bytes, line.capacity.memory_bytes);
        let fits = counted
            .checked_add(memory_bytes)
            .is_some_and(|total| total <= limit);
        (!fits).then_some(Refusal {
            kind: RefusalKind::BookedMemory,
            counted,
            new: memory_bytes,
            limit,
            measured_cpu_millis: 0,
        })
    }

    /// Adds a placement of `booked` on `host` to its sum.
    pub fn add(&mut self, host: &HostId, booked: Booked) {
        if let Some(line) = self.lines.get_mut(host) {
            let booked = on_host(booked, line.quota_disk_bytes);
            line.booking = plus(line.booking, &booked);
        }
    }
}

/// `booking` with `booked` added (a count that would overflow stays at the top).
fn plus(booking: HostBooking, booked: &Booked) -> HostBooking {
    let allocations = booking.allocations.saturating_add(1);
    match *booked {
        Booked::WholeHost => HostBooking {
            whole_host: true,
            allocations,
            ..booking
        },
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes,
            accelerators,
        } => HostBooking {
            cpu_millis: booking.cpu_millis.saturating_add(cpu_millis),
            memory_bytes: booking.memory_bytes.saturating_add(memory_bytes),
            disk_bytes: booking.disk_bytes.saturating_add(disk_bytes),
            accelerators: booking.accelerators.saturating_add(accelerators),
            allocations,
            ..booking
        },
    }
}

#[cfg(test)]
#[path = "room_tests.rs"]
mod tests;
