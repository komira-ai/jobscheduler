//! A job's end: watching a running container, and taking a finished job through the
//! cleanup in order.

use super::Lifecycle;
use super::job::{Cause, Stage, check_taint, cleanup_taint, outcome};
use crate::lifecycle::runtime::{RunHandle, Status};
use crate::lifecycle::{CheckFinding, Config, Event, Finding, Finished, code};

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
            ..
        } = self;
        let job = &mut jobs[index];
        let Stage::Running(id) = job.stage.clone() else {
            return;
        };
        // The exit is looked at first: a job that finished by itself before its limit is
        // reported as it finished, even when the agent looks late. A runtime that cannot
        // say is no reason to let the job run past its limit. A container that is gone has
        // nothing to stop, and the job ends as if it had exited.
        let mut seen = runtime.poll(&id);
        if !matches!(seen, Ok(Status::Exited(_))) {
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
            Ok(Status::Running) => {}
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
                if !job.silent {
                    let mut result = job.report(how, status, None);
                    // The container is still there: it is removed in the next stage. A log
                    // that cannot be read costs the result its tail and nothing else.
                    if exit.is_some() {
                        match runtime.log_tail(&id) {
                            Ok(tail) => result.log_tail = tail,
                            Err(why) => job.note(code::LOGS_FAILED, why.to_string(), events),
                        }
                    }
                    job.result = Some(result);
                    events.push(Event::ResultReady {
                        run_id: job.run_id.clone(),
                    });
                }
                job.stage = Stage::Removing(id);
            }
            Err(why) => job.note(code::POLL_FAILED, why.to_string(), events),
        }
    }

    /// Takes one finished job through the cleanup, in order, as far as it goes: the
    /// container, the scratch directory, the checks, then the slot. A step that fails
    /// ends the pass with the slot still booked.
    pub(super) fn clean(&mut self, index: usize, events: &mut Vec<Event>) {
        // A run waiting for its image has nothing to clean (and its stage holds the start).
        if matches!(self.jobs[index].stage, Stage::Preparing(_)) {
            return;
        }
        let live: Vec<RunHandle> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .filter_map(|(_, job)| job.stage.container().cloned())
            .chain(self.stuck.iter().map(|held| held.id.clone()))
            .collect();
        // The slots of the other jobs whose containers are still on the host.
        let live_slots: Vec<u32> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .filter(|(_, job)| job.stage.container().is_some())
            .filter_map(|(_, job)| job.slot)
            .chain(self.stuck.iter().filter_map(|held| held.slot))
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
            ..
        } = self;
        let job = &mut jobs[index];
        let run_id = job.run_id.clone();
        loop {
            let stage = job.stage.clone();
            // The checks are owed from before the first step of the cleanup until they have
            // run: an agent that stops in between leaves the record, and the next start
            // taints the host. Without the record nothing could be removed.
            let cleaning = matches!(stage, Stage::Removing(_) | Stage::Scratch | Stage::Checks);
            if cleaning && !job.owed {
                match config.state.owe(&run_id) {
                    Ok(()) => job.owed = true,
                    Err(why) => {
                        job.note(code::OWED_FAILED, why.to_string(), events);
                        return;
                    }
                }
            }
            match stage {
                Stage::Removing(id) => match runtime.remove(&id) {
                    Ok(()) => {
                        events.push(Event::ContainerRemoved {
                            run_id: run_id.clone(),
                        });
                        job.stage = Stage::Scratch;
                    }
                    Err(why) => {
                        let tainted = cleanup_taint(Some(&run_id), code::CONTAINER_REMOVE_FAILED);
                        job.taint(taint, tainted, events);
                        job.note(code::CONTAINER_REMOVE_FAILED, why.to_string(), events);
                        return;
                    }
                },
                Stage::Scratch => match remove_run_dirs(config, &run_id) {
                    Ok(()) => {
                        // The tree is gone; now the bound comes off its project, and the
                        // slot stays held until the project reads no usage (the file system
                        // frees the inodes of a removed tree in the background, and a run
                        // that took the slot sooner would be counted with them). The booked
                        // bytes stay held with it: the server counts them as free once the
                        // run has ended, so holding them a little longer only makes the
                        // agent refuse later than the server admits for the seconds the
                        // lag lasts, the same as for every other booked dimension, which
                        // are held until this cleanup has passed. A failure is tried again
                        // at the next step, together with the removal, which finds nothing
                        // to do. None of it taints: nothing is left on disk.
                        if let (Some(quota), Some(slot)) = (&config.quota, job.slot) {
                            let dir = config.scratch.of(&run_id);
                            let now = clock.now_ms();
                            let health = config.scratch.health_dir();
                            if let Err(why) = quota.clear(&dir, slot) {
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
                    Err(why) => {
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

/// Removes the run's scratch directory and its files directory; both are tried, and the
/// first failure is the one reported.
fn remove_run_dirs(config: &Config, run_id: &str) -> std::io::Result<()> {
    let scratch = config.scratch.remove(run_id);
    let files = config
        .files
        .as_ref()
        .map_or(Ok(()), |root| root.remove(run_id));
    scratch.and(files)
}
