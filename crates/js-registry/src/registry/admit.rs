//! Admitting a hello in two steps: [`Registry::judge_hello`] makes every check and draws the
//! session secret and changes nothing; [`Registry::take_hello`] records the admission and
//! cannot refuse. [`Registry::hello`] is the two together.

use super::*;

/// What the judge found of an admissible hello that does not change before the hello is
/// recorded.
struct Judged {
    host: HostId,
    protocol: u32,
    pool: usize,
    claim: Option<Shape>,
    rebinding: bool,
}

/// What the registry keeps of a host's last hello, which the record of the next one carries
/// over. Read from the registry when the record is made, so a taint, a cordon or a mark set
/// since the judgement is kept.
struct Kept {
    binding: Option<Binding>,
    taint: Option<Taint>,
    cordon: Option<Timestamp>,
    cordon_holder: Option<CordonHolder>,
    leaving: Option<marks::Leaving>,
    prior_epoch: Option<u64>,
}

impl Kept {
    /// What `state` keeps of `host`.
    fn of(state: &State, host: &HostId) -> Self {
        let known = state.records.get(host);
        let leaving = match state.ending.contains(host) {
            true => Some(marks::Leaving::Releasing),
            false => known.and_then(|record| record.leaving),
        };
        Self {
            binding: known.and_then(|record| record.binding.clone()),
            taint: known.and_then(|record| record.taint.clone()),
            cordon: known.and_then(|record| record.cordon),
            cordon_holder: known.and_then(|record| record.cordon_holder.clone()),
            leaving,
            prior_epoch: known.map(|record| record.epoch),
        }
    }
}

/// A hello the registry judged admissible, with the session secret drawn for it
/// ([`Registry::judge_hello`]), to be recorded by [`Registry::take_hello`], which cannot
/// refuse it. It holds no lock and changes nothing until it is taken; a caller that drops it
/// leaves the registry as it was (the secret drawn is simply never used).
#[derive(Debug)]
pub struct Admission {
    hello: Hello,
    credential: Credential,
    host: HostId,
    protocol: u32,
    pool: usize,
    claim: Option<Shape>,
    rebinding: bool,
    secret: String,
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
        let kept = Kept::of(state, &host);
        let kept_binding = kept.binding;
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
        if ![kept.prior_epoch.is_some(), room].into_iter().any(|ok| ok) {
            return Err(Refusal::PoolFull);
        }
        Ok(Judged {
            host,
            protocol,
            pool,
            claim,
            rebinding,
        })
    }

    /// Judges `hello` from `credential` as [`Registry::hello`] would, or, with `certified`
    /// set, as [`Registry::hello_as_certified_host`] would: every check it makes, in its
    /// order, with its refusals, and then the draw of the session secret (the refusal
    /// `session-unavailable` is given here, when the source of secrets gives none). Changes
    /// nothing. The admission it returns is recorded by [`Registry::take_hello`], which
    /// cannot refuse it, so a caller that records the hello elsewhere first judges, writes,
    /// and takes, and has nothing to undo when the hello is refused.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] the hello meets, as of the call.
    pub fn judge_hello(
        &self,
        hello: &Hello,
        credential: &Credential,
        certified: bool,
    ) -> Result<Admission, Refusal> {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        let judged = self.judge(hello, credential, certified, (&state, now))?;
        let secret = self
            .shared
            .sessions
            .fresh()
            .map(|bytes| secret_text(&bytes))
            .ok_or(Refusal::SessionUnavailable)?;
        let Judged {
            host,
            protocol,
            pool,
            claim,
            rebinding,
        } = judged;
        Ok(Admission {
            hello: hello.clone(),
            credential: credential.clone(),
            host,
            protocol,
            pool,
            claim,
            rebinding,
            secret,
        })
    }

    /// Records the hello `admission` judged ([`Registry::judge_hello`]) and answers it. It
    /// cannot refuse: it does not judge again, so a hello admitted is recorded even when the
    /// registry changed since the judgement (the pool filled, or the host went live). What
    /// the registry keeps of the host's last hello (its binding, taint, cordon and marks)
    /// is read when the record is made, so what was set in between is kept.
    ///
    /// The pool's cap of machines is judged per instance until the rows judge it (G5 of the
    /// multi-instance design), so a pool that filled between the two calls may hold one more
    /// host than its cap: no more than two hellos arriving at two instances at once can do
    /// now.
    pub fn take_hello(&self, admission: Admission) -> Welcome {
        let Admission {
            hello,
            credential,
            host,
            protocol,
            pool,
            claim,
            rebinding,
            secret,
        } = admission;
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let kept = Kept::of(&state, &host);
        let config = self.pool_at(pool);
        let epoch = kept.prior_epoch.unwrap_or(0).saturating_add(1);
        config.list_claimed(&mut state.book, host.clone(), claim.as_ref());
        let held = hello.held.iter().map(|run| run.run_id.clone()).collect();
        state.records.insert(
            host.clone(),
            Record {
                pool: Some(pool),
                binding: Some(match kept.binding {
                    Some(bound) if !rebinding => bound,
                    _ => Binding::new(credential, hello.instance.clone()),
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
                last_seq: None,
                taint: kept.taint,
                cordon: kept.cordon,
                cordon_holder: kept.cordon_holder,
                hello_at: Some(now),
                runtime: hello.capabilities.get(RUNTIME).cloned(),
                leaving: kept.leaving,
                claim,
                load: load::Kept::default(),
                agent_process: hello.agent_process.clone(),
            },
        );
        // The placer sees the host as the book lists it.
        let listed = state.book.shape_of(&host).cloned();
        listed.iter().for_each(|shape| self.tell_up(&host, shape));
        Welcome {
            protocol,
            epoch,
            heartbeat_interval_ms: config.heartbeat_interval_ms(),
            grace_ms: config.grace_ms(),
            pool: config.name().to_string(),
            session: secret,
            host_credential: None,
            dispatch_poll: false,
        }
    }
}
