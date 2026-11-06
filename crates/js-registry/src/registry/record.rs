//! What the registry keeps for each host that said hello, and what it can say of it.

use super::*;
use js_usage::DrainOutcome;

/// A lease booked on a host, the latest it can end (its start plus its max wall time) and
/// when it started, in milliseconds on the registry's clock, and what its request went
/// through before the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Bound {
    pub(super) lease: LeaseId,
    pub(super) end: Timestamp,
    pub(super) started_at: u64,
    pub(super) queue: Queue,
}

/// What a lease's request went through before the lease started, for the record of its end
/// ([`js_usage::EndedRow`]): how long it waited, what became of the host set aside for it
/// and how many hosts it gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Queue {
    pub(super) queued_ms: Option<u64>,
    pub(super) drain: DrainOutcome,
    pub(super) give_ups: Option<u32>,
}

impl Queue {
    /// An ordinary lease: placed on arrival, it waited for nothing and drained nothing.
    pub(super) const ORDINARY: Self = Self {
        queued_ms: Some(0),
        drain: DrainOutcome::None,
        give_ups: None,
    };

    /// A whole-node request started at `now`: it waited from `since`, its first presentation
    /// (`None` for one that never waited, which waited for nothing); `held` says it started on
    /// the host its mark set aside, `marked` that it held a mark at some time while it waited,
    /// and `give_ups` is how many hosts it had given up. A request that held a mark, or gave a
    /// host up, and did not start on its mark's host had its mark dropped.
    pub(super) fn whole(
        now: Timestamp,
        since: Option<Timestamp>,
        (held, marked): (bool, bool),
        give_ups: u32,
    ) -> Self {
        let waited = since.map_or(0, |since| now.millis().saturating_sub(since.millis()));
        let drain = match (held, marked | (give_ups > 0)) {
            (true, _) => DrainOutcome::MarkHeld,
            (false, true) => DrainOutcome::MarkDropped,
            (false, false) => DrainOutcome::NoMark,
        };
        Self {
            queued_ms: Some(waited),
            drain,
            give_ups: Some(give_ups),
        }
    }

    /// A lease put back after a restart, of a pool whose class books a whole node (`whole`)
    /// or not: an ordinary lease's queue is known, a whole-node request's was not kept.
    pub(super) fn restored(whole: bool) -> Self {
        match whole {
            true => Self {
                queued_ms: None,
                drain: DrainOutcome::Unknown,
                give_ups: None,
            },
            false => Self::ORDINARY,
        }
    }
}

#[derive(Debug)]
pub(super) struct Record {
    pub(super) binding: Option<Binding>,
    pub(super) epoch: u64,
    /// The digest of the secret issued at the hello that began this epoch; `None` for a
    /// host that has not said hello since a restart.
    pub(super) session: Option<SessionDigest>,
    pub(super) phase: Phase,
    pub(super) heard: Option<Timestamp>,
    /// When a booking was put back for the host after a restart; `None` for a host that
    /// has said hello. It is what an awaiting host's retention counts from.
    pub(super) seated: Option<Timestamp>,
    pub(super) agent_version: Option<String>,
    pub(super) labels: BTreeMap<String, String>,
    /// Whether the host's last hello reported the `run-files` capability. It changes only
    /// at a hello, and a hello of a lost host gives up every run still queued for it, so
    /// the check at queueing time is the only one a start needs.
    pub(super) files: bool,
    /// Whether the host's last hello reported `exec` as `sandbox-exec`: it runs native
    /// jobs. Kept like `files`, and for the same reason.
    pub(super) native: bool,
    /// Whether the host's last hello reported a bundle store (`bundle-root`). Kept like
    /// `files`.
    pub(super) bundles: bool,
    /// The quota in bytes the host's last hello said it enforces on a run that sets no
    /// `disk_bytes` (`scratch-default-gib`), when it also reported `scratch-quota`; `None`
    /// for a host that reported no quota, or a default that is missing, not a whole
    /// number, zero or too large to count. Kept like `files`, and for the same reason: the
    /// run check sees only the pool's shape otherwise. It changes only at a hello, and a
    /// run books what it was given at queueing, so a new default never rebooks a live run.
    pub(super) quota_default: Option<u64>,
    /// The run ids the host listed as held in its last hello or heartbeat.
    pub(super) held: BTreeSet<String>,
    /// The heartbeats taken in this session.
    pub(super) beats: u64,
    /// The first taint the host reported. It outlives a session and, through the file of
    /// bindings, a restart.
    pub(super) taint: Option<Taint>,
    /// When the host's cordon lapses, once it has been cordoned ([`Registry::cordon`]): it
    /// takes no new lease until then. It outlives a session and, through the file of
    /// bindings, a restart; a lapsed value is a cordon that is over.
    pub(super) cordon: Option<Timestamp>,
    /// When the host's current session began: the time of the hello that began it. `None`
    /// for a host that has not said hello since a restart. A heartbeat does not move it.
    pub(super) hello_at: Option<Timestamp>,
    /// The `runtime` capability of the host's last hello. Kept like `files`.
    pub(super) runtime: Option<String>,
    /// Where the host stands in a whole-node lease; `Free` for a host that never held one.
    pub(super) exclusive: Exclusive,
    /// Every lease started on the host since the registry began, with the latest it can end
    /// and when it started. A lease put back after a restart has them only with a source of
    /// lives set, from the lease file ([`crate::RegisteredProvider::seat_booked`]); without
    /// one it has none, and counts as ending now. Held in memory only; a lease that is no
    /// longer booked on the host is dropped at the next start or seat.
    pub(super) bounds: Vec<Bound>,
    /// Whether the host is being scaled away (see [`Registry::drain`]). It outlives a
    /// session and is held in memory only.
    pub(super) leaving: Option<marks::Leaving>,
    /// The shape the host announced at its last hello.
    pub(super) claim: Option<Shape>,
    /// The load of the host's latest accepted reading, see [`HostView::load`].
    pub(super) load: load::Kept,
}

impl Record {
    /// A host put back after a restart. The taint and the cordon a binding carries are the
    /// host's own, and the binding kept is the bare one, so each has a single home.
    pub(super) fn awaiting(binding: Option<Binding>, seated: Timestamp) -> Self {
        let taint = binding.as_ref().and_then(|bound| bound.taint().cloned());
        let cordon = binding
            .as_ref()
            .and_then(Binding::cordon_until_ms)
            .map(Timestamp::from_millis);
        Self {
            binding: binding.map(|bound| bound.with_taint(None).with_cordon(None)),
            taint,
            cordon,
            hello_at: None,
            runtime: None,
            epoch: 0,
            session: None,
            phase: Phase::Quarantined,
            heard: None,
            seated: Some(seated),
            agent_version: None,
            labels: BTreeMap::new(),
            files: false,
            native: false,
            bundles: false,
            quota_default: None,
            held: BTreeSet::new(),
            beats: 0,
            exclusive: Exclusive::Free,
            bounds: Vec::new(),
            leaving: None,
            claim: None,
            load: load::Kept::default(),
        }
    }

    /// Whether the host lists no run and the server holds none live for it: nothing is
    /// left of the last lease for a new one to meet.
    pub(super) fn quiet(&self, runs: &Runs, host: &HostId) -> bool {
        [self.held.is_empty(), !runs.live_on(host)]
            .into_iter()
            .all(|ok| ok)
    }

    /// Whether a lease that shares the host may meet what the host holds: it is quiet, or
    /// everything it holds, listed by the host or kept live by `runs`, belongs to `leases`,
    /// the leases booked on it now ([`Runs::only_of`]).
    pub(super) fn clear_beside(&self, runs: &Runs, host: &HostId, leases: &[LeaseId]) -> bool {
        self.quiet(runs, host) || runs.only_of(host, &self.held, leases)
    }

    /// Whether the host is cordoned at `now`: its cordon has not lapsed.
    pub(super) fn cordoned(&self, now: Timestamp) -> bool {
        self.cordon.is_some_and(|until| until > now)
    }

    pub(super) fn state(&self, now: Timestamp, grace_ms: u64) -> HostState {
        match self.heard {
            None => HostState::Awaiting,
            Some(heard) => match now.millis().saturating_sub(heard.millis()) > grace_ms {
                true => HostState::Lost,
                false => HostState::Live,
            },
        }
    }

    /// The instant the host's silence passed the grace: when it was last heard or, for a host
    /// never heard since it was seated (awaiting), when it was seated, plus the grace; `None`
    /// while that has not passed by `now`, and for a host neither heard nor seated. A host
    /// heard since it was silent has a new last-heard, so its silence clears.
    pub(super) fn silent_since(&self, now: Timestamp, grace_ms: u64) -> Option<Timestamp> {
        let since = self
            .heard
            .or(self.seated)
            .map_or(now.millis(), |at| at.millis());
        let silent = now.millis().saturating_sub(since) > grace_ms;
        silent.then(|| Timestamp::from_millis(since.saturating_add(grace_ms)))
    }

    /// Whether a new lease may be placed here: heard from within the grace period,
    /// serving, `quiet` (no run left over from the last lease), not tainted, not cordoned,
    /// clear of a whole-node lease (not held, not restoring) and not being scaled away. Every
    /// other phase takes no new lease. A lease already on a tainted host is untouched.
    pub(super) fn bookable(&self, now: Timestamp, grace_ms: u64, quiet: bool) -> bool {
        self.bookable_as(now, grace_ms, quiet, false)
    }

    /// [`Record::bookable`], with the cordon left out when `probe` says so: a probe the
    /// placer admitted to this host ([`crate::Placer::admit_probe`]) passes its cordon, and
    /// nothing else.
    pub(super) fn bookable_as(
        &self,
        now: Timestamp,
        grace_ms: u64,
        quiet: bool,
        probe: bool,
    ) -> bool {
        let live = self.state(now, grace_ms) == HostState::Live;
        let serving = self.phase == Phase::Serving;
        let clean = self.taint.is_none();
        let open = probe || !self.cordoned(now);
        let clear = self.exclusive.is_free();
        let staying = self.leaving.is_none();
        [live, serving, quiet, clean, open, clear, staying]
            .into_iter()
            .all(|ok| ok)
    }

    /// Whether the host has been silent for longer than the grace period and the
    /// retention together, counted from when it was last heard or, for a host never heard
    /// (awaiting), from when it was seated. A tainted host is never out of retention: it
    /// keeps its place, and its taint, until an operator deals with it. A cordoned host is
    /// not either, until its cordon lapses.
    pub(super) fn out_of_retention(
        &self,
        now: Timestamp,
        grace_ms: u64,
        retention_ms: u64,
    ) -> bool {
        let kept_for = grace_ms.saturating_add(retention_ms);
        let silent = self
            .heard
            .or(self.seated)
            .is_some_and(|since| now.millis().saturating_sub(since.millis()) > kept_for);
        [silent, self.taint.is_none(), !self.cordoned(now)]
            .into_iter()
            .all(|ok| ok)
    }
}
