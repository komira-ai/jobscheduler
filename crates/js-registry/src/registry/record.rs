//! What the registry keeps for each host that said hello, and what it can say of it.

use super::*;

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
    /// The run ids the host listed as held in its last hello or heartbeat: runs of the
    /// previous release's lease protocol, each answered with a cancel.
    pub(super) held: BTreeSet<String>,
    /// The heartbeats taken in this session.
    pub(super) beats: u64,
    /// The first taint the host reported. It outlives a session and, through the file of
    /// bindings, a restart.
    pub(super) taint: Option<Taint>,
    /// When the host's cordon lapses, once it has been cordoned ([`Registry::cordon`]): it
    /// takes no new job until then. It outlives a session and, through the file of
    /// bindings, a restart; a lapsed value is a cordon that is over.
    pub(super) cordon: Option<Timestamp>,
    /// When the host's current session began: the time of the hello that began it. `None`
    /// for a host that has not said hello since a restart. A heartbeat does not move it.
    pub(super) hello_at: Option<Timestamp>,
    /// The `runtime` capability of the host's last hello. Kept like `files`.
    pub(super) runtime: Option<String>,
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
            held: BTreeSet::new(),
            beats: 0,
            leaving: None,
            claim: None,
            load: load::Kept::default(),
        }
    }

    /// Whether the host lists no run: nothing is left of the previous release's work for a
    /// scale-down mark to cut.
    pub(super) fn quiet(&self) -> bool {
        self.held.is_empty()
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
