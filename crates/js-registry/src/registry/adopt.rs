//! The registry as a cache of the store's `node/<host>` rows: a session another server's
//! hello wrote is adopted ([`Registry::adopt`]), and a hello admitted here is numbered by the
//! store ([`Registry::renumber`]) or given up when the store could not record it
//! ([`Registry::abandon`]).

use std::time::Duration;

use super::*;

/// A session another server's hello wrote, as a caller read it from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSession {
    /// The epoch of the hello that began the session.
    pub epoch: u64,
    /// The digest of the session secret ([`crate::session_digest`]); `None` when the row
    /// keeps none, and then no heartbeat matches the session.
    pub session: Option<[u8; 32]>,
    /// The name of the pool the host joined.
    pub pool: String,
    /// The shape the host announced.
    pub claim: Option<Shape>,
    /// The labels the host announced.
    pub labels: BTreeMap<String, String>,
    /// The phase the host last announced.
    pub phase: Phase,
    /// The agent process id of the hello, when it sent one.
    pub agent_process: Option<String>,
    /// How long before now the row was last heard, by the store's clock.
    pub silent_for: Duration,
    /// The binding the row keeps, used when the registry holds none for the host.
    pub binding: Option<Binding>,
}

/// What [`Registry::adopt`] did with a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopted {
    /// The session is now the registry's record of the host.
    Installed,
    /// The registry holds this epoch or a newer one: nothing changed.
    Kept,
    /// The session names a pool the registry has not: nothing changed.
    UnknownPool,
}

impl Registry {
    /// How long a host of the pool named `pool` is kept after it was last heard before it is
    /// forgotten: the pool's grace and its lost retention. `None` for a pool the registry does
    /// not have. A caller that holds when another server last heard a host compares with this,
    /// as the registry does with its own record.
    #[must_use]
    pub fn kept_for(&self, pool: &str) -> Option<Duration> {
        let config = self.pool_at(self.pool_named(pool)?);
        let kept = config.grace_ms().saturating_add(config.lost_retention_ms());
        Some(Duration::from_millis(kept))
    }

    /// Takes the session of `host` that another server's hello wrote into the registry, as
    /// if its hello had been admitted here, when the registry holds no record of the host,
    /// holds one awaiting its first hello, or holds one of an older epoch than
    /// `node.epoch`; the record keeps its taint, cordon, holder, mark and binding (the
    /// binding of `node` when it has none), and the host is listed in the book and told to
    /// the placer as a hello does. The host counts as heard `node.silent_for` ago.
    ///
    /// No admission check runs: the row is the decision another server already made. A
    /// record in session at the same or a newer epoch is kept as it is ([`Adopted::Kept`]), so
    /// a live session in the cache never moves back; a record with no session (awaiting, or
    /// abandoned because the store could not record its hello) takes the row's session
    /// whatever its epoch, since the row is then the only session there is. A pool the
    /// registry does not have changes nothing ([`Adopted::UnknownPool`]).
    pub fn adopt(&self, host: &HostId, node: NodeSession) -> Adopted {
        let Some(pool) = self.pool_named(&node.pool) else {
            return Adopted::UnknownPool;
        };
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let known = state.records.get(host);
        let newer =
            known.is_none_or(|record| record.session.is_none() || record.epoch < node.epoch);
        if !newer {
            return Adopted::Kept;
        }
        let silent = u64::try_from(node.silent_for.as_millis()).unwrap_or(u64::MAX);
        let heard = Timestamp::from_millis(now.millis().saturating_sub(silent));
        let leaving = match state.ending.contains(host) {
            true => Some(marks::Leaving::Releasing),
            false => known.and_then(|record| record.leaving),
        };
        let binding = known.and_then(|record| record.binding.clone());
        let taint = known.and_then(|record| record.taint.clone());
        let cordon = known.and_then(|record| record.cordon);
        let cordon_holder = known.and_then(|record| record.cordon_holder.clone());
        self.pool_at(pool)
            .list_claimed(&mut state.book, host.clone(), node.claim.as_ref());
        let runtime = node
            .claim
            .as_ref()
            .and_then(|claim| claim.capabilities().get(RUNTIME).cloned());
        state.records.insert(
            host.clone(),
            Record {
                pool: Some(pool),
                binding: binding.or(node.binding),
                epoch: node.epoch,
                session: node.session.map(SessionDigest::from_bytes),
                phase: node.phase,
                heard: Some(heard),
                seated: None,
                agent_version: None,
                labels: node.labels,
                held: BTreeSet::new(),
                beats: 0,
                last_seq: None,
                taint,
                cordon,
                cordon_holder,
                hello_at: Some(heard),
                runtime,
                leaving,
                claim: node.claim,
                load: load::Kept::default(),
                agent_process: node.agent_process,
            },
        );
        let listed = state.book.shape_of(host).cloned();
        listed.iter().for_each(|shape| self.tell_up(host, shape));
        Adopted::Installed
    }

    /// Gives the record of `host` whose epoch is `from` the epoch `to`: a hello admitted
    /// here whose row the store numbered. `false`, and nothing changed, when the registry
    /// holds no record of the host at `from`.
    pub fn renumber(&self, host: &HostId, from: u64, to: u64) -> bool {
        let mut state = self.lock();
        match state
            .records
            .get_mut(host)
            .filter(|record| record.epoch == from)
        {
            Some(record) => {
                record.epoch = to;
                true
            }
            None => false,
        }
    }

    /// Ends the session of `host` when the registry holds it in session epoch `epoch`, as if
    /// the host had not said hello since a restart: its beats are refused as an unknown
    /// host, a hello for it is not refused as live, and its retention counts from now. Its
    /// taint, cordon and binding are kept. It is for a hello the registry admitted and the
    /// store could not record. `false`, and nothing changed, otherwise.
    pub fn abandon(&self, host: &HostId, epoch: u64) -> bool {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let held = state
            .records
            .get_mut(host)
            .filter(|record| record.epoch == epoch && record.session.is_some());
        match held {
            Some(record) => {
                record.session = None;
                record.heard = None;
                record.seated = Some(now);
                true
            }
            None => false,
        }
    }
}
