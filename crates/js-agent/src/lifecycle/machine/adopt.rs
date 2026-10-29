//! A restart: taking over the containers the agent labelled, evicting the ones no job can
//! stand for, and trying again to remove the ones that could not be removed.

use std::cmp::Reverse;

use js_agent_wire::Taint;

use super::job::{
    Booking, Job, Retired, Stuck, cleanup_taint, host_taint, host_taint_of, label_id,
    may_change_host, slot_label,
};
use super::{Lifecycle, MAX_JOBS, SLOTS, Slots};
use crate::lifecycle::runtime::{ContainerRuntime, Found, RuntimeError, Status};
use crate::lifecycle::taint::{TaintState, reason};
use crate::lifecycle::{CheckHook, Clock, Config, Event, Finding, code, label};

/// The taint of a record of owed checks: the run it names, or the check code when the
/// records could not be listed.
fn owed_taint(run_id: Option<String>, check: Option<&str>) -> Taint {
    Taint {
        reason: reason::CHECK_OWED.to_owned(),
        run_id,
        check: check.map(str::to_owned),
    }
}

impl Lifecycle {
    /// Starts the lifecycle of an agent that has just started, taking over the labelled
    /// containers it finds so that they are held, first, from the very first heartbeat. A
    /// container that no job can stand for is stopped and removed by its own id, and
    /// recorded as a finding: one that was created and never started, one whose labels do
    /// not say what it booked, one that repeats
    /// a run id, one whose booking does not fit the machine's offer beside the containers
    /// taken over before it (a GPU not offered, or one already booked), and any beyond the
    /// number of runs a heartbeat can list. One that cannot be removed is not left
    /// unbooked: it holds what its labels say it booked (the whole offer when they do not
    /// say), and the stop and removal are tried again at every [`Lifecycle::step`] until
    /// the removal succeeds. It is not a held run, and it never costs a running job its
    /// place.
    ///
    /// The containers are taken over in the order of their age: those that have exited
    /// (their results are held) first, then the rest, the longest running first, whatever
    /// order the runtime lists them in. When the offer is lower than at the last start,
    /// the jobs that no longer fit are therefore the newest, and they are evicted.
    ///
    /// What the runtime lists is the agent's own ([`ContainerRuntime::list_labelled`] is
    /// scoped by the agent label); the listing removes nothing, so a container that was
    /// created and never started is evicted here like one whose labels are unreadable.
    ///
    /// A restart while a whole-node lease was ready: when the first reply that names a lease
    /// names one that a run taken over here belongs to, the hold starts ready (see
    /// [`Lifecycle::exclusive`]), because that run could only have been started once the
    /// host was found quiet. With no such run the hold is prepared for as for any grant.
    ///
    /// # Errors
    ///
    /// The runtime cannot list its containers. The caller must not say hello with an
    /// empty held list then: it does not know what it holds.
    pub fn adopt(
        config: Config,
        mut runtime: Box<dyn ContainerRuntime>,
        checks: Box<dyn CheckHook>,
        clock: Box<dyn Clock>,
    ) -> Result<(Self, Vec<Event>), RuntimeError> {
        let mut found: Vec<(bool, Found)> = runtime
            .list_labelled()?
            .into_iter()
            .map(|container| {
                let exited = matches!(runtime.poll(&container.id), Ok(Status::Exited(_)));
                (exited, container)
            })
            .collect();
        found.sort_by_key(|(exited, container)| (!exited, Reverse(container.running_ms)));
        let taint = TaintState::load(config.state.clone());
        let mut this = Self {
            config,
            runtime,
            checks,
            clock,
            jobs: Vec::new(),
            stuck: Vec::new(),
            retired: Retired::default(),
            slots: Slots::new(SLOTS),
            taint,
            exclusive: None,
            adopted_leases: Vec::new(),
        };
        let mut events = Vec::new();
        this.owed_at_start(&mut events);
        let mut stuck = Vec::new();
        for (_, container) in found {
            let now = this.clock.now_ms();
            match this.adoptable(&container, now) {
                Ok(mut job) => {
                    job.cgroup = this.runtime.cgroup(&container.id);
                    // A host job that outlived the agent changed the host, and a marker
                    // written when it started may be all that was lost.
                    if job.host {
                        job.taint(&mut this.taint, host_taint(&job.run_id), &mut events);
                    }
                    events.push(Event::Adopted {
                        run_id: job.run_id.clone(),
                    });
                    // Its slot is held until its cleanup passes, as for a run started here.
                    if let Some(slot) = job.slot {
                        this.slots.hold(slot);
                    }
                    this.jobs.push(job);
                }
                Err(why) => stuck.extend(this.evict(&container, why, &mut events)),
            }
        }
        // Booked only now, so that a container that cannot be removed never makes a
        // container that runs look like one that does not fit.
        // A container that is still there may still run a process of its slot's uid, and no
        // cleanup check will run for it: the slot is not used again.
        for slot in stuck.iter().filter_map(|held: &Stuck| held.slot) {
            this.slots.quarantine(slot);
        }
        this.stuck = stuck;
        this.adopted_leases = this.jobs.iter().map(|job| job.lease_id.clone()).collect();
        // A container that could not be removed may still use its run's directory.
        let kept = this.jobs.iter().map(|job| job.run_id.as_str());
        let held = this.stuck.iter().filter_map(|held| held.run_id.as_deref());
        let owned: Vec<&str> = kept.chain(held).collect();
        this.config.scratch.sweep(owned.iter().copied());
        if let Some(files) = &this.config.files {
            // A root that cannot be listed leaves earlier runs' secrets in place; say so.
            if let Err(why) = files.sweep(owned.iter().copied()) {
                events.push(Event::Finding(Finding {
                    run_id: None,
                    code: code::SCRATCH_FAILED,
                    message: format!("the run files root cannot be swept: {why}"),
                }));
            }
        }
        Ok((this, events))
    }

    /// Taints the host when the state root holds the record of a job whose checks were owed
    /// and never ran (the previous process stopped after the job's cleanup began and before
    /// its checks finished), or cannot be listed: what the job left behind may already be
    /// part of the baseline the new process takes.
    fn owed_at_start(&mut self, events: &mut Vec<Event>) {
        let owed = match self.config.state.owed() {
            Ok(runs) if runs.is_empty() => return,
            Ok(runs) => owed_taint(runs.first().cloned(), None),
            Err(_) => owed_taint(None, Some(code::OWED_UNREADABLE)),
        };
        self.seal(owed, events);
    }

    /// Taints the host with `taint` and writes the marker; a marker that cannot be written
    /// is a finding that belongs to no run.
    fn seal(&mut self, taint: Taint, events: &mut Vec<Event>) {
        if let Err(why) = self.taint.seal(taint, events) {
            events.push(Event::Finding(Finding {
                run_id: None,
                code: code::TAINT_WRITE_FAILED,
                message: why.to_string(),
            }));
        }
    }

    /// The job a labelled container stands for, or why it cannot be one: it never started,
    /// its labels do not say what it booked, its run id is held already, the agent holds as many runs as it
    /// may, or its booking does not fit what the machine offers beside the jobs taken over
    /// before it.
    fn adoptable(&self, found: &Found, now: u64) -> Result<Job, &'static str> {
        if found.never_started {
            return Err("it was created and never started");
        }
        let job = Job::adopted(found, now).ok_or("its labels do not say what it booked")?;
        let used = self.booked();
        let refusals = [
            (
                self.jobs.iter().any(|held| held.run_id == job.run_id),
                "its run id is held already",
            ),
            (
                job.slot.is_some_and(|slot| !self.slots.is_free(slot)),
                "its slot is held already",
            ),
            (
                self.jobs.len() >= MAX_JOBS,
                "the agent holds as many runs as a heartbeat can list",
            ),
            (
                !job.booking.fits(&self.config.capacity, &used),
                "its booking does not fit the machine beside the jobs taken over",
            ),
        ];
        match refusals.into_iter().find(|(refused, _)| *refused) {
            Some((_, why)) => Err(why),
            None => Ok(job),
        }
    }

    /// Stops and removes a container no job stands for, so that nothing runs on the host
    /// unbooked, and records it. A stop that fails does not keep the removal from being
    /// tried (removal takes a running container too); a removal that fails is the louder
    /// finding, because the container is then still there, and it is returned to be booked
    /// and tried again.
    fn evict(&mut self, found: &Found, why: &str, events: &mut Vec<Event>) -> Option<Stuck> {
        let id = &found.id;
        // A container that may be a host job's changed the host whatever becomes of it
        // now: its booking being wrong does not undo that, and it is not left to a
        // removal that may fail.
        if may_change_host(found) {
            let run = label_id(found, label::RUN);
            self.seal(host_taint_of(run), events);
        }
        let _ = self.runtime.stop(id, self.config.stop_grace);
        let (code, fate, stuck) = match self.runtime.remove(id) {
            Ok(()) => (code::UNADOPTABLE, "stopped and removed".to_owned(), None),
            Err(failed) => {
                // A container that stays on the host is a cleanup that failed.
                self.seal(cleanup_taint(None, code::UNADOPTABLE_KEPT), events);
                (
                    code::UNADOPTABLE_KEPT,
                    format!("not removed, still on the host and booked: {failed}"),
                    Some(Stuck {
                        run_id: label_id(found, label::RUN).map(str::to_owned),
                        slot: slot_label(found),
                        id: id.clone(),
                        booking: Booking::from_labels(found)
                            .unwrap_or_else(|| Booking::everything(&self.config.capacity)),
                    }),
                )
            }
        };
        events.push(Event::Finding(Finding {
            run_id: None,
            code,
            message: format!("container {}: {why}; {fate}", id.0),
        }));
        stuck
    }

    /// Tries again to stop and remove the containers that could not be removed; the
    /// booking of one that is gone is freed.
    pub(super) fn retry_stuck(&mut self, events: &mut Vec<Event>) {
        let Self {
            stuck,
            runtime,
            config,
            ..
        } = self;
        stuck.retain(|held| {
            let _ = runtime.stop(&held.id, config.stop_grace);
            let gone = runtime.remove(&held.id).is_ok();
            if gone {
                events.push(Event::Finding(Finding {
                    run_id: None,
                    code: code::UNADOPTABLE,
                    message: format!("container {}: removed on a later attempt", held.id.0),
                }));
            }
            !gone
        });
    }
}
