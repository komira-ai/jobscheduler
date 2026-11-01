//! What the registry keeps for each host that said hello, and what it can say of it.

use super::*;

/// A lease booked on a host, the latest it can end (its start plus its max wall time) and
/// when it started, in milliseconds on the registry's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Bound {
    pub(super) lease: LeaseId,
    pub(super) end: Timestamp,
    pub(super) started_at: u64,
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
    /// and when it started. A lease put back after a restart has none, and counts as ending
    /// now. Held in memory only; a lease that is no longer booked on the host is dropped at
    /// the next start.
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

    /// Whether a new lease may be placed here: heard from within the grace period,
    /// serving, `quiet` (no run left over from the last lease), not tainted, not cordoned,
    /// clear of a whole-node lease (not held, not restoring) and not being scaled away. Every
    /// other phase takes no new lease. A lease already on a tainted host is untouched.
    pub(super) fn bookable(&self, now: Timestamp, grace_ms: u64, quiet: bool) -> bool {
        let live = self.state(now, grace_ms) == HostState::Live;
        let serving = self.phase == Phase::Serving;
        let clean = self.taint.is_none();
        let open = !self.cordoned(now);
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
