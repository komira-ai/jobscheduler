//! A job's end: watching a running container, and taking a finished job through the
//! cleanup in order.

use super::Lifecycle;
use super::job::{Cause, Stage, check_taint, cleanup_taint, outcome};
use crate::lifecycle::runtime::{ContainerId, Status};
use crate::lifecycle::{CheckFinding, Event, Finished, code};

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
                    job.result = Some(job.report(how, status, None));
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
        let live: Vec<ContainerId> = self
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
                Stage::Scratch => match config.scratch.remove(&run_id) {
                    Ok(()) => {
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
                        if let Some(slot) = job.slot {
                            slots.release(slot);
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
                Stage::Running(_) | Stage::Blocked | Stage::Freed => return,
            }
        }
    }
}
