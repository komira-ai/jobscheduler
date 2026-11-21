//! A job's end: watching a running container, and taking a finished job through the
//! cleanup in order. The container's removal and the removal of the run's directories run off
//! the worker when the runtime ([`Runtime::removing`]) and the lifecycle
//! ([`Lifecycle::with_cleanup_threads`]) can: the job waits in its stage until they answer,
//! and every other job, a start among them, goes on meanwhile.

use std::path::{Path, PathBuf};

use super::Lifecycle;
use super::job::{
    Cause, Job, Phase, Stage, cause_detail, cgroup_not_found_taint, check_taint, cleanup_taint,
    input_failure, outcome,
};
use super::run_dirs::{DirsRemoval, RunDirs};
use crate::lifecycle::runtime::{Removal, RunHandle, Runtime, Status, Whereabouts};
use crate::lifecycle::{CheckFinding, Event, FilesRoot, Finding, Finished, ScratchRoot, code};

/// How often a finished run's usage that stays nonzero is reported again while the agent
/// waits for it (one hour).
const REMIND_HELD_MS: u64 = 3_600_000;

impl Lifecycle {
    /// Watches one running job: the wall-time limit, a stop that is owed, the exit.
    pub(super) fn supervise(&mut self, index: usize, events: &mut Vec<Event>) {
        let now = self.clock.now_ms();
        let Self {
            jobs,
            runtime,
            config,
            taint,
            blind_wait,
            ..
        } = self;
        let blind_wait_ms = u64::try_from(blind_wait.as_millis()).unwrap_or(u64::MAX);
        let job = &mut jobs[index];
        let Stage::Running(id) = job.stage.clone() else {
            return;
        };
        // The exit is looked at first: a job that finished by itself before its limit is
        // reported as it finished, even when the agent looks late. A runtime that cannot
        // say is no reason to let the job run past its limit. A container that is gone has
        // nothing to stop, and the job ends as if it had exited.
        let mut seen = runtime.poll(&id);
        // A run whose end is held while the runtime checks its inputs has ended by itself:
        // neither its limit nor a stop is its end.
        if !matches!(seen, Ok(Status::Exited(_))) && !runtime.ends_held(&id) {
            if job.cause.is_none() && now >= job.deadline_ms {
                job.cause = Some(Cause::TimedOut);
            }
            let gone = matches!(seen, Ok(Status::Gone));
            if job.cause.is_some() && !job.stop_sent && !gone {
                match runtime.stop(&id, config.stop_grace) {
                    Ok(()) => {
                        job.stop_sent = true;
                        seen = runtime.poll(&id);
                    }
                    Err(why) => job.note(code::STOP_FAILED, why.to_string(), events),
                }
            }
        }
        match seen {
            Ok(Status::Running) => {
                // A runtime that puts every container in a group the agent can read, and
                // cannot name this running container's group (not at the start, not at a
                // window, not now), may leave the agent blind to it. A group that is missing
                // is not blindness by itself: the run is running from the moment its
                // supervisor is ready, before the container (and its group) is made, and it
                // is still read as running for a moment after the container ended and its
                // group was removed with it. A run seen only after it ended has no group to
                // look at (`cgroup` says `None` for it), and one that ended between the poll
                // and the look is read as ended by the next step.
                //
                // A run whose end the runtime holds (a failed run whose sources are being
                // checked) has ended, though `poll` says running: its group is gone, and no
                // number of steps makes that blindness.
                if runtime.places_in_cgroup()
                    && job.cgroup.is_none()
                    && !job.blind_told
                    && !runtime.ends_held(&id)
                {
                    job.cgroup = runtime.cgroup(&id);
                    // Steps come at once after one another (every order nudges the worker),
                    // so the count of steps says nothing: the runtime is asked where the
                    // container is only once the run has been without a group for a settle
                    // time (`--settle-ms`) after the first sighting, and the host is blind
                    // to it only when its first process runs outside its group. A container
                    // that is not there (not made yet, or ended) is asked about again a
                    // settle time later.
                    if job.cgroup.is_none() {
                        let since = *job.blind_since.get_or_insert(now);
                        if now.saturating_sub(since) >= blind_wait_ms {
                            match runtime.whereabouts(&id) {
                                Whereabouts::Grouped(dir) => job.cgroup = Some(dir),
                                Whereabouts::NoProcess => job.blind_since = Some(now),
                                Whereabouts::Elsewhere(group) => {
                                    job.blind_told = true;
                                    job.taint(taint, cgroup_not_found_taint(&job.run_id), events);
                                    let message = format!(
                                        "run {}: its container group was not found within the \
                                         settle wait of {blind_wait_ms} ms, and its first process \
                                         runs in {group}",
                                        job.run_id
                                    );
                                    job.note(code::CGROUP_NOT_FOUND, message, events);
                                }
                            }
                        }
                    }
                }
            }
            Ok(ended @ (Status::Exited(_) | Status::Gone)) => {
                let exit = match ended {
                    Status::Exited(exit) => Some(exit),
                    _ => None,
                };
                // What ended the job is what happened, not what the agent meant to do: an
                // exit that came before any stop was sent is the job's own, whether the
                // stop was merely asked for (a `Stop` applied since the last step) or
                // failed (the runtime refused it). A container that is gone has nothing to
                // tell, so the cause stands.
                let cause = job.cause.filter(|_| exit.is_none() || job.stop_sent);
                let (how, status) = outcome(cause, exit);
                // A result read back from the state root is the one to send: the exit of a
                // container taken over after the agent had reported it makes no second.
                if !job.silent && job.result.is_none() {
                    let mut result = job.report(how, status, cause_detail(cause));
                    // The container is still there: it is removed in the next stage. A log
                    // that cannot be read costs the result its tail and nothing else. What the
                    // capture saw of the streams is read beside it; it never waits for the
                    // upload of live logs, which goes on after the cleanup.
                    if exit.is_some() {
                        match runtime.log_tail(&id) {
                            Ok(tail) => result.log_tail = tail,
                            Err(why) => job.note(code::LOGS_FAILED, why.to_string(), events),
                        }
                        job.logs = runtime.logs_report(&id);
                        job.usage = runtime.usage(&id);
                    }
                    job.result = Some(result);
                    job.input_failure = input_failure(job.cause, how, runtime.input_failure(&id));
                    events.push(Event::ResultReady {
                        run_id: job.run_id.clone(),
                    });
                }
                job.stage = Stage::Removing(id);
            }
            Err(why) => job.note(code::POLL_FAILED, why.to_string(), events),
        }
    }

    /// Takes one finished job through the cleanup, in order, as far as it goes
    /// ([`Lifecycle::clean_pass`]), and keeps when each step was done and how long the worker
    /// spent on it; once the slot is free, [`Event::Cleaned`] carries that.
    pub(super) fn clean(&mut self, index: usize, events: &mut Vec<Event>) {
        if !self.jobs[index].stage.cleaning() {
            self.clean_pass(index, events);
            return;
        }
        let began = self.clock.now_ms();
        let seen = events.len();
        self.clean_pass(index, events);
        let now = self.clock.now_ms();
        let job = &mut self.jobs[index];
        let since = *job.cleaning.since_ms.get_or_insert(began);
        let timeline = &mut job.cleaning.timeline;
        timeline.worker_ms = timeline.worker_ms.saturating_add(now.saturating_sub(began));
        let after = now.saturating_sub(since);
        for event in &events[seen..] {
            let step = match event {
                Event::ContainerRemoved { .. } => "removed",
                Event::ScratchRemoved { .. } => "scratch",
                Event::ChecksPassed { .. } => "checks",
                _ => continue,
            };
            timeline.steps.push((step, after));
        }
        if job.stage == Stage::Freed {
            events.push(Event::Cleaned {
                run_id: job.run_id.clone(),
                timeline: std::mem::take(timeline),
            });
        }
    }

    /// Takes one finished job through the cleanup, in order, as far as it goes: the
    /// container, the scratch directory, the checks, then the slot. A step that fails
    /// ends the pass with the slot still booked, and so does a removal still under way off
    /// the worker, whose answer a later step takes.
    fn clean_pass(&mut self, index: usize, events: &mut Vec<Event>) {
        // A run that waits to start is cleaned once it ends (its stage holds the start).
        if matches!(self.jobs[index].stage, Stage::Preparing(_)) {
            return;
        }
        // The containers of the other jobs still on the host: those the lifecycle holds, and
        // those whose create is under way off the worker, which may have made the run's
        // group and started its processes before the lifecycle takes the answer (a busy
        // host's runs overlap so, and they all run as the one job user under `crun`).
        let runtime = &*self.runtime;
        let on_host = |job: &Job| match &job.stage {
            Stage::Preparing(waiting) if waiting.phase == Phase::Creating => {
                runtime.creating(&job.run_id)
            }
            stage => stage.container().cloned(),
        };
        let others = || {
            self.jobs
                .iter()
                .enumerate()
                .filter(move |(other, _)| *other != index)
                .filter_map(|(_, job)| on_host(job).map(|id| (id, job.slot)))
        };
        let live: Vec<RunHandle> = others()
            .map(|(id, _)| id)
            .chain(self.stuck.iter().map(|held| held.id.clone()))
            .collect();
        // The slots of the other jobs whose containers are still on the host.
        let live_slots: Vec<u32> = others()
            .filter_map(|(_, slot)| slot)
            .chain(self.stuck.iter().filter_map(|held| held.slot))
            .collect();
        // Where the runtime mounts the file systems of the other jobs it still holds, a job
        // that waits for its assembly or for its cleanup among them.
        let held_mounts: Vec<std::path::PathBuf> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(other, job)| *other != index && job.stage != Stage::Freed)
            .filter_map(|(_, job)| self.runtime.mounts_of(&job.run_id))
            .collect();
        let Self {
            jobs,
            runtime,
            config,
            checks,
            taint,
            slots,
            quota_health,
            clock,
            providers,
            run_dirs,
            ..
        } = self;
        let job = &mut jobs[index];
        let run_id = job.run_id.clone();
        let lease = runtime.lease_dir(&run_id);
        let tmp = runtime.tmp_dir(&run_id);
        loop {
            let stage = job.stage.clone();
            // The checks are owed from before the first step of the cleanup until they have
            // run: an agent that stops in between leaves the record, and the next start
            // taints the host. Without the record nothing could be removed. It is written on
            // the worker, before any removal is asked of a thread.
            if stage.cleaning() && !job.owed {
                match config.state.owe(&run_id) {
                    Ok(()) => job.owed = true,
                    Err(why) => {
                        job.note(code::OWED_FAILED, why.to_string(), events);
                        return;
                    }
                }
            }
            match stage {
                // Asked of the runtime at every step until it answers: off the worker when the
                // runtime can, and then the job waits here with its slot booked.
                Stage::Removing(id) => match runtime.removing(&id) {
                    Removal::Pending => return,
                    Removal::Done(Ok(())) => {
                        events.push(Event::ContainerRemoved {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Scratch;
                    }
                    // Everything of the run is gone and only the copy of an output failed (a
                    // link where an output was declared, an output over its cap): the host
                    // holds nothing of the job, so it is a finding and not a taint, and the
                    // cleanup goes on. Only the native runtime (a macOS build's) copies outputs.
                    #[cfg(target_os = "macos")]
                    Removal::Done(Err(why)) if why.is_outputs() => {
                        job.note(code::OUTPUTS_FAILED, why.to_string(), events);
                        events.push(Event::ContainerRemoved {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Scratch;
                    }
                    Removal::Done(Err(why)) => {
                        let tainted = cleanup_taint(Some(&run_id), code::CONTAINER_REMOVE_FAILED);
                        job.taint(taint, tainted, events);
                        job.note(code::CONTAINER_REMOVE_FAILED, why.to_string(), events);
                        return;
                    }
                },
                // The run's directories, off the worker when the lifecycle has threads for it
                // (see `dirs_removal`); the job waits here until they are gone.
                Stage::Scratch => match dirs_removal(
                    (job.cleaning.dirs_removed, run_id.as_str()),
                    (runtime.as_mut(), run_dirs),
                    (&config.scratch, config.files.as_ref()),
                    [&lease, &tmp],
                ) {
                    DirsRemoval::Pending => return,
                    DirsRemoval::Done(Ok(())) => {
                        job.cleaning.dirs_removed = true;
                        // What the run's providers made for it is given up now that nothing
                        // mounts it. A failed release is tried at every step (`provided`)
                        // and never taints the host or holds the slot.
                        providers.leave(&run_id, clock.now_ms());
                        // The tree is gone; now the bound comes off its project, and the
                        // slot stays held until the project reads no usage (the file system
                        // frees the inodes of a removed tree in the background, and a run
                        // that took the slot sooner would be counted with them). The booked
                        // bytes stay held with it: the server counts them as free once the
                        // run has ended, so holding them a little longer only makes the
                        // agent refuse later than the server admits for the seconds the
                        // lag lasts, the same as for every other booked dimension, which
                        // are held until this cleanup has passed. A failure is tried again
                        // at the next step; the directories, which are gone, are not removed
                        // again. None of it taints: nothing is left on disk.
                        if let (Some(quota), Some(slot)) = (&config.quota, job.slot) {
                            let dir = config.scratch.of(&run_id);
                            // A runtime that gives no temporary directory leaves the slot's
                            // temporary project unbound; its limits are taken off all the same.
                            let gone = tmp.clone().unwrap_or_else(|| config.scratch.vanished());
                            let now = clock.now_ms();
                            let health = config.scratch.health_dir();
                            if let Err(why) = quota.clear(&dir, &gone, slot) {
                                quota_health.failed(quota, now, &health, events);
                                job.note(code::QUOTA_CLEAR_FAILED, why.to_string(), events);
                                return;
                            }
                            match quota.usage(slot) {
                                Ok(used) if used.is_zero() => quota_health.ok(),
                                Ok(used) => {
                                    let held = format!(
                                        "the project still holds {} bytes in {} inodes",
                                        used.bytes, used.inodes
                                    );
                                    let (since, reported) =
                                        *job.usage_held.get_or_insert((now, now));
                                    if now.saturating_sub(since) < quota.usage_wait_ms() {
                                        job.note(code::QUOTA_USAGE_HELD, held.clone(), events);
                                        if now.saturating_sub(reported) >= REMIND_HELD_MS {
                                            job.usage_held = Some((since, now));
                                            events.push(Event::Finding(Finding {
                                                run_id: Some(run_id.clone()),
                                                code: code::QUOTA_USAGE_HELD,
                                                message: held,
                                            }));
                                        }
                                        return;
                                    }
                                    // The usage never reached zero in the time the agent waits
                                    // (a file held open outside the container, accounting that
                                    // drifted): the slot is parked instead of held for good, the
                                    // cleanup goes on, so the run's place and bytes are given
                                    // back, and the quota counts a failure.
                                    let gave_up = format!(
                                        "{held} {} seconds after its tree was removed: the slot is \
                                         parked and the cleanup goes on",
                                        quota.usage_wait_ms() / 1000
                                    );
                                    job.note(code::QUOTA_USAGE_ABANDONED, gave_up, events);
                                    job.slot_parked = true;
                                    quota_health.failed(quota, now, &health, events);
                                    quota_health.abandoned(quota, now, events);
                                }
                                Err(why) => {
                                    quota_health.failed(quota, now, &health, events);
                                    job.note(code::QUOTA_CLEAR_FAILED, why.to_string(), events);
                                    return;
                                }
                            }
                        }
                        events.push(Event::ScratchRemoved {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Checks;
                    }
                    DirsRemoval::Done(Err(why)) => {
                        let tainted = cleanup_taint(Some(&run_id), code::SCRATCH_FAILED);
                        job.taint(taint, tainted, events);
                        job.note(code::SCRATCH_FAILED, why.to_string(), events);
                        return;
                    }
                },
                Stage::Checks => match checks.run(&Finished {
                    run_id: &run_id,
                    live: &live,
                    slot: job.slot,
                    live_slots: &live_slots,
                    held_mounts: &held_mounts,
                }) {
                    // A marker that is not on disk yet (it could not be written when the
                    // host was tainted) holds the slot, whatever the job left; so does a
                    // record of the owed checks that cannot be removed.
                    Ok(()) => {
                        if let Err(why) = taint.persist() {
                            job.note(code::TAINT_WRITE_FAILED, why.to_string(), events);
                            return;
                        }
                        if let Err(why) = config.state.settle(&run_id) {
                            job.note(code::OWED_FAILED, why.to_string(), events);
                            return;
                        }
                        events.push(Event::ChecksPassed {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Freed;
                        // The cleanup passed: no container, no scratch directory and no
                        // process of the slot's uid. Only here does the slot come back.
                        match (job.slot, job.slot_parked) {
                            (Some(slot), true) => slots.park(slot),
                            (Some(slot), false) => slots.release(slot),
                            (None, _) => {}
                        }
                        events.push(Event::SlotFreed {
                            run_id: run_id.clone(),
                        });
                    }
                    Err(found) => {
                        // `detail()` is cut, whoever built the finding.
                        let message = match &found {
                            CheckFinding::Residue { .. } => found.detail().to_owned(),
                            CheckFinding::Unchecked { .. } => {
                                format!("the check could not run: {}", found.detail())
                            }
                        };
                        job.taint(taint, check_taint(&run_id, &found), events);
                        job.note(found.code(), message, events);
                        job.stage = Stage::Blocked;
                        // Whatever the check found may be a process of the slot's uid: the
                        // slot takes no new run.
                        if let Some(slot) = job.slot {
                            slots.quarantine(slot);
                        }
                        // The checks have run. The record that they were owed goes once
                        // the taint is on disk; until then it stays, so that a crash
                        // still taints the host at the next start. If it cannot be
                        // removed it stays and taints a restart that is tainted already.
                        if taint.persist().is_ok() {
                            let _ = config.state.settle(&run_id);
                        }
                        return;
                    }
                },
                Stage::Preparing(_) | Stage::Running(_) | Stage::Blocked | Stage::Freed => return,
            }
        }
    }
}

/// How the removal of the directories of the run `run_id` stands: done once they are
/// `removed` (a step that waits for the quota after the removal); the answer of the removal
/// under way on `dirs` when there is one; otherwise, once the `runtime` has discarded what it
/// mounted for a run it never made (an assembly before the container, on the worker: it is
/// the runtime's; the run's directories hold its writable layers, so it goes first), the
/// removal of the run's directories in the scratch root and the files root (`roots`) and of
/// the runtime's directories `beside` them, begun on `dirs`.
fn dirs_removal(
    (removed, run_id): (bool, &str),
    (runtime, dirs): (&mut dyn Runtime, &mut RunDirs),
    (scratch, files): (&ScratchRoot, Option<&FilesRoot>),
    beside: [&Option<PathBuf>; 2],
) -> DirsRemoval {
    if removed {
        return DirsRemoval::Done(Ok(()));
    }
    if let Some(followed) = dirs.follow(run_id) {
        return followed;
    }
    if let Err(why) = runtime.discard_fs(run_id) {
        return DirsRemoval::Done(Err(std::io::Error::other(why)));
    }
    let (scratch, files, id) = (scratch.clone(), files.cloned(), run_id.to_owned());
    let [lease, tmp] = beside;
    let (lease, tmp) = (lease.clone(), tmp.clone());
    let task = move || {
        let beside = [lease.as_deref(), tmp.as_deref()];
        remove_run_dirs((&scratch, files.as_ref()), &id, beside)
    };
    dirs.begin(run_id, Box::new(task))
}

/// Removes the run's scratch directory, the runtime's directories beside it (its lease and
/// its job's temporary directory) and its files directory; all are tried, and the first
/// failure is the one reported.
fn remove_run_dirs(
    (scratch, files): (&ScratchRoot, Option<&FilesRoot>),
    run_id: &str,
    beside: [Option<&Path>; 2],
) -> std::io::Result<()> {
    // The runtime removes its own directories with the container; one it left (a start that
    // failed after the directory was made) goes with the run's.
    let [lease, tmp] = beside.map(|dir| dir.map_or(Ok(()), |dir| scratch.remove_lease(dir)));
    let removed = scratch.remove(run_id).and(lease).and(tmp);
    let files = files.map_or(Ok(()), |root| root.remove(run_id));
    removed.and(files)
}
