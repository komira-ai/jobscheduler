//! Admitting a hello: the judge that makes every check, shared by [`Registry::hello`] (which
//! then records the session) and [`Registry::check_hello`] (which records nothing), so the
//! two cannot answer differently for one state.

use super::*;

/// What the judge found of an admissible hello: what the record it makes keeps of the host's
/// last one.
struct Judged {
    host: HostId,
    protocol: u32,
    pool: usize,
    claim: Option<Shape>,
    kept_binding: Option<Binding>,
    kept_taint: Option<Taint>,
    kept_cordon: Option<Timestamp>,
    kept_cordon_holder: Option<CordonHolder>,
    kept_leaving: Option<marks::Leaving>,
    rebinding: bool,
    prior_epoch: Option<u64>,
}

impl Registry {
    /// Every check an admitted hello passes, in the order they are made, against `state` as
    /// of `now`. Changes nothing.
    fn judge(
        &self,
        hello: &Hello,
        credential: &Credential,
        rebind: bool,
        (state, now): (&State, Timestamp),
    ) -> Result<Judged, Refusal> {
        let host = HostId::new(&hello.host_id).map_err(Refusal::BadHostId)?;
        let protocol = hello.negotiate().map_err(Refusal::Invalid)?;
        hello.validate().map_err(Refusal::Invalid)?;
        let pool = self.pool_named(&hello.pool).ok_or(Refusal::UnknownPool)?;
        let config = self.pool_at(pool);
        if state.reserved.contains(&host) {
            return Err(Refusal::HostListed);
        }
        let known = state.records.get(&host);
        // A host put back after a restart is in no pool until it says hello: its binding
        // says who may.
        if known.is_some_and(|record| record.pool.is_some_and(|held| held != pool)) {
            return Err(Refusal::HostListed);
        }
        let kept_binding = known.and_then(|record| record.binding.clone());
        let kept_taint = known.and_then(|record| record.taint.clone());
        let kept_cordon = known.and_then(|record| record.cordon);
        let kept_cordon_holder = known.and_then(|record| record.cordon_holder.clone());
        let kept_leaving = known.and_then(|record| record.leaving);
        let kept_leaving = match state.ending.contains(&host) {
            true => Some(marks::Leaving::Releasing),
            false => kept_leaving,
        };
        let instance = hello.instance.as_ref();
        let rebinding = rebind
            && kept_binding
                .as_ref()
                .is_some_and(|bound| bound.credential() != credential);
        let bound_elsewhere = kept_binding
            .as_ref()
            .is_some_and(|bound| !bound.admits(credential, instance));
        if bound_elsewhere && !rebinding {
            return Err(Refusal::HostBound);
        }
        // The one process that lost its session says hello again: no other process holds
        // the host, so the live session is its own and is replaced.
        let live = known.is_some_and(|record| {
            let same_process =
                hello.agent_process.is_some() && hello.agent_process == record.agent_process;
            let live = record.state(now, self.pool_of(record).grace_ms()) == HostState::Live;
            live && !same_process
        });
        if live {
            return Err(Refusal::HostLive);
        }
        let prior_epoch = known.map(|record| record.epoch);
        let floor = config.shape();
        if let Some(exec) = config.exec() {
            let contract = HostContract::new(exec, floor.resources().gpus() > 0);
            contract.check(&hello.capabilities)?;
        }
        let claim = hello.to_shape().ok();
        let offers_floor = claim
            .as_ref()
            .is_some_and(|claim| claim.satisfies(floor.resources(), floor.capabilities()));
        if !offers_floor {
            return Err(Refusal::ShapeNotAccepted(ShapeFault::BelowFloor));
        }
        // A pool that names `scratch-quota` books a default for every job without disk, so
        // a host must report one that counts and fits the pool's disk: otherwise its jobs
        // without disk would book nothing, or never fit.
        let quota_pool = floor.capabilities().contains_key(SCRATCH_QUOTA);
        let default = quota_default(&hello.capabilities);
        let fits = default.is_some_and(|bytes| bytes <= floor.resources().disk_bytes());
        if [quota_pool, !fits].into_iter().all(|ok| ok) {
            return Err(Refusal::ShapeNotAccepted(ShapeFault::QuotaDefaultUnfit));
        }
        // A host that enforces a quota books a default for every job without disk, so a
        // pool that books no disk (and does not name `scratch-quota`) would count 0 for
        // jobs the host counts at its default, and the server would admit what the agent
        // refuses. Such a host is refused rather than booked wrongly.
        let books_no_disk = floor.resources().disk_bytes() == 0;
        if [!quota_pool, books_no_disk, default.is_some()]
            .into_iter()
            .all(|ok| ok)
        {
            return Err(Refusal::ShapeNotAccepted(ShapeFault::QuotaWithoutPoolDisk));
        }
        let held = state.records.values();
        // A host put back after a restart counts as the first pool's, as it is listed.
        let in_pool = held
            .filter(|record| record.pool.unwrap_or(0) == pool)
            .count();
        let room = in_pool < config.max_machines();
        if ![prior_epoch.is_some(), room].into_iter().any(|ok| ok) {
            return Err(Refusal::PoolFull);
        }
        Ok(Judged {
            host,
            protocol,
            pool,
            claim,
            kept_binding,
            kept_taint,
            kept_cordon,
            kept_cordon_holder,
            kept_leaving,
            rebinding,
            prior_epoch,
        })
    }

    /// Whether [`Registry::hello`] would admit `hello` from `credential`, or, with
    /// `certified` set, [`Registry::hello_as_certified_host`] would: every check it makes,
    /// in its order, with its refusals, changing nothing and issuing no secret. A caller
    /// that records the hello elsewhere first uses it to refuse before it writes anything.
    ///
    /// The one refusal it cannot give is `session-unavailable`, which comes of the source of
    /// secrets when one is asked for. The answer is as of the call; a call in between can
    /// change what the hello then does.
    pub fn check_hello(
        &self,
        hello: &Hello,
        credential: &Credential,
        certified: bool,
    ) -> Result<(), Refusal> {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        self.judge(hello, credential, certified, (&state, now))
            .map(|_| ())
    }

    /// [`Registry::hello`], binding the host to `credential` over another credential's
    /// binding when `rebind` is set and the host is not live.
    pub(super) fn admit(
        &self,
        hello: &Hello,
        credential: &Credential,
        rebind: bool,
    ) -> Result<Welcome, Refusal> {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let judged = self.judge(hello, credential, rebind, (&state, now))?;
        let Judged {
            host,
            protocol,
            pool,
            claim,
            kept_binding,
            kept_taint,
            kept_cordon,
            kept_cordon_holder,
            kept_leaving,
            rebinding,
            prior_epoch,
        } = judged;
        let config = self.pool_at(pool);
        let epoch = prior_epoch.unwrap_or(0).saturating_add(1);
        let secret = self
            .shared
            .sessions
            .fresh()
            .map(|bytes| secret_text(&bytes))
            .ok_or(Refusal::SessionUnavailable)?;
        config.list_claimed(&mut state.book, host.clone(), claim.as_ref());
        let held = hello.held.iter().map(|run| run.run_id.clone()).collect();
        state.records.insert(
            host.clone(),
            Record {
                pool: Some(pool),
                binding: Some(match kept_binding {
                    Some(kept) if !rebinding => kept,
                    _ => Binding::new(credential.clone(), hello.instance.clone()),
                }),
                epoch,
                session: Some(SessionDigest::of(&secret)),
                phase: hello.phase,
                heard: Some(now),
                seated: None,
                agent_version: Some(hello.agent_version.clone()),
                labels: hello.labels.clone(),
                held,
                beats: 0,
                taint: kept_taint,
                cordon: kept_cordon,
                cordon_holder: kept_cordon_holder,
                hello_at: Some(now),
                runtime: hello.capabilities.get(RUNTIME).cloned(),
                leaving: kept_leaving,
                claim,
                load: load::Kept::default(),
                agent_process: hello.agent_process.clone(),
            },
        );
        // The placer sees the host as the book lists it.
        let listed = state.book.shape_of(&host).cloned();
        listed.iter().for_each(|shape| self.tell_up(&host, shape));
        Ok(Welcome {
            protocol,
            epoch,
            heartbeat_interval_ms: config.heartbeat_interval_ms(),
            grace_ms: config.grace_ms(),
            pool: config.name().to_string(),
            session: secret,
            host_credential: None,
        })
    }
}
