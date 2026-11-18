//! A start between its admission and its container: it waits for its images (the image and
//! the image of each image layer), for the directories its providers make for the layers
//! and mounts that name one (`providing`), and for the lookups and then the assembly of its
//! file system ([`Runtime::resolve_fs`], [`Runtime::assemble_fs`]), each off the worker,
//! asking the runtime again at every step; then its container is created, unless the run
//! was stopped or its lease ended first.
//!
//! A start with layers or mounts runs its independent waits side by side. At its admission,
//! before the worker's step returns, its providers are asked to prepare their sources, its
//! images are asked, the runtime begins the lookups of the host paths that wait for no
//! provider (the file system without its provider sources), and its scratch directory, and
//! the lease directory beside it, are made. At each step its providers are followed, its
//! images asked again and its lookups followed; once the providers have answered, their
//! directories are bound into the file system and the runtime is asked about that. When the
//! providers have answered, the lookups are done and no image is left, the assembly, which
//! puts its writable layers in the directories, is asked for at once. The directories are
//! made on the worker as they are for any start (local operations on the scratch volume),
//! and the assembly, whose mounts look up the providers' file systems, runs off it.
//!
//! [`Runtime::resolve_fs`]: crate::lifecycle::Runtime::resolve_fs
//! [`Runtime::assemble_fs`]: crate::lifecycle::Runtime::assemble_fs

use std::path::PathBuf;

use js_agent_wire::{Isolation, JobSpec};

use super::job::{Given, Keyed, Phase, Stage, Waiting, run_spec};
use super::providing::{Follow, Providing, follow};
use super::timeline::mark_at;
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
    /// Its providers have answered, its host paths are found and its images are here: its
    /// assembly is to be asked for, in the directories made at its admission.
    Assemble(PathBuf),
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
        let at = (None, u64::MAX);
        // The providers are asked first, before the images: a provider fetches while the
        // image is pulled and the container made.
        let now = self.clock.now_ms();
        let providing = match self.provide(&spec.run_id, &given.fs, now) {
            Ok(providing) => providing,
            Err(failure) => {
                self.settle((spec, keyed), Err(failure), (slot, booked), at, events);
                return;
            }
        };
        let images = match (spec.isolation, given.vm) {
            (Isolation::Host, _) | (_, true) => Vec::new(),
            (Isolation::Contained, false) => images_of(spec, &given.fs),
        };
        let tickets = match ask(self.runtime.as_mut(), images) {
            Ok(tickets) => tickets,
            Err(why) => {
                let failure = runtime_failure(&why);
                self.settle((spec, keyed), Err(failure), (slot, booked), at, events);
                return;
            }
        };
        let phase = match (given.fs.is_empty(), tickets.is_empty()) {
            (true, true) => {
                let started =
                    self.begin((spec, keyed), given, (mounts, None), (slot, booked), events);
                self.settle((spec, keyed), started, (slot, booked), at, events);
                return;
            }
            (true, false) => Phase::Images,
            (false, _) => {
                match self.resolve_and_make_dirs((spec, given), providing, (slot, booked), events) {
                    Ok(phase) => phase,
                    Err(failure) => {
                        self.settle((spec, keyed), Err(failure), (slot, booked), at, events);
                        return;
                    }
                }
            }
        };
        // The lease the start names ends `valid_for_ms` after the reply came; a run that
        // waits is held to that end when it starts.
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

    /// Begins the waits of a start with layers or mounts, beside its providers' and its
    /// images': the runtime is asked about the host paths that wait for no provider, and the
    /// run's directories are made. The phase it waits in keeps its providers' part and its
    /// scratch directory.
    fn resolve_and_make_dirs(
        &mut self,
        (spec, given): (&JobSpec, &Given),
        providing: Option<Providing>,
        (slot, booked): (u32, u64),
        events: &mut Vec<Event>,
    ) -> Result<Phase, Failure> {
        let now = self.clock.now_ms();
        // Without providers the file system is whole: it is its own part without them.
        let fs = given.fs.without_providers();
        let asked = self.runtime.resolve_fs(&spec.run_id, &spec.image, &fs, now);
        fs_ready(asked)?;
        let scratch = self.make_dirs(&spec.run_id, (slot, booked), events)?;
        Ok(Phase::Resolving { providing, scratch })
    }

    /// Takes the job at `index`, if it waits, one step on: stopped or cancelled while it
    /// waited, or past the end of its lease, it ends without a container (once an assembly
    /// under way has answered: what it mounts is unmounted by the cleanup, never while it is
    /// mounting); with its providers answered, its host paths found and its images here, its
    /// assembly is asked for, and once that is done its container is created and its
    /// wall-time limit begun; an image or a preparation that failed ends it as
    /// `failed_to_start`; otherwise it waits on.
    pub(super) fn advance(&mut self, index: usize, events: &mut Vec<Event>) {
        let now = self.clock.now_ms();
        let Self {
            jobs,
            runtime,
            config,
            providers,
            starting,
            ..
        } = self;
        let job = &mut jobs[index];
        let Stage::Preparing(waiting) = &mut job.stage else {
            return;
        };
        let next = match job.cause.is_some() || now >= waiting.lease_end_ms {
            true => ending(runtime.as_mut(), waiting, now),
            false => match follow(waiting, (providers, &config.mounts), now) {
                Follow::Go => step(runtime.as_mut(), waiting, now),
                Follow::Bound => {
                    mark_at(starting, &waiting.spec.run_id, "provided", now);
                    step(runtime.as_mut(), waiting, now)
                }
                Follow::Fail(failure) => Next::Fail(failure),
            },
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
            Next::End => {
                job.end_unstarted(events);
                self.drop_timeline(&spec.run_id);
            }
            Next::Fail(failure) => {
                self.settle((&spec, keyed), Err(failure), (slot, booked), at, events);
            }
            Next::Assemble(scratch) => {
                // Everything the assembly needs is here: it is asked for at once, so a start
                // that is ending is never asked to assemble, and it waits. An answer it gives
                // at once is asked again at the next step.
                self.mark(&spec.run_id, "resolved");
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
        self.mark(run_id, "ready");
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
        self.mark(run_id, "dirs");
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

/// Whether an answer of the runtime about a file system says the lookups or the assembly are
/// done (`true`) or under way (`false`).
///
/// # Errors
///
/// The runtime refused the start's file system or a call of it failed.
fn fs_ready(answer: FsPrepared) -> Result<bool, Failure> {
    match answer {
        FsPrepared::Ready => Ok(true),
        FsPrepared::Pending => Ok(false),
        FsPrepared::Refused(refused) => Err((refused.code, code::START_FAILED, refused.why, false)),
        FsPrepared::Failed(why) => Err(runtime_failure(&why)),
    }
}

/// A step of a start that waits on: its images asked again, and its file system's
/// preparation followed. While its providers have not all answered, the lookups are of the
/// file system without their sources.
fn step(runtime: &mut dyn Runtime, waiting: &mut Waiting, now: u64) -> Next {
    let images = std::mem::take(&mut waiting.tickets)
        .into_iter()
        .map(|(image, ticket)| (image, Some(ticket)))
        .collect();
    let asked = ask(runtime, images);
    let run_id = waiting.spec.run_id.as_str();
    let fs = match &waiting.phase {
        Phase::Images => FsPrepared::Ready,
        // The sources of a provider that has not answered are left out; once it has, they
        // are host paths in the file system and nothing is left out.
        Phase::Resolving { .. } => runtime.resolve_fs(
            run_id,
            &waiting.spec.image,
            &waiting.given.fs.without_providers(),
            now,
        ),
        Phase::Assembling(_) => runtime.assemble_fs(run_id, now),
    };
    let tickets = match (asked, fs_ready(fs)) {
        (Err(why), _) => return Next::Fail(runtime_failure(&why)),
        (_, Err(failure)) => return Next::Fail(failure),
        (Ok(tickets), Ok(false)) => {
            waiting.tickets = tickets;
            return Next::Wait;
        }
        (Ok(tickets), Ok(true)) => tickets,
    };
    if !tickets.is_empty() {
        waiting.tickets = tickets;
        return Next::Wait;
    }
    match &waiting.phase {
        Phase::Images => Next::Begin(None),
        Phase::Resolving {
            providing: Some(_), ..
        } => Next::Wait,
        Phase::Resolving { scratch, .. } => Next::Assemble(scratch.clone()),
        Phase::Assembling(scratch) => Next::Begin(Some(scratch.clone())),
    }
}
