//! The end of a run's processes: the stop, its grace and the sweep at the poll, and the sweep
//! that comes first in a removal.
//!
//! The runs here are the test user's own processes, and the account's uid is the test
//! user's, so no test sweeps the Mac's real process table by uid: that would end the test
//! runner. The runtime sweeps a [`GroupTable`] instead, which shows only the process groups
//! of the programs this rig's launcher started (each fake supervisor is its job, in a group of
//! its own) and the sweeping process as root, and whose kill is a real `SIGKILL`. A
//! [`Recording`] table on top of it adds processes that are not real (a detached process, one
//! that never dies), and records every listing, every kill and, at each listing, what a probe
//! saw of the run's outputs and account.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent::lifecycle::{Exit, RunHandle, Runtime as _, Status};
use js_agent::mac_sweep::{Proc, ProcessTable};
use js_agent::native::NativeRuntime;
use js_agent_wire::{OutputKind, OutputSpec};
use rustix::process::{Pid, Signal, kill_process};

use super::{FakeLauncher, POLLS, Rig, me, native, settle};

/// `libproc`'s filter of the processes of one process group (`sys/proc_info.h`).
const PROC_PGRP_ONLY: u32 = 2;

/// A pid no process of the test has, for the processes a [`Recording`] adds.
const DETACHED: i32 = 99_001;
/// Another such pid, for a process that never dies.
const STUBBORN: i32 = 99_002;

/// The pids of the process group `group`, from `libproc`.
fn members(group: i32) -> Vec<i32> {
    let Ok(group) = u32::try_from(group) else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; 256];
    let room = i32::try_from(pids.len() * size_of::<libc::pid_t>()).unwrap();
    // SAFETY: `pids` is ours, `room` bytes long, and outlives the call, which writes at most
    // that many bytes and returns how many it wrote.
    let wrote =
        unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), room) };
    if wrote <= 0 {
        return Vec::new();
    }
    pids.truncate(wrote.unsigned_abs() as usize / size_of::<libc::pid_t>());
    pids.retain(|&pid| pid > 1);
    pids
}

/// Waits until the child `pid` has exited, leaving it to be reaped by its owner (the fake
/// supervisor's `try_wait`), so the poll that killed it reads its end at once.
fn exited_not_reaped(pid: i32) {
    let Ok(id) = libc::id_t::try_from(pid) else {
        return;
    };
    // SAFETY: an all-zero `siginfo_t` is a valid value of the plain C struct.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is ours and writable for the call; `WNOWAIT` leaves the child to be
    // reaped by its owner. A pid that is not a child of this process fails with ECHILD and
    // waits for nothing.
    let _ = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
}

/// The sweeping process as a table shows it: root, so the test user's uid (the account's
/// here) is not the sweeper's own.
fn sweeper() -> Proc {
    Proc {
        pid: rustix::process::getpid().as_raw_nonzero().get(),
        uid: 0,
        ruid: 0,
        asid: None,
        name: "js-agent".to_owned(),
    }
}

/// A process of the account that is not real: the table ends it by taking it out.
fn fake(pid: i32, name: &str) -> Proc {
    Proc {
        pid,
        uid: me(),
        ruid: me(),
        asid: None,
        name: name.to_owned(),
    }
}

/// The process groups of the programs a [`FakeLauncher`] started, as the account's
/// processes, and the sweeper. Its kill is a real `SIGKILL`, sent only to a process in one of
/// those groups.
pub struct GroupTable {
    launcher: Arc<FakeLauncher>,
    /// The processes killed: a `SIGKILL` cannot be refused, so they are not shown again.
    ended: Mutex<BTreeSet<i32>>,
}

impl GroupTable {
    /// The table of `launcher`'s programs.
    pub fn new(launcher: Arc<FakeLauncher>) -> Arc<Self> {
        Arc::new(Self {
            launcher,
            ended: Mutex::new(BTreeSet::new()),
        })
    }

    fn ended(&self) -> MutexGuard<'_, BTreeSet<i32>> {
        self.ended.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every live pid of the launcher's groups.
    fn pids(&self) -> Vec<i32> {
        let ended = self.ended().clone();
        let mut pids: Vec<i32> = self
            .launcher
            .groups()
            .into_iter()
            .flat_map(members)
            .filter(|pid| !ended.contains(pid))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

impl ProcessTable for GroupTable {
    fn list(&self, _with_asid: bool) -> io::Result<Vec<Proc>> {
        let mut procs = vec![sweeper()];
        procs.extend(self.pids().into_iter().map(|pid| fake(pid, "job")));
        Ok(procs)
    }

    fn kill(&self, pid: i32) {
        if !self.pids().contains(&pid) {
            return;
        }
        if let Some(target) = Pid::from_raw(pid) {
            let _ = kill_process(target, Signal::KILL);
        }
        if self.launcher.groups().contains(&pid) {
            exited_not_reaped(pid);
        }
        self.ended().insert(pid);
    }

    fn pause(&self) {
        std::thread::yield_now();
    }
}

#[derive(Default)]
struct Seen {
    /// Processes added to the group table's, and those of them that never die.
    extra: Vec<Proc>,
    stubborn: BTreeSet<i32>,
    /// Whether the next listings fail.
    failing: bool,
    listings: usize,
    kills: Vec<i32>,
    /// What the probe said at each listing.
    probes: Vec<String>,
}

/// A [`GroupTable`] with processes added, and a record of its use.
pub struct Recording {
    inner: Arc<GroupTable>,
    seen: Mutex<Seen>,
    probe: Box<dyn Fn() -> String + Send + Sync>,
}

impl Recording {
    /// A recording table over `launcher`'s groups; `probe` is asked at every listing.
    fn new(
        launcher: Arc<FakeLauncher>,
        probe: impl Fn() -> String + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: GroupTable::new(launcher),
            seen: Mutex::new(Seen::default()),
            probe: Box::new(probe),
        })
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The table also shows `process`, which ends at its first kill.
    fn add(&self, process: Proc) {
        self.seen().extra.push(process);
    }

    /// The table also shows `process`, which never ends until [`Recording::release`].
    fn add_stubborn(&self, process: Proc) {
        let mut seen = self.seen();
        seen.stubborn.insert(process.pid);
        seen.extra.push(process);
    }

    /// The process `pid` ends at its next kill.
    fn release(&self, pid: i32) {
        self.seen().stubborn.remove(&pid);
    }

    fn listings(&self) -> usize {
        self.seen().listings
    }

    fn kills(&self) -> Vec<i32> {
        self.seen().kills.clone()
    }

    fn probes(&self) -> Vec<String> {
        self.seen().probes.clone()
    }
}

impl ProcessTable for Recording {
    fn list(&self, with_asid: bool) -> io::Result<Vec<Proc>> {
        let probe = (self.probe)();
        let (extra, failing) = {
            let mut seen = self.seen();
            seen.listings += 1;
            seen.probes.push(probe);
            (seen.extra.clone(), seen.failing)
        };
        if failing {
            return Err(io::Error::other("the table cannot be read"));
        }
        let mut procs = self.inner.list(with_asid)?;
        procs.extend(extra);
        Ok(procs)
    }

    fn kill(&self, pid: i32) {
        let added = {
            let mut seen = self.seen();
            seen.kills.push(pid);
            let added = seen.extra.iter().any(|p| p.pid == pid);
            if added && !seen.stubborn.contains(&pid) {
                seen.extra.retain(|p| p.pid != pid);
            }
            added
        };
        if !added {
            self.inner.kill(pid);
        }
    }

    fn pause(&self) {
        self.inner.pause();
    }
}

/// A shell script that ignores `SIGTERM`, writes its pid to `marker` once it does, then
/// sleeps (an ignored signal stays ignored across `exec`).
fn ignoring_term(marker: &Path) -> String {
    let part = marker.with_extension("part");
    format!(
        "trap '' TERM; echo $$ > {part}; mv {part} {marker}; exec /bin/sleep 600",
        part = part.display(),
        marker = marker.display()
    )
}

/// Waits (by polls of the file, not by time) until the job has written `marker`, and returns
/// the pid it wrote.
fn pid_in(marker: &Path) -> i32 {
    for _ in 0..POLLS {
        if let Ok(text) = fs::read_to_string(marker) {
            return text.trim().parse().expect("a pid");
        }
        std::thread::yield_now();
    }
    panic!("the job never wrote {}", marker.display());
}

/// A runtime of `rig` reading the clock `now` and sweeping `table`.
fn runtime(rig: Rig, now: &Arc<AtomicU64>, table: Arc<Recording>) -> (Rig, NativeRuntime) {
    let clock = now.clone();
    let rt = NativeRuntime::over(
        Some(rig.pool.clone()),
        rig.launcher.clone(),
        super::config(&rig.dir, &rig.settings),
        super::TERM + 10,
    )
    .unwrap()
    .with_clock(Arc::new(move || clock.load(Ordering::Relaxed)))
    .with_process_table(table);
    (rig, rt)
}

/// A started job that ignores `SIGTERM`, in a rig named `name`, with the clock at 1000 ms and
/// a recording table (whose probe says nothing); returns its pid too.
fn started(
    name: &str,
) -> (
    Rig,
    NativeRuntime,
    Arc<AtomicU64>,
    Arc<Recording>,
    RunHandle,
    i32,
) {
    let rig = Rig::new(name);
    let now = Arc::new(AtomicU64::new(1_000));
    let table = Recording::new(rig.launcher.clone(), String::new);
    let (rig, mut rt) = runtime(rig, &now, table.clone());
    let marker = rig.dir.path("job-pid");
    let spec = rig.spec("r1", native("/bin/sh", &["-c", &ignoring_term(&marker)]));
    let id = rt.create_and_start(&spec).unwrap();
    let pid = pid_in(&marker);
    (rig, rt, now, table, id, pid)
}

fn killed() -> Status {
    Status::Exited(Exit {
        code: 128 + 9,
        oom_killed: false,
    })
}

/// A stop leaves the run running and sweeps nothing (no listing at all) until its grace has
/// passed on the injected clock; the poll at the grace's end sweeps the job (the fake
/// supervisor, the agent's child) and a detached process of the account, and answers the
/// killed supervisor's status, 137, so the lifecycle reads the run's log tail as for any exit.
///
/// Catches: a sweep at the stop (no grace), no sweep at all, a sweep by a thread that sleeps
/// out the grace, and an escalated run answered `Gone`.
#[test]
fn stop_then_the_grace_then_the_sweep() {
    let (_rig, mut rt, now, table, id, pid) = started("sweep-grace");
    table.add(fake(DETACHED, "leave"));
    rt.stop(&id, std::time::Duration::from_millis(500)).unwrap();
    assert_eq!(rt.poll(&id).unwrap(), Status::Running, "within the grace");
    now.store(1_499, Ordering::Relaxed);
    assert_eq!(
        rt.poll(&id).unwrap(),
        Status::Running,
        "a moment before its end"
    );
    assert_eq!(table.listings(), 0, "swept within the grace");
    now.store(1_500, Ordering::Relaxed);
    assert_eq!(rt.poll(&id).unwrap(), killed());
    let kills = table.kills();
    assert_eq!(
        kills.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([pid, DETACHED])
    );
    assert_eq!(kills.len(), 2, "each process killed once: {kills:?}");
    assert_eq!(rt.poll(&id).unwrap(), killed(), "told the same again");
    rt.remove(&id).unwrap();
}

/// A job that ends on the stop's `SIGTERM` within the grace is told as it ended (143), and
/// no poll sweeps it, before the grace's end or after: what it left is the removal's, whose
/// sweep comes first.
///
/// Catches: a poll that sweeps a run that has already ended, and a removal that skips the
/// sweep of a run whose end was read.
#[test]
fn a_job_that_exits_on_term_within_the_grace_is_not_swept_by_poll() {
    let rig = Rig::new("sweep-term");
    let now = Arc::new(AtomicU64::new(1_000));
    let table = Recording::new(rig.launcher.clone(), String::new);
    let (rig, mut rt) = runtime(rig, &now, table.clone());
    let spec = rig.spec("r1", native("/bin/sleep", &["600"]));
    let id = rt.create_and_start(&spec).unwrap();
    rt.stop(&id, std::time::Duration::from_millis(500)).unwrap();
    let ended = Status::Exited(Exit {
        code: 128 + 15,
        oom_killed: false,
    });
    assert_eq!(settle(&mut rt, &id), ended);
    now.store(2_000, Ordering::Relaxed);
    assert_eq!(rt.poll(&id).unwrap(), ended);
    assert_eq!(table.listings(), 0, "the poll swept an ended run");
    rt.remove(&id).unwrap();
    assert!(table.listings() >= 1, "the removal did not sweep");
}

/// A second stop moves neither the grace's end nor anything else: the sweep comes at the
/// first stop's grace.
///
/// Catches: a stop that pushes the kill back each time the lifecycle stops again.
#[test]
fn a_second_stop_does_not_move_kill_after() {
    let (_rig, mut rt, now, table, id, pid) = started("sweep-second-stop");
    rt.stop(&id, std::time::Duration::from_millis(500)).unwrap();
    now.store(1_400, Ordering::Relaxed);
    rt.stop(&id, std::time::Duration::from_millis(500)).unwrap();
    assert_eq!(rt.poll(&id).unwrap(), Status::Running);
    now.store(1_500, Ordering::Relaxed);
    assert_eq!(rt.poll(&id).unwrap(), killed());
    assert_eq!(table.kills(), [pid]);
    rt.remove(&id).unwrap();
}

/// A sweep that is not clean (a process that does not die) leaves the run running, though
/// its supervisor has been killed, and the next poll sweeps again; once the process dies,
/// the next poll's sweep is clean and the run is told as its supervisor ended.
///
/// Catches: an end told while the job's processes are still alive, and an escalation that
/// sweeps once and never again.
#[test]
fn an_unclean_sweep_is_tried_again_at_the_next_poll() {
    let (_rig, mut rt, now, table, id, pid) = started("sweep-again");
    table.add_stubborn(fake(STUBBORN, "stuck"));
    rt.stop(&id, std::time::Duration::from_millis(500)).unwrap();
    now.store(1_500, Ordering::Relaxed);
    assert_eq!(rt.poll(&id).unwrap(), Status::Running, "not clean");
    let first = table.listings();
    assert!(table.kills().contains(&pid));
    assert_eq!(rt.poll(&id).unwrap(), Status::Running, "still not clean");
    assert!(
        table.listings() > first,
        "the next poll did not sweep again"
    );
    table.release(STUBBORN);
    assert_eq!(rt.poll(&id).unwrap(), killed());
    rt.remove(&id).unwrap();
}

/// The removal sweeps before anything else: at every listing of its sweep the run's
/// declared output is not yet copied out and the account's record is still there; after the
/// removal the output is copied and the record gone, and the detached process the job left
/// (still running after the job's own exit) was ended.
///
/// Catches: outputs collected while a detached process still writes into the job's folder,
/// and an account retired before its processes were swept.
#[test]
fn remove_sweeps_before_outputs_are_collected_and_before_the_account_is_retired() {
    let rig = Rig::new("sweep-remove");
    let now = Arc::new(AtomicU64::new(1_000));
    let collected = rig
        .dir
        .path("scratch")
        .join("r1")
        .join("outputs")
        .join("out.txt");
    let accounts = rig.accounts.clone();
    let seen_out = collected.clone();
    let table = Recording::new(rig.launcher.clone(), move || {
        format!(
            "collected={} records={:?}",
            seen_out.exists(),
            accounts.records()
        )
    });
    let (rig, mut rt) = runtime(rig, &now, table.clone());
    let mut body = native("/bin/sh", &["-c", "echo done > out.txt"]);
    body.outputs = vec![OutputSpec {
        path: "out.txt".to_owned(),
        kind: OutputKind::File,
    }];
    let id = rt.create_and_start(&rig.spec("r1", body)).unwrap();
    assert_eq!(
        settle(&mut rt, &id),
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        })
    );
    let name = rig
        .launcher
        .last()
        .var("USER")
        .expect("the account")
        .to_owned();
    table.add(fake(DETACHED, "leave"));
    rt.remove(&id).unwrap();
    let probes = table.probes();
    assert!(!probes.is_empty(), "the removal did not sweep");
    for probe in &probes {
        assert!(probe.starts_with("collected=false "), "{probe}");
        assert!(probe.contains(&format!("{name:?}")), "{probe}");
    }
    assert_eq!(table.kills(), [DETACHED]);
    assert_eq!(fs::read_to_string(&collected).unwrap(), "done\n");
    assert!(
        !rig.accounts.records().contains(&name),
        "the account was not retired"
    );
}

/// A sweep that cannot read the table is not clean, and the removal still goes on to the
/// retirement; a retirement the pool holds (the record cannot be deleted) fails the removal
/// and keeps the run, and the removal tried again sweeps again and retires.
///
/// Catches: a removal that stops at a failed sweep (the account never retired), and one that
/// takes a held account for a removed run.
#[test]
fn a_failed_sweep_still_retires_the_account_and_reports_held_when_retire_does() {
    let rig = Rig::new("sweep-held");
    let now = Arc::new(AtomicU64::new(1_000));
    let table = Recording::new(rig.launcher.clone(), String::new);
    let (rig, mut rt) = runtime(rig, &now, table.clone());
    let id = rt
        .create_and_start(&rig.spec("r1", native("/bin/sh", &["-c", "exit 0"])))
        .unwrap();
    let _ = settle(&mut rt, &id);
    let name = rig
        .launcher
        .last()
        .var("USER")
        .expect("the account")
        .to_owned();

    table.seen().failing = true;
    rig.accounts.keep_record(&name);
    let held = rt
        .remove(&id)
        .expect_err("a held account fails the removal");
    assert!(held.0.contains(&name), "{}", held.0);
    assert_eq!(
        table.listings(),
        1,
        "one sweep, ended by the failed listing"
    );
    assert!(rig.accounts.records().contains(&name));

    table.seen().failing = false;
    rig.accounts.free_record(&name);
    rt.remove(&id).unwrap();
    assert_eq!(table.listings(), 2, "the removal tried again swept again");
    assert!(!rig.accounts.records().contains(&name));
}
