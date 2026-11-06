//! What a runtime does with the leases an earlier process of the same agent left: it takes
//! over the runs that still run, and removes the rest.
//!
//! A run's `crun run` is run by a supervisor process, not by the agent, so a lease outlives
//! the agent that made it. At [`CrunRuntime::open`] the ledger's sidecars of this agent are
//! read, and a lease is **adopted** only when everything its sidecar says holds, each part
//! read from the host and none assumed:
//!
//! - the sidecar is of version 2 and in the state running, with every field present;
//! - its labels agree with its fields (run, lease, agent, wall-time limit, booking, slot);
//! - the lease's directory, cgroup and exit record are the ones this runtime would name for
//!   it, its scratch directory is the run's own (beside the runs when the leases are), and
//!   the directories are directories, not links;
//! - and either the supervisor left an exit record that can be read (the run ended while no
//!   agent ran, and is reported from the record), or the supervisor named by the sidecar is
//!   the live process it was and, when `crun state` names the container's init, that
//!   process is in the lease's cgroup (or one below it). A live supervisor with no record
//!   and no init named is a run that is ending, or not yet created (`crun` deletes the
//!   container when the job exits, and the supervisor drains the streams before it writes
//!   its record): it is adopted, and its handle gives the end once the record is there.
//!   With no record, the runtime must also have no egress: an allow-list egress removes every
//!   interface and table a previous process wired when it starts, so a run that may still
//!   run would have no network, and it is removed. A run whose record is there has ended and
//!   has no network left: its result is taken over whatever the egress.
//!
//! An adopted run stands as a run this process started, with the start time the sidecar
//! recorded (wall clock), so the lifecycle reads how long it has run and holds it to the rest
//! of its wall-time limit. The wall limit is enforced only while an agent runs: the time a
//! job ran with no agent counts against it, and a job whose limit passed meanwhile is
//! stopped at the first step after the adoption.
//!
//! A run whose lease directory holds the progress record of its live logs
//! (`logs-upload.json`, [`crate::logs`]) gets an uploader made from it when the runtime is given
//! its live logs ([`CrunRuntime::with_live_logs`]): a regular file of the agent's user, of a
//! known version, whose prefix is the job's. One that ended while no agent ran is finishing at
//! once, with the totals of its exit record. A record that fails a check is ignored with one
//! warning, and the run is adopted as it would be without one: it uploads nothing.
//!
//! Every other lease of this agent is torn down and reported with its [`Reason`]: a sidecar
//! of version 1, one whose container never started, one that failed a check, and a lease of
//! this agent's name that has no sidecar at all. The sidecars and leases of other agents are
//! left alone, as they always were.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use js_contain::cgroup::LeaseCgroup;
use js_contain::oci::{
    ExitRecord, end_supervisor, exit_record_path, read_exit_record, recorded_supervisor,
    stderr_path, stdout_path,
};
use js_types::JobNetwork;

use super::dirs::cgroup_dir;
use super::ledger::{
    Booked, Entry, Failed, Phase, Read, Reason, Removed, Supervisor, SweepReport, VERSION,
};
use super::spec::{adopted_spec, concluded};
use super::{CrunError, CrunRuntime, Fault, Run, State, legacy_lease, net};
use crate::cli_runtime::{NAME_PREFIX, agent_tag, container_name};
use crate::lifecycle::{RunSpec, RuntimeError, label};
use crate::logs::{LiveLogs, RunLogs};

/// The version 2 fields of a sidecar in the state running, all present.
#[derive(Debug)]
pub(super) struct Fields<'a> {
    allocation: &'a str,
    booked: &'a Booked,
    started_wall_ms: u64,
    wall_ms: u64,
    lease_dir: &'a Path,
    cgroup: &'a Path,
    scratch: &'a Path,
    exit_record: &'a Path,
    image_digest: &'a str,
    supervisor: Supervisor,
}

/// The version 2 fields of `entry`.
///
/// # Errors
/// The first that is missing.
pub(super) fn fields_of(entry: &Entry) -> Result<Fields<'_>, String> {
    let missing = first_wrong(&[
        ("lease id", entry.allocation.is_some()),
        ("booking", entry.booked.is_some()),
        ("start", entry.started_wall_ms.is_some()),
        ("wall-time limit", entry.wall_ms.is_some()),
        ("lease directory", entry.lease_dir.is_some()),
        ("cgroup", entry.cgroup.is_some()),
        ("scratch directory", entry.scratch.is_some()),
        ("exit record", entry.exit_record.is_some()),
        ("image", entry.image_digest.is_some()),
        ("supervisor", entry.supervisor.is_some()),
    ]);
    match (
        (entry.allocation.as_deref(), entry.booked.as_ref()),
        (entry.started_wall_ms, entry.wall_ms),
        (entry.lease_dir.as_deref(), entry.cgroup.as_deref()),
        (entry.scratch.as_deref(), entry.exit_record.as_deref()),
        (entry.image_digest.as_deref(), entry.supervisor),
    ) {
        (
            (Some(allocation), Some(booked)),
            (Some(started_wall_ms), Some(wall_ms)),
            (Some(lease_dir), Some(cgroup)),
            (Some(scratch), Some(exit_record)),
            (Some(image_digest), Some(supervisor)),
        ) => Ok(Fields {
            allocation,
            booked,
            started_wall_ms,
            wall_ms,
            lease_dir,
            cgroup,
            scratch,
            exit_record,
            image_digest,
            supervisor,
        }),
        _ => Err(format!(
            "its {} is not recorded",
            missing.unwrap_or_default()
        )),
    }
}

/// The label `key` of `labels` as a number, when it is one.
fn number(labels: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    labels.get(key)?.parse().ok()
}

/// The first of `checks` that does not hold, by its name.
fn first_wrong<'a>(checks: &[(&'a str, bool)]) -> Option<&'a str> {
    checks
        .iter()
        .find(|(_, holds)| !holds)
        .map(|(what, _)| *what)
}

/// Whether the labels of `entry` say what its fields say: the run, the lease, the agent,
/// the wall-time limit and the booking.
///
/// # Errors
/// The first label that differs.
pub(super) fn agree(entry: &Entry, fields: &Fields<'_>) -> Result<(), String> {
    let booked = fields.booked;
    let gpus: Vec<String> = booked.gpus.iter().map(u32::to_string).collect();
    let expected = [
        (label::RUN, Some(entry.run_id.clone())),
        (label::LEASE, Some(fields.allocation.to_owned())),
        (label::AGENT, Some(entry.agent.clone())),
        (label::WALL_MS, Some(fields.wall_ms.to_string())),
        (label::CPU_MILLIS, Some(booked.cpu_millis.to_string())),
        (label::MEMORY_BYTES, Some(booked.memory_bytes.to_string())),
        (
            label::DISK_BYTES,
            booked.disk_bytes.map(|bytes| bytes.to_string()),
        ),
        (label::GPUS, Some(gpus.join(","))),
        (label::SLOT, Some(booked.slot.to_string())),
    ];
    let differs = expected
        .into_iter()
        .find(|(key, value)| entry.labels.get(*key) != value.as_ref());
    match differs {
        Some((key, _)) => Err(format!("the label {key} disagrees with the record")),
        None => Ok(()),
    }
}

/// Whether `scratch` is the scratch directory of the run `run_id`: named for it, and, when
/// the leases are made beside the runs' directories in `beside`, in that directory.
pub(super) fn scratch_fits(scratch: &Path, run_id: &str, beside: Option<&Path>) -> bool {
    scratch.file_name() == Some(OsStr::new(run_id))
        && beside.is_none_or(|root| scratch.parent() == Some(root))
}

/// Whether `dir` is a directory and not a link.
pub(super) fn is_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir())
}

/// Whether the process `pid` is in the `cgroup.procs` of the cgroup `dir`, or of a cgroup
/// below it. What cannot be read lists nothing.
pub(super) fn in_cgroup(dir: &Path, pid: u32) -> bool {
    let listed = fs::read_to_string(dir.join("cgroup.procs"))
        .is_ok_and(|procs| procs.lines().any(|line| line.trim() == pid.to_string()));
    listed
        || fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|child| child.file_type().is_ok_and(|kind| kind.is_dir()))
            .any(|child| in_cgroup(&child.path(), pid))
}

/// The run ids of the leases that have an entry in `dir` named `<marker><run id>`.
pub(super) fn named_in(dir: &Path, marker: &str) -> BTreeSet<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let rest = name.to_str()?.strip_prefix(marker)?;
            Some(rest.to_owned())
        })
        .collect()
}

/// One line for each run a runtime took over, each lease it removed and each it could not
/// remove, when it opened. The lines are for the operator and name no host path.
pub(super) fn report_lines(report: &SweepReport) -> Vec<String> {
    let adopted = report
        .adopted
        .iter()
        .map(|run| format!("crun: adopted job {run}"));
    let removed = report.removed.iter().map(|lease| {
        format!(
            "crun: removed the lease of job {}: {}",
            lease.run_id, lease.reason
        )
    });
    let failed = report.failed.iter().map(|lease| {
        format!(
            "crun: could not remove the lease of job {}: {}",
            lease.run_id, lease.why
        )
    });
    adopted.chain(removed).chain(failed).collect()
}

impl CrunRuntime {
    /// The start-up pass: adopts the runs of this agent that still stand, removes the rest
    /// of its leases, and removes the leases of its name that no sidecar records.
    pub(super) fn recover(&mut self) -> Result<SweepReport, CrunError> {
        let swept = self
            .ledger
            .sweep(|entry| self.judge(entry), |entry| self.tear_down(entry));
        // On a line of its own: the line of the call above holds the closures, and its only
        // region of this function would be the error's, which no start takes.
        let swept = swept?;
        let mut report = swept.report;
        for (name, run) in swept.adopted {
            self.images.lease(&name, &run.spec.image_digest);
            self.runs.insert(name, run);
        }
        self.remove_unrecorded(&mut report);
        self.sweep_store_leases();
        Ok(report)
    }

    /// The start of the name of every lease of this agent: `js-<agent tag>-`.
    fn lease_prefix(&self) -> String {
        format!("{NAME_PREFIX}{}-", agent_tag(&self.host_id))
    }

    /// The sidecar of the lease `name` of the run `spec`, in the state starting, with all
    /// a successor needs to adopt it once it runs (the start and the supervisor are filled
    /// in when the container started: [`Entry::running`]).
    pub(super) fn entry_for(
        &self,
        spec: &RunSpec,
        name: &str,
        (labels, image_digest): (&BTreeMap<String, String>, &str),
    ) -> Entry {
        let lease = self.oci.lease_dirs(name).dir;
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, name);
        let mut entry = Entry::new(&self.host_id, &spec.run_id, name, labels);
        entry.allocation = spec.labels.get(label::LEASE).cloned();
        entry.booked = Some(Booked {
            cpu_millis: spec.cpu_millis,
            memory_bytes: spec.memory_bytes,
            disk_bytes: number(&spec.labels, label::DISK_BYTES),
            gpus: spec.gpu_indices.clone(),
            slot: spec.slot,
        });
        entry.wall_ms = number(&spec.labels, label::WALL_MS);
        entry.cgroup = Some(cgroup.dir().to_owned());
        entry.scratch = Some(spec.scratch.clone());
        entry.exit_record = Some(exit_record_path(&lease));
        entry.lease_dir = Some(lease);
        entry.image_digest = Some(image_digest.to_owned());
        entry
    }

    /// Whether the sidecar `entry` can be taken over, and the run it makes if so.
    ///
    /// # Errors
    /// Why not.
    fn judge(&self, entry: &Entry) -> Result<(String, Run), Reason> {
        if entry.version < VERSION {
            return Err(Reason::OldForm);
        }
        if entry.state != Some(Phase::Running) {
            return Err(Reason::NeverRan);
        }
        let fields = fields_of(entry).map_err(Reason::Inconsistent)?;
        agree(entry, &fields).map_err(Reason::Inconsistent)?;
        self.places(entry, &fields).map_err(Reason::Inconsistent)?;
        let run = self.adopted(entry, &fields)?;
        Ok((entry.name.clone(), run))
    }

    /// The run the sidecar `entry`, whose `fields` were checked, stands for, with its state:
    /// ended, from the exit record its supervisor wrote, or still standing. The one place an
    /// adopted run and its state are made.
    ///
    /// # Errors
    /// The record cannot be read, or the run cannot stand (`standing`).
    fn adopted(&self, entry: &Entry, fields: &Fields<'_>) -> Result<Run, Reason> {
        let mut run = self.run_of(entry, fields);
        // The supervisor is looked at before the record: one that is gone has written its
        // record already, so a record not there then is one that never will be.
        let alive = fields.supervisor.id().alive();
        let record = read_exit_record(fields.lease_dir)
            .map_err(|why| Reason::Inconsistent(format!("the exit record: {why}")))?;
        run.state = match record {
            // A container that ended has no network left to take over: its result is kept
            // whatever the egress, so it is reported. Its uploader, if it gets one, finishes
            // with the totals the supervisor counted.
            Some(record) => {
                let totals = (record.tail.stdout.total, record.tail.stderr.total);
                run.logs = RunLogs::adopted(Some(totals));
                self.ended_state(&run, &record)
            }
            // One that may still run is wired, and an egress removed the wiring of every
            // earlier process when it started.
            None if self.egress.is_some() => {
                return Err(Reason::Inconsistent(
                    "its network wiring cannot be taken over".to_owned(),
                ));
            }
            None => self
                .standing(&run, fields, alive)
                .map_err(Reason::Inconsistent)?,
        };
        Ok(run)
    }

    /// Gives each adopted run whose lease directory holds a progress record of this agent's
    /// user its uploader, with `logs` (see [`RunLogs::resume`]): one that ended is finishing
    /// at once. A run without a record, or with one that is ignored, has none.
    pub(super) fn resume_uploads(&mut self, logs: &LiveLogs) {
        let owner = rustix::process::geteuid().as_raw();
        for run in self.runs.values_mut() {
            let files = (stdout_path(&run.lease.dir), stderr_path(&run.lease.dir));
            let job = (run.entry.run_id.as_str(), owner);
            run.logs
                .resume(logs, (&files.0, &files.1), &run.lease.dir, job);
        }
    }

    /// Whether the paths `fields` records are the ones this runtime names for the lease of
    /// `entry`, and are directories.
    fn places(&self, entry: &Entry, fields: &Fields<'_>) -> Result<(), String> {
        let lease = self.oci.lease_dirs(&entry.name);
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, &entry.name);
        let beside = self.leases_beside.then_some(lease.dir.parent()).flatten();
        let scratch = scratch_fits(fields.scratch, &entry.run_id, beside);
        let wrong = first_wrong(&[
            ("lease directory", fields.lease_dir == lease.dir),
            ("cgroup", fields.cgroup == cgroup.dir()),
            (
                "exit record",
                fields.exit_record == exit_record_path(&lease.dir),
            ),
            ("scratch directory", scratch),
        ]);
        if let Some(what) = wrong {
            return Err(format!("the {what} is not the one this agent makes"));
        }
        let gone = first_wrong(&[
            ("lease directory", is_dir(fields.lease_dir)),
            ("cgroup", is_dir(fields.cgroup)),
            ("scratch directory", is_dir(fields.scratch)),
        ]);
        gone.map_or(Ok(()), |what| {
            Err(format!("the {what} is gone or is a link"))
        })
    }

    /// The run of the lease `entry` records, not started or ended: the container, the
    /// lease's directories and cgroup, and the labels, as a start by this process makes
    /// them. The OOM account starts at the parent's count now.
    fn run_of(&self, entry: &Entry, fields: &Fields<'_>) -> Run {
        let lease = self.oci.lease_dirs(&entry.name);
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, &entry.name);
        Run {
            spec: adopted_spec(&entry.name, fields.image_digest, &lease, cgroup.name()),
            labels: entry.labels.clone(),
            lease,
            cap: js_contain::hard_memory_max(fields.booked.memory_bytes),
            backstop: cgroup.parent_ooms().ok(),
            cgroup,
            state: State::Created,
            // A run that may still run is adopted only by a runtime with no egress, which admits
            // no job with a network; one that ended has no network to release.
            net: net::Net::asked(JobNetwork::None, Vec::new()),
            entry: entry.clone(),
            // Its uploader, if its lease holds a progress record, comes with the runtime's live
            // logs (`resume_uploads`); the sink came only in its start.
            logs: RunLogs::adopted(None),
        }
    }

    /// The end of `run` as its supervisor recorded it: the exit and the tail, read as a
    /// run this process waited for.
    fn ended_state(&self, run: &Run, record: &ExitRecord) -> State {
        let ended = self
            .rt
            .block_on(self.oci.ended_as(&run.spec, record.end.clone()));
        let (exit, tail) = concluded(ended, &record.tail, (&run.cgroup, run.cap, run.backstop));
        State::Ended(exit, tail)
    }

    /// The state of the run `run`, which left no exit record, if it still stands: its
    /// supervisor (`alive`, as looked at before the record was read) is the process the
    /// sidecar names, and, if `crun state` names an init, that init is in the lease's cgroup.
    /// No init named is not a reason to refuse: `crun` deletes the container when the job
    /// exits and the supervisor drains the streams before it writes its record, and the
    /// container does not exist yet between the supervisor's ready word and `crun`'s create.
    /// The handle gives the end once the record is written.
    fn standing(&self, run: &Run, fields: &Fields<'_>, alive: bool) -> Result<State, String> {
        if !alive {
            return Err("the supervisor has ended and left no exit record".to_owned());
        }
        if let Ok(pid) = self.oci.container_pid(&run.spec)
            && !in_cgroup(fields.cgroup, pid)
        {
            return Err("the container's init is not in the lease's cgroup".to_owned());
        }
        Ok(State::Running {
            run: self
                .oci
                .adopt_supervised(fields.lease_dir, fields.supervisor.id()),
            since_ms: fields.started_wall_ms,
        })
    }

    /// Removes the leases of this agent's name that no sidecar records (a directory beside
    /// the runs, or a cgroup), reporting each.
    fn remove_unrecorded(&self, report: &mut SweepReport) {
        let prefix = self.lease_prefix();
        let probe = self.oci.lease_dirs(&prefix).dir;
        let marker = probe
            .file_name()
            .map(|leaf| leaf.to_string_lossy().into_owned())
            .unwrap_or_default();
        let root = probe.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut runs = named_in(&root, &marker);
        runs.extend(named_in(
            &cgroup_dir(&self.cgroups.mount, &self.cgroups.actions),
            &prefix,
        ));
        for run_id in runs {
            let name = format!("{prefix}{run_id}");
            if self.ledger.read(&name).is_some() {
                continue;
            }
            match self.tear_down_named(&name) {
                Ok(()) => report.removed.push(Removed {
                    run_id,
                    name,
                    labels: BTreeMap::new(),
                    reason: Reason::NoRecord,
                }),
                Err(why) => report.failed.push(Failed { run_id, name, why }),
            }
        }
    }

    /// Removes what an earlier process of this agent left for the run `run_id` (the
    /// lease's container state, cgroup and directories, and its sidecar). Returns whether
    /// there was a lease to remove. A run this process holds is not an orphan: use
    /// [`Runtime::remove`](crate::lifecycle::Runtime::remove).
    ///
    /// # Errors
    /// The run is held by this process, its sidecar is another agent's or is not well
    /// formed (it is left alone), or the removal failed (the sidecar stays, and the call
    /// can be made again).
    pub fn remove_orphan(&mut self, run_id: &str) -> Result<bool, RuntimeError> {
        let name = container_name(&self.host_id, run_id)
            .map_err(|_| RuntimeError::new(Fault::Name(run_id.to_owned()).to_string()))?;
        if self.runs.contains_key(&name) {
            return Err(RuntimeError::new(format!("run {run_id} is not an orphan")));
        }
        self.remove_leftover(&name)
            .map_err(|why| RuntimeError::new(format!("remove the leftovers of {name}: {why}")))
    }

    /// Removes the leases of this agent that an agent from before the leases were beside the
    /// runs left in the store (`<store>/js-<tag>-<run id>`), whatever the ledger says: the
    /// sidecar may be gone with the directory still there. No run of this process that is in
    /// the store exists. One that cannot be removed stays for the next start.
    fn sweep_store_leases(&self) {
        if !self.leases_beside {
            return;
        }
        let prefix = self.lease_prefix();
        let left = fs::read_dir(&self.store).into_iter().flatten().flatten();
        for entry in left.filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix)) {
            let _ = js_contain::remove::remove_tree(&entry.path());
        }
    }

    /// Removes the lease `name` of an earlier process, if the ledger holds it as this
    /// agent's. Returns whether it did.
    pub(super) fn remove_leftover(&self, name: &str) -> Result<bool, String> {
        match self.ledger.read(name) {
            None => Ok(false),
            Some(Read::Mine(entry)) => {
                self.ledger.discard(&entry, |entry| self.tear_down(entry))?;
                Ok(true)
            }
            Some(Read::Foreign(entry)) => Err(format!("it belongs to agent {:?}", entry.agent)),
            Some(Read::Malformed(why)) => Err(format!("its sidecar is not well formed: {why}")),
        }
    }

    /// Removes everything the lease of `entry` made, the supervisor it names first (with its
    /// `crun run`, by force) and then everything the lease made.
    pub(super) fn tear_down(&self, entry: &Entry) -> Result<(), String> {
        if let Some(supervisor) = &entry.supervisor {
            end_supervisor(supervisor.id());
        }
        self.tear_down_named(&entry.name)
    }

    /// Removes everything the lease `name` made: its supervisor, what still runs in its cgroup,
    /// the container's state, the cgroup and the directories. What is not there is skipped.
    fn tear_down_named(&self, name: &str) -> Result<(), String> {
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, name);
        let lease = self.oci.lease_dirs(name);
        // The supervisor the lease's directory names, for a lease whose sidecar is gone: ended
        // first, or it would write the container's end into the directory being removed. The
        // directory is the job user's, so the file is trusted only as far as it can be checked:
        // the process must run as the job user and be this lease's supervisor by its command
        // line (another lease's names its own lease), or it is left alone.
        let job_uid = self.oci.job_ids().0;
        let named = recorded_supervisor(&lease.dir)
            .filter(|id| id.runs_as(job_uid) && id.supervises(&lease.dir));
        if let Some(id) = named {
            end_supervisor(id);
        }
        // A lease an agent from before the leases were beside the runs left in the store.
        let legacy = self
            .leases_beside
            .then(|| legacy_lease(&self.store, name))
            .flatten();
        let (has_cgroup, has_dir) = (
            cgroup.dir().exists(),
            lease.dir.exists() || legacy.as_ref().is_some_and(|dir| dir.exists()),
        );
        if has_cgroup {
            // A process the supervisor's end did not reach; the removals below retry.
            let _ = cgroup.kill();
        }
        if has_dir {
            step("delete the container", self.oci.remove(name).err())?;
        }
        if has_cgroup {
            let removed = cgroup.remove();
            step(
                "remove the lease's cgroup",
                removed.as_ref().err().map(ToString::to_string),
            )?;
        }
        if has_dir {
            for dir in [Some(&lease.dir), legacy.as_ref()].into_iter().flatten() {
                if dir.exists() {
                    let removed = js_contain::remove::remove_tree(dir);
                    step(
                        "remove the lease directory",
                        removed.as_ref().err().map(ToString::to_string),
                    )?;
                }
            }
        }
        // Last: the image may go only once nothing of the lease can use it.
        let unpinned = self.oci.unpin_image(name);
        self.images.release(name);
        step(
            "remove the image's pin",
            unpinned.as_ref().err().map(ToString::to_string),
        )
    }
}

/// The failure `why` of the step `what`, if it failed, with the step named.
fn step(what: &str, why: Option<String>) -> Result<(), String> {
    why.map_or(Ok(()), |why| Err(format!("{what}: {why}")))
}
