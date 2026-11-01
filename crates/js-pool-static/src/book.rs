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
    /// The host holds no lease, yet is too small for the resources the lease books.
    #[error("host {0} has no room for the lease")]
    NoRoom(HostId),
}

/// One lease on a host and what it books there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The lease.
    pub lease: LeaseId,
    /// What it books on the host: its own resources, or the whole shape for a lease that
    /// books the whole host.
    pub resources: Resources,
    /// Whether the lease books the whole host, whatever its resources add up to.
    pub whole: bool,
}

/// A host a start could be booked on: it fits the request, has room for it and is
/// eligible. [`HostBook::start_by`] shows the candidates to the caller's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The host.
    pub host: HostId,
    /// What the host offers to be booked.
    pub shape: Shape,
    /// The leases booked on it, in the order they were booked.
    pub leases: Vec<Held>,
}

#[derive(Debug)]
struct Host {
    shape: Shape,
    memory: MemoryHeadroom,
    /// The leases on the host, in the order they were booked.
    leases: Vec<Held>,
}

impl Host {
    fn holds(&self, lease: &LeaseId) -> bool {
        self.leases.iter().any(|booked| booked.lease == *lease)
    }

    /// Books `spec`'s lease here: the whole shape for a lease that books the whole host,
    /// else the resources it asks for.
    fn book(&mut self, spec: &StartSpec, whole: bool) {
        let resources = match whole {
            true => self.shape.resources().clone(),
            false => spec.request().resources.clone(),
        };
        self.leases.push(Held {
            lease: spec.lease(),
            resources,
            whole,
        });
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

    /// The refusal for a spec whose class the provider does not list. `None` when it does,
    /// whether the class books a whole host or part of one: the gate of a provider that
    /// lets leases share a host.
    pub fn listed_gate(classes: &[ResourceClass], spec: &StartSpec) -> Option<RefusalCode> {
        (!classes.contains(spec.class())).then_some(RefusalCode::UnsupportedClass)
    }

    /// Books `spec`'s lease on a host, in this order of answers:
    ///
    /// 1. `LeaseEnded` when the lease was stopped;
    /// 2. `Started` on the host it already holds (a repeated start books nothing more);
    /// 3. `Refused(gate)` when the caller passes a refusal as `gate`;
    /// 4. `Started` on the first host by id that fits the request, has room for it, and
    ///    that `eligible` accepts;
    /// 5. `NoCapacity` when a host fits but none has room and is eligible, else `NoFit`.
    ///
    /// A host that fits but is not eligible counts as a fit: the request could be served
    /// by it later. This stops at the first host that qualifies; [`start_by`] shows the
    /// caller every one.
    ///
    /// [`start_by`]: HostBook::start_by
    pub fn start_where(
        &mut self,
        spec: &StartSpec,
        gate: Option<RefusalCode>,
        eligible: &dyn Fn(&HostId, &Shape) -> bool,
    ) -> StartAck {
        if let Some(settled) = self.settled(spec, gate) {
            return settled;
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
                host.book(spec, whole);
                return StartAck::Started { host: id.clone() };
            }
        }
        Self::declined(fits_somewhere)
    }

    /// The answer a start already has before any host is looked at: the lease was stopped,
    /// it already holds a host, or the caller's gate refuses it.
    fn settled(&self, spec: &StartSpec, gate: Option<RefusalCode>) -> Option<StartAck> {
        if self.ended.contains(&spec.lease()) {
            return Some(StartAck::Refused(RefusalCode::LeaseEnded));
        }
        if let Observed::Present { host } = self.read(&spec.lease()) {
            return Some(StartAck::Started { host });
        }
        gate.map(StartAck::Refused)
    }

    /// The refusal of a start no host took: `NoCapacity` when a host fits, else `NoFit`.
    fn declined(fits_somewhere: bool) -> StartAck {
        match fits_somewhere {
            true => StartAck::Refused(RefusalCode::NoCapacity),
            false => StartAck::Refused(RefusalCode::NoFit),
        }
    }

    /// Books `spec`'s lease as [`start_where`] does, except that the caller picks the host:
    /// `choose` is shown every candidate (a host that fits, has room and is eligible, in
    /// id order; it is not asked when there is none) and answers the index of the one to
    /// book. The answers are those of [`start_where`]; `NoCapacity` also when `choose`
    /// picks none, or an index past the candidates.
    ///
    /// [`start_where`]: HostBook::start_where
    pub fn start_by(
        &mut self,
        spec: &StartSpec,
        gate: Option<RefusalCode>,
        eligible: &dyn Fn(&HostId, &Shape) -> bool,
        choose: &dyn Fn(&[Candidate]) -> Option<usize>,
    ) -> StartAck {
        if let Some(settled) = self.settled(spec, gate) {
            return settled;
        }
        let wanted = spec.request();
        let whole = spec.class().admission().books_whole_host();
        let fitting: Vec<(&HostId, &Host)> = self
            .hosts
            .iter()
            .filter(|(_, host)| {
                host.shape
                    .satisfies(&wanted.resources, &wanted.capabilities)
            })
            .collect();
        let fits_somewhere = !fitting.is_empty();
        let open: Vec<Candidate> = fitting
            .into_iter()
            .filter(|(id, host)| {
                let room = host.has_room(&wanted.resources, whole);
                [room, eligible(id, &host.shape)].into_iter().all(|ok| ok)
            })
            .map(|(id, host)| Candidate {
                host: id.clone(),
                shape: host.shape.clone(),
                leases: host.leases.clone(),
            })
            .collect();
        let chosen = match open.is_empty() {
            true => None,
            false => choose(&open),
        };
        let picked = chosen.and_then(|at| open.get(at));
        let Some(picked) = picked else {
            return Self::declined(fits_somewhere);
        };
        let id = picked.host.clone();
        // The candidate was read from this map a moment ago, so the entry is there.
        let _ = self
            .hosts
            .entry(id.clone())
            .and_modify(|host| host.book(spec, whole));
        StartAck::Started { host: id }
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

    /// Every listed host, in id order, with its shape and the leases booked on it.
    pub fn hosts(&self) -> Vec<HostCapacity> {
        let listed = self.hosts.iter();
        let listed = listed.map(|(id, host)| HostCapacity {
            host: id.clone(),
            shape: host.shape.clone(),
            memory: host.memory,
            leased_to: host.leases.first().map(|booked| booked.lease),
            leases: host.leases.iter().map(|booked| booked.lease).collect(),
        });
        listed.collect()
    }

    /// Books `host` for `lease` without a start: the way to put back a booking the
    /// provider lost with its process. The booking is what a start would have made, so
    /// the lease then reads `Present` on `host` and a stop frees it. Seating a lease on
    /// the host it already holds changes nothing. The booking is of the whole shape, as a
    /// lease of a class that books a whole host makes; [`seat_as`] puts back a lease that
    /// shares.
    ///
    /// Books nothing when `host` is not listed, when another lease holds it, when
    /// `lease` holds another host, or when `lease` was stopped.
    ///
    /// [`seat_as`]: HostBook::seat_as
    pub fn seat(&mut self, host: &HostId, lease: LeaseId) -> Result<(), SeatError> {
        self.seat_as(host, lease, None)
    }

    /// Books `host` for `lease` as [`seat`] does, with the resources the lease booked:
    /// `Some(resources)` puts back a lease that shares the host, which books those and
    /// needs a host with no whole-host lease and room for them beside the leases already
    /// there; `None` books the whole shape and needs a host with no lease.
    ///
    /// The refusals are those of [`seat`], and `Taken` also when the room is gone because
    /// of the leases already seated (the first of them is named), `NoRoom` when a host
    /// with no lease is too small for `resources`.
    ///
    /// [`seat`]: HostBook::seat
    pub fn seat_as(
        &mut self,
        host: &HostId,
        lease: LeaseId,
        shares: Option<Resources>,
    ) -> Result<(), SeatError> {
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
        let whole = shares.is_none();
        let resources = shares.unwrap_or_else(|| listed.shape.resources().clone());
        if !listed.has_room(&resources, whole) {
            return Err(match listed.leases.first() {
                Some(holder) => SeatError::Taken {
                    host: host.clone(),
                    holder: holder.lease,
                },
                None => SeatError::NoRoom(host.clone()),
            });
        }
        listed.leases.push(Held {
            lease,
            resources,
            whole,
        });
        Ok(())
    }
}
