//! A start between its admission and its container: it waits for its images (the image and
//! the image of each image layer), and for the lookups and then the assembly of its file
//! system ([`Runtime::resolve_fs`], [`Runtime::assemble_fs`]), each off the worker, asking
//! the runtime again at every step; then its directories are made and its container
//! created, unless the run was stopped or its lease ended first.
//!
//! A start with layers or mounts has its scratch directory, and the lease directory beside
//! it, made once its images are here and its host paths found, before the assembly, which
//! puts its writable layers in them: the directories are made on the worker as they are
//! for any start (local operations on the scratch volume), and the assembly, whose mounts
//! look up the providers' file systems, runs off it.
//!
//! [`Runtime::resolve_fs`]: crate::lifecycle::Runtime::resolve_fs
//! [`Runtime::assemble_fs`]: crate::lifecycle::Runtime::assemble_fs

use std::path::PathBuf;

use js_agent_wire::{Isolation, JobSpec};

use super::job::{Given, Keyed, Phase, Stage, Waiting, run_spec};
use super::{Failure, Lifecycle, runtime_failure};
use crate::lifecycle::runtime::{
    FsPrepared, JobFs, Prepared, ResolvedMount, RunHandle, Runtime, RuntimeError, Ticket,
};
use crate::lifecycle::{Event, code, detail};

/// The images a contained start waits for: its image, when it has one, and the image of
/// each of its image layers, base first.
fn images_of(spec: &JobSpec, fs: &JobFs) -> Vec<(String, Option<Ticket>)> {
    let image = Some(spec.image.as_str()).filter(|image| !image.is_empty());
    image
        .into_iter()
        .chain(fs.images())
        .map(|image| (image.to_owned(), None))
        .collect()
}

/// Asks the runtime for each image (`waited`: the fetch it waits on, none for a new start),
/// and gives the images still being fetched, each with its fetch, or the failure of the
/// first that could not be had.
fn ask(
    runtime: &mut dyn Runtime,
    images: Vec<(String, Option<Ticket>)>,
) -> Result<Vec<(String, Ticket)>, RuntimeError> {
    let mut waiting = Vec::new();
    for (image, waited) in images {
        match runtime.prepare(&image, waited) {
            Prepared::Ready => {}
            Prepared::Pending(ticket) => waiting.push((image, ticket)),
            Prepared::Failed(why) => return Err(why),
        }
    }
    Ok(waiting)
}

/// What a step makes of a start that waits.
enum Next {
    /// It waits on.
    Wait,
    /// It ends without a container, as asked or at the end of its lease.
    End,
    /// Its preparation failed.
    Fail(Failure),
    /// Its images and host paths are here: its directories are to be made.
    Dirs,
    /// Everything is here: its container is to be made, in `scratch` when its directories
    /// were made before (the assembly's).
    Begin(Option<PathBuf>),
}

impl Lifecycle {
    /// Starts an admitted start, or holds it waiting (as `starting`, booking its slot) for
    /// what it needs, or answers it `failed_to_start` when an image could not be had. A
    /// host-isolation run uses no image store (the runtime pulls for it inside the create, as
    /// it always did) and a VM start boots the agent's base image: neither is asked about an
    /// image. A start with layers or mounts always waits: its host paths are looked up off
    /// the worker.
    pub(super) fn prepare_or_begin(
        &mut self,
        (spec, keyed): (&JobSpec, Keyed),
        given: &Given,
        (mounts, slot, booked): (Vec<ResolvedMount>, u32, u64),
        received: u64,
        events: &mut Vec<Event>,
    ) {
        let images = match (spec.isolation, given.vm) {
            (Isolation::Host, _) | (_, true) => Vec::new(),
            (Isolation::Contained, false) => images_of(spec, &given.fs),
        };
        let phase = match given.fs.is_empty() {
            true => Phase::Images,
            false => Phase::Resolving,
        };
        let at = (None, u64::MAX);
        match (ask(self.runtime.as_mut(), images), phase) {
            (Err(why), _) => {
                let failure = runtime_failure(&why);
                self.settle((spec, keyed), Err(failure), (slot, booked), at, events);
            }
            (Ok(tickets), Phase::Images) if tickets.is_empty() => {
                let started =
                    self.begin((spec, keyed), given, (mounts, None), (slot, booked), events);
                self.settle((spec, keyed), started, (slot, booked), at, events);
            }
            (Ok(tickets), phase) => {
                // The lease the start names ends `valid_for_ms` after the reply came; a run
                // that waits is held to that end when it starts.
                let lease_end_ms = match spec.valid_for_ms {
                    0 => u64::MAX,
                    valid => received.saturating_add(valid),
                };
                let waiting = Waiting {
                    spec: spec.clone(),
                    given: given.clone(),
                    mounts,
                    slot,
                    booked,
                    tickets,
                    phase,
                    lease_end_ms,
                };
                let mut job = super::Job::of(
                    spec,
                    Stage::Preparing(Box::new(waiting)),
                    0,
                    Some(slot),
                    booked,
                );
                job.keyed = keyed;
                self.jobs.push(job);
            }
        }
    }

    /// Takes the job at `index`, if it waits, one step on: stopped or cancelled while it
    /// waited, or past the end of its lease, it ends without a container (once an assembly
    /// under way has answered: what it mounts is unmounted by the cleanup, never while it is
    /// mounting); with everything here, its directories are made, or its container created
    /// and its wall-time limit begun; an image or a preparation that failed ends it as
    /// `failed_to_start`; otherwise it waits on.
    pub(super) fn advance(&mut self, index: usize, events: &mut Vec<Event>) {
        let now = self.clock.now_ms();
        let Self { jobs, runtime, .. } = self;
        let job = &mut jobs[index];
        let Stage::Preparing(waiting) = &mut job.stage else {
            return;
        };
        let next = match job.cause.is_some() || now >= waiting.lease_end_ms {
            true => ending(runtime.as_mut(), waiting, now),
            false => step(runtime.as_mut(), waiting, now),
        };
        let keyed = job.keyed;
        let (spec, given, mounts) = (
            waiting.spec.clone(),
            waiting.given.clone(),
            waiting.mounts.clone(),
        );
        let (slot, booked, lease_end_ms) = (waiting.slot, waiting.booked, waiting.lease_end_ms);
        let at = (Some(index), lease_end_ms);
        match next {
            Next::Wait => {}
            Next::End => job.end_unstarted(events),
            Next::Fail(failure) => {
                self.settle((&spec, keyed), Err(failure), (slot, booked), at, events);
            }
            Next::Dirs => match self.make_dirs(&spec.run_id, (slot, booked), events) {
                Ok(scratch) => {
                    // Its images and host paths are here: its assembly is asked for at once,
                    // so a start that is ending is never asked to assemble, and it waits.
                    // An answer it gives at once is asked again at the next step.
                    let _ = self.runtime.assemble_fs(&spec.run_id, now);
                    let waiting = Waiting {
                        spec,
                        given,
                        mounts,
                        slot,
                        booked,
                        tickets: Vec::new(),
                        phase: Phase::Assembling(scratch),
                        lease_end_ms,
                    };
                    self.jobs[index].stage = Stage::Preparing(Box::new(waiting));
                }
                Err(failure) => {
                    self.settle((&spec, keyed), Err(failure), (slot, booked), at, events);
                }
            },
            Next::Begin(scratch) => {
                let started = self.begin(
                    (&spec, keyed),
                    &given,
                    (mounts, scratch),
                    (slot, booked),
                    events,
                );
                self.settle((&spec, keyed), started, (slot, booked), at, events);
            }
        }
    }

    /// Makes the run's scratch directory (with its quota) and files, unless the directories
    /// were made before (`scratch`), and creates and starts its container.
    pub(super) fn begin(
        &mut self,
        (spec, keyed): (&JobSpec, Keyed),
        given: &Given,
        (mounts, scratch): (Vec<ResolvedMount>, Option<PathBuf>),
        (slot, booked): (u32, u64),
        events: &mut Vec<Event>,
    ) -> Result<RunHandle, Failure> {
        let run_id = &spec.run_id;
        let scratch = match scratch {
            Some(scratch) => scratch,
            None => self.make_dirs(run_id, (slot, booked), events)?,
        };
        // The files are written before the container exists, and only here: a start that
        // is sent again finds its job above and never reaches this point. `admit` has
        // seen to it that a start with files has a files root.
        let written = match (&self.config.files, spec.files.is_empty()) {
            (Some(root), false) => root.write(run_id, &spec.files).map(Some),
            _ => Ok(None),
        };
        let files = written.map_err(scratch_failure)?;
        let run = run_spec((spec, keyed), given, (mounts, scratch, files), slot, booked);
        self.runtime
            .create_and_start(&run)
            .map_err(|why| runtime_failure(&why))
    }

    /// Makes the run's scratch directory and, beside it, the directory the runtime keeps of
    /// the run (a container's writable layers and home), both put under the run's project
    /// before anything is written to them: a file made earlier would carry no project and
    /// escape the count. A failure leaves what was made to the cleanup, which removes it and
    /// clears the project.
    fn make_dirs(
        &mut self,
        run_id: &str,
        (slot, booked): (u32, u64),
        events: &mut Vec<Event>,
    ) -> Result<PathBuf, Failure> {
        let scratch = self
            .config
            .scratch
            .create(run_id)
            .map_err(scratch_failure)?;
        let lease = self
            .runtime
            .lease_dir(run_id)
            .map(|dir| self.config.scratch.create_lease(&dir))
            .transpose()
            .map_err(scratch_failure)?;
        for dir in std::iter::once(&scratch).chain(lease.as_ref()) {
            self.bind(dir, slot, booked, events)
                .map_err(|why| scratch_failure(why.into()))?;
        }
        Ok(scratch)
    }
}

/// The failure of a run whose directories or files could not be made.
fn scratch_failure(why: std::io::Error) -> Failure {
    (
        detail::SCRATCH_FAILED,
        code::SCRATCH_FAILED,
        why.to_string(),
        false,
    )
}

/// A step of a start that is to end: at once, unless its assembly is under way.
fn ending(runtime: &mut dyn Runtime, waiting: &Waiting, now: u64) -> Next {
    let assembling = matches!(waiting.phase, Phase::Assembling(_));
    match assembling && runtime.assemble_fs(&waiting.spec.run_id, now) == FsPrepared::Pending {
        true => Next::Wait,
        false => Next::End,
    }
}

/// A step of a start that waits on: its images asked again, and its file system's
/// preparation followed.
fn step(runtime: &mut dyn Runtime, waiting: &mut Waiting, now: u64) -> Next {
    let images = std::mem::take(&mut waiting.tickets)
        .into_iter()
        .map(|(image, ticket)| (image, Some(ticket)))
        .collect();
    let asked = ask(runtime, images);
    let run_id = waiting.spec.run_id.as_str();
    let fs = match &waiting.phase {
        Phase::Images => FsPrepared::Ready,
        Phase::Resolving => runtime.resolve_fs(run_id, &waiting.spec.image, &waiting.given.fs, now),
        Phase::Assembling(_) => runtime.assemble_fs(run_id, now),
    };
    let tickets = match (asked, fs) {
        (Err(why), _) | (_, FsPrepared::Failed(why)) => return Next::Fail(runtime_failure(&why)),
        (_, FsPrepared::Refused(refused)) => {
            return Next::Fail((refused.code, code::START_FAILED, refused.why, false));
        }
        (Ok(tickets), FsPrepared::Pending) => {
            waiting.tickets = tickets;
            return Next::Wait;
        }
        (Ok(tickets), FsPrepared::Ready) => tickets,
    };
    if !tickets.is_empty() {
        waiting.tickets = tickets;
        return Next::Wait;
    }
    match &waiting.phase {
        Phase::Images => Next::Begin(None),
        Phase::Resolving => Next::Dirs,
        Phase::Assembling(scratch) => Next::Begin(Some(scratch.clone())),
    }
}
