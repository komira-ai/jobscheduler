use std::collections::{BTreeMap, BTreeSet};

use js_provider::{HostCapacity, Observed, RefusalCode, StartAck, StartSpec};
use js_types::{HostId, LeaseId, MemoryHeadroom, ResourceClass, Resources, Shape};

/// Why [`HostBook::seat`] booked nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SeatError {
    /// The pool does not list the host.
    #[error("host {0} is not in the pool")]
    Unlisted(HostId),
    /// Another lease holds the host.
    #[error("host {host} is held by lease {holder}")]
    Taken {
        /// The host asked for.
        host: HostId,
        /// The lease that holds it.
        holder: LeaseId,
    },
    /// The lease already holds a different host.
    #[error("lease {lease} already holds host {host}")]
    Elsewhere {
        /// The lease.
        lease: LeaseId,
        /// The host it holds.
        host: HostId,
    },
    /// The lease was stopped, and a stopped lease never starts again.
    #[error("lease {0} was stopped")]
    Ended(LeaseId),
}

/// One lease on a host and what it books there.
#[derive(Debug)]
struct Booked {
    lease: LeaseId,
    resources: Resources,
    /// Whether the lease books the whole host, whatever its resources add up to.
    whole: bool,
}

#[derive(Debug)]
struct Host {
    shape: Shape,
    memory: MemoryHeadroom,
    /// The leases on the host, in the order they were booked.
    leases: Vec<Booked>,
}

impl Host {
    fn holds(&self, lease: &LeaseId) -> bool {
        self.leases.iter().any(|booked| booked.lease == *lease)
    }

    /// Whether a lease asking `wanted` can be booked here now. A lease that books a
    /// whole host needs a host with no lease. A lease that shares needs a host with no
    /// whole-host lease, and room in every dimension (count of GPUs and their class
    /// included) beside the leases already here.
    fn has_room(&self, wanted: &Resources, whole: bool) -> bool {
        if whole {
            return self.leases.is_empty();
        }
        if self.leases.iter().any(|booked| booked.whole) {
            return false;
        }
        let sum = self.leases.iter().try_fold(wanted.clone(), |sum, booked| {
            sum.checked_add(&booked.resources)
        });
        sum.is_some_and(|sum| sum.fits_in(self.shape.resources()))
    }
}

/// The booking table of a provider that leases whole hosts: which host each lease
/// holds, and every lease it has stopped.
///
/// It owns the rules the static pool and the registry of self-registered hosts share, so
/// neither copies them: a start is idempotent on the lease id, a stopped lease id never
/// starts again (`LeaseEnded`), a host books many leases (a whole-host lease takes the
/// host, a lease that shares takes its resources from what is left), the first fitting
/// host with room by id wins, and a booking can be put back without a start ([`seat`]).
/// What makes a host *eligible* to be booked beyond fitting and being free is the
/// caller's: it passes a predicate to [`start_where`].
///
/// The table remembers every stopped lease id as an exact set, 16 bytes per stopped lease
/// for the life of the process. A high-water mark per term would be smaller but would
/// also refuse a lower sequence that was never started and arrives late behind a higher
/// one that was stopped.
///
/// [`seat`]: HostBook::seat
/// [`start_where`]: HostBook::start_where
#[derive(Debug, Default)]
pub struct HostBook {
    hosts: BTreeMap<HostId, Host>,
    ended: BTreeSet<LeaseId>,
}

impl HostBook {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lists `id` with `shape`, free, all of whose memory is booked. A host already listed
    /// keeps its shape and its booking.
    pub fn insert_host(&mut self, id: HostId, shape: Shape) {
        let memory = MemoryHeadroom::full(shape.resources().memory_bytes());
        self.insert_host_with(id, shape, memory);
    }

    /// Lists `id` with `shape`, free, as [`insert_host`](HostBook::insert_host) does, and
    /// remembers `memory`: the memory the operator configured and the percent of it that
    /// `shape` books. The caller has already cut the memory of `shape`; the book books
    /// `shape` and never applies the percent itself, so the cut is made once.
    pub fn insert_host_with(&mut self, id: HostId, shape: Shape, memory: MemoryHeadroom) {
        self.hosts.entry(id).or_insert(Host {
            shape,
            memory,
            leases: Vec::new(),
        });
    }

    /// Unlists `id` when it is listed and free; answers whether it did. A booked host is
    /// never removed: its booking would go with it.
    pub fn remove_free(&mut self, id: &HostId) -> bool {
        let free = self
            .hosts
            .get(id)
            .is_some_and(|host| host.leases.is_empty());
        free && self.hosts.remove(id).is_some()
    }

    /// Whether `id` is listed.
    pub fn contains(&self, id: &HostId) -> bool {
        self.hosts.contains_key(id)
    }

    /// The refusal for a spec whose class the provider cannot lease whole: a class it
    /// does not list, or one whose admission does not book a whole host. `None` when the provider
    /// can lease it.
    pub fn class_gate(classes: &[ResourceClass], spec: &StartSpec) -> Option<RefusalCode> {
        let listed = classes.contains(spec.class());
        let whole = spec.class().admission().books_whole_host();
        let leasable = [listed, whole].into_iter().all(|ok| ok);
        (!leasable).then_some(RefusalCode::UnsupportedClass)
    }

    /// Books `spec`'s lease on a host, in this order of answers:
    ///
    /// 1. `LeaseEnded` when the lease was stopped;
    /// 2. `Started` on the host it already holds (a repeated start books nothing more);
    /// 3. `Refused(gate)` when the caller passes a refusal as `gate`;
    /// 4. `Started` on the first host by id that fits the request, is free, and that
    ///    `eligible` accepts;
    /// 5. `NoCapacity` when a host fits but none is free and eligible, else `NoFit`.
    ///
    /// A host that fits but is not eligible counts as a fit: the request could be served
    /// by it later.
    pub fn start_where(
        &mut self,
        spec: &StartSpec,
        gate: Option<RefusalCode>,
        eligible: &dyn Fn(&HostId, &Shape) -> bool,
    ) -> StartAck {
        if self.ended.contains(&spec.lease()) {
            return StartAck::Refused(RefusalCode::LeaseEnded);
        }
        if let Observed::Present { host } = self.read(&spec.lease()) {
            return StartAck::Started { host };
        }
        if let Some(code) = gate {
            return StartAck::Refused(code);
        }
        let wanted = spec.request();
        let whole = spec.class().admission().books_whole_host();
        let mut fits_somewhere = false;
        for (id, host) in &mut self.hosts {
            if !host
                .shape
                .satisfies(&wanted.resources, &wanted.capabilities)
            {
                continue;
            }
            fits_somewhere = true;
            let room = host.has_room(&wanted.resources, whole);
            let open = [room, eligible(id, &host.shape)].into_iter().all(|ok| ok);
            if open {
                let resources = match whole {
                    true => host.shape.resources().clone(),
                    false => wanted.resources.clone(),
                };
                host.leases.push(Booked {
                    lease: spec.lease(),
                    resources,
                    whole,
                });
                return StartAck::Started { host: id.clone() };
            }
        }
        match fits_somewhere {
            true => StartAck::Refused(RefusalCode::NoCapacity),
            false => StartAck::Refused(RefusalCode::NoFit),
        }
    }

    /// Stops `lease`: remembers it as ended, frees its host, and reads the lease back.
    pub fn stop(&mut self, lease: &LeaseId) -> Observed {
        self.ended.insert(*lease);
        for host in self.hosts.values_mut() {
            host.leases.retain(|booked| booked.lease != *lease);
        }
        self.read(lease)
    }

    /// Where `lease` is booked: `Present` on its host, else `Absent`.
    pub fn read(&self, lease: &LeaseId) -> Observed {
        let holder = self.hosts.iter().find(|(_, host)| host.holds(lease));
        match holder {
            Some((host, _)) => Observed::Present { host: host.clone() },
            None => Observed::Absent,
        }
    }

    /// The hosts that are booked and the lease holding each.
    pub fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        let held = self
            .hosts
            .iter()
            .filter_map(|(id, host)| Some((id.clone(), host.leases.first()?.lease)));
        held.collect()
    }

    /// Every lease booked on each booked host, in the order they were booked. [`booked`] names
    /// the first of them.
    ///
    /// [`booked`]: HostBook::booked
    pub fn leases(&self) -> BTreeMap<HostId, Vec<LeaseId>> {
        let held = self
            .hosts
            .iter()
            .filter(|(_, host)| !host.leases.is_empty());
        let held = held.map(|(id, host)| {
            let leases = host.leases.iter().map(|booked| booked.lease);
            (id.clone(), leases.collect())
        });
        held.collect()
    }

    /// Every listed host, in id order, with its shape and the lease holding it.
    pub fn hosts(&self) -> Vec<HostCapacity> {
        let listed = self.hosts.iter();
        let listed = listed.map(|(id, host)| HostCapacity {
            host: id.clone(),
            shape: host.shape.clone(),
            memory: host.memory,
            leased_to: host.leases.first().map(|booked| booked.lease),
        });
        listed.collect()
    }

    /// Books `host` for `lease` without a start: the way to put back a booking the
    /// provider lost with its process. The booking is what a start would have made, so
    /// the lease then reads `Present` on `host` and a stop frees it. Seating a lease on
    /// the host it already holds changes nothing. The booking is of the whole shape, for
    /// any class: putting back a lease that shares is owed to the step that starts one.
    ///
    /// Books nothing when `host` is not listed, when another lease holds it, when
    /// `lease` holds another host, or when `lease` was stopped.
    pub fn seat(&mut self, host: &HostId, lease: LeaseId) -> Result<(), SeatError> {
        if self.ended.contains(&lease) {
            return Err(SeatError::Ended(lease));
        }
        if let Observed::Present { host: held } = self.read(&lease) {
            return match held == *host {
                true => Ok(()),
                false => Err(SeatError::Elsewhere { lease, host: held }),
            };
        }
        let listed = self.hosts.get_mut(host);
        let listed = listed.ok_or_else(|| SeatError::Unlisted(host.clone()))?;
        match listed.leases.first() {
            Some(holder) => Err(SeatError::Taken {
                host: host.clone(),
                holder: holder.lease,
            }),
            None => {
                let resources = listed.shape.resources().clone();
                listed.leases.push(Booked {
                    lease,
                    resources,
                    whole: true,
                });
                Ok(())
            }
        }
    }
}
