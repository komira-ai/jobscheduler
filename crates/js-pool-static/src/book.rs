use std::collections::{BTreeMap, BTreeSet};

use js_provider::{HostCapacity, Observed, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, HostId, LeaseId, ResourceClass, Shape};

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

#[derive(Debug)]
struct Host {
    shape: Shape,
    lease: Option<LeaseId>,
}

/// The booking table of a provider that leases whole hosts: which host each lease
/// holds, and every lease it has stopped.
///
/// It owns the rules the static pool and the registry of self-registered hosts share, so
/// neither copies them: a start is idempotent on the lease id, a stopped lease id never
/// starts again (`LeaseEnded`), a host is booked to one lease at most, the first free
/// fitting host by id wins, and a booking can be put back without a start ([`seat`]).
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

    /// Lists `id` with `shape`, free. A host already listed keeps its shape and its
    /// booking.
    pub fn insert_host(&mut self, id: HostId, shape: Shape) {
        self.hosts.entry(id).or_insert(Host { shape, lease: None });
    }

    /// Unlists `id` when it is listed and free; answers whether it did. A booked host is
    /// never removed: its booking would go with it.
    pub fn remove_free(&mut self, id: &HostId) -> bool {
        let free = self.hosts.get(id).is_some_and(|host| host.lease.is_none());
        free && self.hosts.remove(id).is_some()
    }

    /// Whether `id` is listed.
    pub fn contains(&self, id: &HostId) -> bool {
        self.hosts.contains_key(id)
    }

    /// The refusal for a spec whose class the provider cannot lease whole: a class it
    /// does not list, or one whose admission is not `BookAll`. `None` when the provider
    /// can lease it.
    pub fn class_gate(classes: &[ResourceClass], spec: &StartSpec) -> Option<RefusalCode> {
        let listed = classes.contains(spec.class());
        let whole = matches!(spec.class().admission(), Admission::BookAll);
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
        let mut fits_somewhere = false;
        for (id, host) in &mut self.hosts {
            if !host
                .shape
                .satisfies(&wanted.resources, &wanted.capabilities)
            {
                continue;
            }
            fits_somewhere = true;
            let free = host.lease.is_none();
            let open = [free, eligible(id, &host.shape)].into_iter().all(|ok| ok);
            if open {
                host.lease = Some(spec.lease());
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
            if host.lease.as_ref() == Some(lease) {
                host.lease = None;
            }
        }
        self.read(lease)
    }

    /// Where `lease` is booked: `Present` on its host, else `Absent`.
    pub fn read(&self, lease: &LeaseId) -> Observed {
        let holder = self
            .hosts
            .iter()
            .find(|(_, host)| host.lease.as_ref() == Some(lease));
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
            .filter_map(|(id, host)| Some((id.clone(), host.lease?)));
        held.collect()
    }

    /// Every listed host, in id order, with its shape and the lease holding it.
    pub fn hosts(&self) -> Vec<HostCapacity> {
        let listed = self.hosts.iter();
        let listed = listed.map(|(id, host)| HostCapacity {
            host: id.clone(),
            shape: host.shape.clone(),
            leased_to: host.lease,
        });
        listed.collect()
    }

    /// Books `host` for `lease` without a start: the way to put back a booking the
    /// provider lost with its process. The booking is what a start would have made, so
    /// the lease then reads `Present` on `host` and a stop frees it. Seating a lease on
    /// the host it already holds changes nothing.
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
        match listed.lease {
            Some(holder) => Err(SeatError::Taken {
                host: host.clone(),
                holder,
            }),
            None => {
                listed.lease = Some(lease);
                Ok(())
            }
        }
    }
}
