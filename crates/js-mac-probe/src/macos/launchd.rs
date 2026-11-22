//! Which children of a launchd job outlive the job (`docs/design/mac-one-program.md`,
//! section 4.3). For each [`Case`], it bootstraps a `KeepAlive` job that starts one child
//! of each [`Kind`], ends the job the case's way, waits until launchd has dealt with that
//! end, and reads with `ps` which children run on and with which parent. Asserted: every
//! child but the one in the job's own group runs on, the job's own with parent pid 1 and the
//! supervisor's child with the supervisor as its parent. Reported with it: each child's state
//! just before the stop, and how each child the job reaped ended. Each case removes
//! its job and ends its children, whatever happened.
//!
//! Unprivileged it runs in the user's `gui/<uid>` domain (`user/<uid>` when the runner has
//! no GUI domain); as root, in the `system` domain, with a child that drops to another uid.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::exec::{Outcome, run};
use crate::launchd::{
    self, Case, Fate, Ids, Kind, Plist, Row, Spawned, Stop, judge, parse_children, parse_instances,
    parse_ps, print_summary,
};
use crate::report::{Probe, Report};

/// How long the job and its children stay if nothing ends them.
const LIFETIME_SECS: u64 = 900;
/// How long the probe waits for one state (the job's children listed, launchd done with
/// the stop, the children settled) before it records that the state never came.
const WAIT: Duration = Duration::from_secs(60);
/// How often it looks while it waits.
const LOOK: Duration = Duration::from_millis(100);

const LAUNCHCTL: &str = "/bin/launchctl";

/// Runs every case in a directory of its own, `dir`, which it makes and removes. With
/// `drop`, it runs in the system domain (root) and adds the dropped child.
pub fn probes(dir: &Path, exe: &Path, drop: Option<Ids>, report: &mut Report) {
    let scope = ["user", "system"][usize::from(drop.is_some())];
    match setup(dir, exe) {
        Ok(copy) => {
            let domains = domains(drop.is_some());
            for case in Case::all() {
                let ctx = Ctx {
                    scope,
                    exe: &copy,
                    dir,
                    drop,
                    domains: &domains,
                };
                report.push(ctx.run(case));
            }
        }
        Err(err) => report
            .push(Probe::asserted(&format!("launchd/{scope}/setup"), false).fact("error", err)),
    }
    // Best effort: every job is gone and every child ended by now.
    let _ = fs::remove_dir_all(dir);
}

/// `dir`, mode 0755, with a copy of the probe in it that every uid may run.
fn setup(dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    let copy = dir.join("js-mac-probe");
    fs::copy(exe, &copy)?;
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755))?;
    Ok(copy)
}

/// The domains to bootstrap into, in the order they are tried.
fn domains(system: bool) -> Vec<String> {
    if system {
        return vec!["system".to_owned()];
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    vec![format!("gui/{uid}"), format!("user/{uid}")]
}

struct Ctx<'a> {
    scope: &'a str,
    exe: &'a Path,
    dir: &'a Path,
    drop: Option<Ids>,
    domains: &'a [String],
}

/// The facts of one case, in the order they were seen.
#[derive(Default)]
struct Facts(Vec<(String, String)>);

impl Facts {
    fn add(&mut self, key: &str, value: impl ToString) {
        self.0.push((key.to_owned(), value.to_string()));
    }

    fn outcome(&mut self, prefix: &str, outcome: &Outcome) {
        let code = outcome.code.map_or("signal".to_owned(), |c| c.to_string());
        self.add(&format!("{prefix}_exit"), code);
        if !outcome.ok {
            self.add(&format!("{prefix}_stderr"), outcome.stderr_tail());
        }
    }

    fn probe(self, name: &str, held: bool) -> Probe {
        let start = Probe::asserted(name, held);
        self.0.into_iter().fold(start, |p, (k, v)| p.fact(&k, v))
    }
}

impl Ctx<'_> {
    fn run(&self, case: Case) -> Probe {
        let name = format!("launchd/{}/{}", self.scope, case.name());
        eprintln!("== {name}");
        let mut facts = Facts::default();
        let state = self.dir.join(case.name());
        let label = format!(
            "js-mac-probe.launchd.{}.{}",
            std::process::id(),
            case.name()
        );
        let plist = match self.write_plist(&state, &label, case.abandon) {
            Ok(path) => path,
            Err(err) => {
                facts.add("error", format!("cannot write the plist: {err}"));
                return facts.probe(&name, false);
            }
        };
        let mut service = None;
        for domain in self.domains {
            let boot = launchctl(&[
                OsString::from("bootstrap"),
                domain.into(),
                plist.clone().into(),
            ]);
            facts.outcome(&format!("bootstrap_{domain}"), &boot);
            if boot.ok {
                service = Some(format!("{domain}/{label}"));
                break;
            }
        }
        let Some(service) = service else {
            return facts.probe(&name, false);
        };
        facts.add("service", &service);
        let held = self.observe(case, &service, &state, &mut facts);
        cleanup(&service, &state);
        facts.probe(&name, held)
    }

    fn write_plist(&self, state: &Path, label: &str, abandon: bool) -> io::Result<PathBuf> {
        fs::create_dir(state)?;
        fs::set_permissions(state, fs::Permissions::from_mode(0o755))?;
        let mut program = vec![
            self.exe.to_string_lossy().into_owned(),
            "child".to_owned(),
            "launchd-job".to_owned(),
            "--state".to_owned(),
            state.to_string_lossy().into_owned(),
            "--secs".to_owned(),
            LIFETIME_SECS.to_string(),
        ];
        if let Some(ids) = self.drop {
            program.extend(["--drop".to_owned(), ids.to_string()]);
        }
        let log = state.join("job.log");
        let text = Plist {
            label,
            program: &program,
            abandon,
            log: &log,
        }
        .render();
        let path = state.join(format!("{label}.plist"));
        fs::write(&path, text)?;
        // launchd refuses a system plist that a group or other may write.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        Ok(path)
    }

    /// From the bootstrapped job to the verdict: whether every child the design needs to
    /// outlive the job did, with launchd seen to be done with the stop.
    fn observe(&self, case: Case, service: &str, state: &Path, facts: &mut Facts) -> bool {
        let expected = Kind::started(self.drop.is_some());
        let Some(children) = wait_for(|| read_children(state)) else {
            facts.add("error", "the job listed no children");
            facts.add("job_log", read_log(state));
            return false;
        };
        let first = instances(state);
        let Some(job) = first.last().copied().filter(|pid| *pid > 1) else {
            facts.add("error", "the job wrote no pid");
            return false;
        };
        facts.add("job_pid_before", job);
        for child in &children {
            let start = match &child.outcome {
                Ok(hello) => hello.to_string(),
                Err(reason) => format!("not started: {reason}"),
            };
            facts.add(&format!("{}_at_start", child.kind.name()), start);
        }
        // Whether each child still ran just before the stop, and any child the job reaped.
        let before = ps(&children);
        let fates = launchd::fates(&children, state, &before);
        let started = children.iter().filter(|child| child.outcome.is_ok());
        for (child, fate) in started.zip(fates) {
            facts.add(&format!("{}_before_stop", child.kind.name()), fate);
        }
        facts.add("ended_before_stop", read_ends(state));
        facts.add("print_before", print(service, "before the stop"));

        let stop = match case.stop {
            Stop::Bootout => launchctl(&[OsString::from("bootout"), service.into()]),
            Stop::Kickstart => {
                launchctl(&[OsString::from("kickstart"), "-k".into(), service.into()])
            }
            Stop::Kill => {
                let mut cmd = Command::new("/bin/kill");
                cmd.arg("-9").arg(job.to_string());
                run(cmd)
            }
        };
        facts.outcome("stop", &stop);
        // launchd has dealt with the end of the job's process (and so with its group) once
        // it has forgotten the job or started it again.
        let handled = wait_until(|| {
            if case.stop.restarts() {
                instances(state).len() > first.len()
            } else {
                !launchctl(&[OsString::from("print"), service.into()]).ok
            }
        });
        facts.add("launchd_done_with_the_stop", handled);
        let settled = wait_until(|| {
            let rows = ps(&children);
            launchd::fates(&children, state, &rows)
                .into_iter()
                .all(|fate| fate.settled(job))
        });
        facts.add("children_settled", settled);
        let rows = ps(&children);
        facts.add(
            "job_pid_after",
            instances(state).last().copied().unwrap_or(0),
        );
        facts.add("print_after", print(service, "after the stop"));
        facts.add("ended_after_stop", read_ends(state));
        let judgement = judge(&expected, &children, state, &rows);
        for (kind, text) in judgement.facts {
            facts.add(&kind, text);
        }
        stop.ok && handled && settled && judgement.held
    }
}

/// Boots the job out if it is still there and ends every child of the case still running.
fn cleanup(service: &str, state: &Path) {
    let _ = launchctl(&[OsString::from("bootout"), service.into()]);
    let children = read_children(state).unwrap_or_default();
    let running = || {
        let rows = ps(&children);
        let fates = launchd::fates(&children, state, &rows);
        let pids = children
            .iter()
            .filter_map(|c| c.outcome.as_ref().ok().map(|h| h.pid));
        let left: Vec<String> = pids
            .zip(fates)
            .filter(|(_, fate)| *fate != Fate::Gone)
            .map(|(pid, _)| pid.to_string())
            .collect();
        left
    };
    let left = running();
    if !left.is_empty() {
        let mut cmd = Command::new("/bin/kill");
        cmd.arg("-9").args(&left);
        let _ = run(cmd);
    }
    if !wait_until(|| running().is_empty()) {
        eprintln!("children still running after cleanup: {:?}", running());
    }
}

fn launchctl(args: &[OsString]) -> Outcome {
    let mut cmd = Command::new(LAUNCHCTL);
    cmd.args(args);
    run(cmd)
}

/// `launchctl print <service>`, in full on stderr and summarised for the report.
fn print(service: &str, when: &str) -> String {
    let out = launchctl(&[OsString::from("print"), service.into()]);
    eprintln!("-- launchctl print {service} ({when}): exit {:?}", out.code);
    eprintln!("{}{}", out.stdout, out.stderr);
    if out.ok {
        print_summary(&out.stdout)
    } else {
        format!("not found ({})", out.stderr_tail())
    }
}

fn read_children(state: &Path) -> Option<Vec<Spawned>> {
    let text = fs::read_to_string(state.join(launchd::CHILDREN)).ok()?;
    Some(parse_children(&text))
}

fn instances(state: &Path) -> Vec<i32> {
    let text = fs::read_to_string(state.join(launchd::INSTANCES)).unwrap_or_default();
    parse_instances(&text)
}

/// The ends file, a child per ` | `: how each child the job reaped ended.
fn read_ends(state: &Path) -> String {
    let text = fs::read_to_string(state.join(launchd::ENDS)).unwrap_or_default();
    text.trim().replace('\n', " | ")
}

fn read_log(state: &Path) -> String {
    let text = fs::read_to_string(state.join("job.log")).unwrap_or_default();
    text.trim().replace('\n', " | ")
}

/// The `ps` rows of the children that started.
fn ps(children: &[Spawned]) -> Vec<Row> {
    let pids: Vec<String> = children
        .iter()
        .filter_map(|c| c.outcome.as_ref().ok().map(|h| h.pid.to_string()))
        .collect();
    if pids.is_empty() {
        return Vec::new();
    }
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-ww", "-o", "pid=,ppid=,pgid=,uid=,command=", "-p"])
        .arg(pids.join(","));
    // `ps` exits 1 when none of the pids is left; its output is still the answer.
    parse_ps(&run(cmd).stdout)
}

fn wait_for<T>(mut found: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(value) = found() {
            return Some(value);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(LOOK);
    }
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    wait_for(|| done().then_some(())).is_some()
}
