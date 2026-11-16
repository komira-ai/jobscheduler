//! crun as the runtime of the Linux build: a start that names neither `--no-jobs` nor
//! `--allow-container-engine` is a crun start, a host that fails the crun check refuses to start
//! (it is never given the container engine), and the refusal names both ways forward. Each agent runs on a host of its own:
//! the stand-in files of the crun check in a directory of the test (`Fixture`), a scripted
//! `crun --version`, and a fake machine that counts the runtimes it opens.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_agent::crun_host::{CrunHost, start};
use js_agent::integrity::HostView;
use js_agent::lifecycle::Runtime;
use js_agent::platform::NOT_READY;
use js_agent::quota::ScratchQuota;
use js_agent::usage::UsageClock;
use js_agent::{FileReader, Machine, RuntimeBuild, SharedClock};

use crate::crun_host::{Fixture, cli};
use crate::jobs::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Host, Lowest, Ran, Script, ScriptConnector, code, flags, linux_host, refused,
    run_on_defaulted,
};

/// The fake machine, with the crun check reading the test's own host, counting the runtimes the
/// agent opens: a host the check refuses must open none, the engine's included.
struct Counted {
    fake: FakeMachine,
    crun: CrunHost,
    opened: Arc<AtomicUsize>,
}

impl Machine for Counted {
    fn clock(&self) -> SharedClock {
        self.fake.clock()
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        self.fake.runtime(build)
    }

    fn view(&self) -> Box<dyn HostView> {
        self.fake.view()
    }

    fn usage_clock(&self) -> Box<dyn UsageClock> {
        self.fake.usage_clock()
    }

    fn crun_host(&self) -> CrunHost {
        self.crun.clone()
    }

    fn scratch_quota(&self, root: &Path) -> Arc<dyn ScratchQuota> {
        self.fake.scratch_quota(root)
    }

    fn cgroup_files(&self) -> Box<dyn FileReader> {
        self.fake.cgroup_files()
    }
}

/// The host the agent of `fixture` runs on: a Linux host with two GPUs whose `crun --version`
/// answers, whose scratch root has 10 GiB free, and whose state root is on a disk of its own.
fn host_of(fixture: &Fixture) -> Host {
    let mut host = linux_host();
    let version = format!("{} --version", fixture.crun.display());
    host.commands = host
        .commands
        .with(&version, Ok("crun version 1.14.1\ncommit: abc\n"));
    let scratch = fs::canonicalize(&fixture.scratch).expect("the scratch root");
    host.commands = host.commands.with(
        &format!("/bin/df -P -k {}", scratch.display()),
        Ok(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
            /dev/sda1 99999999 100 10485760 11% /\n",
        ),
    );
    host.files = host
        .files
        .with_device_under(&fixture.state, 8)
        .with_device_under(fixture.dir.path(), 7);
    host.euid = rustix::process::geteuid().as_raw();
    host.ancestors_floor = fixture.dir.path().to_owned();
    host
}

/// What an agent on a fixture did: what it printed, how many messages it sent the server, and
/// how many runtimes it opened.
pub(crate) struct Started {
    pub(crate) ran: Ran,
    pub(crate) sent: usize,
    pub(crate) opened: usize,
}

/// Runs the agent on `fixture` with the crun flags of the fixture (job user, `crun`, the roots)
/// and `extra`, exactly as given: a crun start unless `extra` names `--no-jobs` or `--allow-container-engine`. The server refuses
/// the hello for good (`host-bound`), so an agent that gets as far as its hello ends there.
pub(crate) fn start_on(fixture: &Fixture, extra: &[&str]) -> Started {
    let (crun, state, scratch) = (
        fixture.crun.display().to_string(),
        fixture.state.display().to_string(),
        fixture.scratch.display().to_string(),
    );
    let mut all = vec!["--host-id", "h1", "--job-user", Fixture::USER];
    all.extend(["--crun-bin", &crun, "--state-root", &state]);
    all.extend(["--scratch-root", &scratch]);
    all.extend_from_slice(extra);
    let (script, sent) = Script::new(vec![refused(403, "host-bound")]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let opened = Arc::new(AtomicUsize::new(0));
    let machine: Arc<dyn Machine> = Arc::new(Counted {
        fake: FakeMachine::new(),
        crun: fixture.host.clone(),
        opened: opened.clone(),
    });
    let host = host_of(fixture);
    let ran = run_on_defaulted(
        &flags(&all),
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        (machine, None),
    );
    let sent = sent.borrow().len();
    Started {
        ran,
        sent,
        opened: opened.load(Ordering::SeqCst),
    }
}

/// What a row of the refusal table does to the host before the agent starts.
type Break = Box<dyn Fn(&Fixture)>;

/// The fixture's `cgroup.controllers` of the agent's cgroup.
fn controllers(fixture: &Fixture) -> std::path::PathBuf {
    let unit = "system.slice/js-agent.service/cgroup.controllers";
    fixture.host.cgroup_mount.join(unit)
}

/// Proves, one row per failing piece the design names, that an agent started with
/// the crun runtime on a host that fails the crun check exits 2 before it sends anything, names that
/// piece and no other (`1 missing`), says crun is this build's runtime and how to go on, and opens no
/// runtime at all, so no job reaches the container engine. The rows: unprivileged user
/// namespaces restricted (`apparmor_restrict_unprivileged_userns` is 1) with no AppArmor profile
/// for the crun path; and the agent's cgroup not offered `cpu`, `memory` or `pids`, each alone.
/// A last row, the same host with every piece, opens the crun runtime and says hello, so the
/// rows fail for the piece they break. It catches an agent that falls back to the engine,
/// registers anyway, or names a different piece.
#[test]
fn a_host_that_fails_the_crun_check_refuses_to_start() {
    let offered = |without: &str| {
        let all = ["cpuset", "cpu", "io", "memory", "pids"];
        let kept: Vec<&str> = all.into_iter().filter(|c| *c != without).collect();
        format!("{}\n", kept.join(" "))
    };
    // (row, what the row breaks, the piece and the words the refusal names)
    let rows: [(&str, Break, &str, String); 4] = [
        (
            "apparmor",
            Box::new(|fx: &Fixture| fs::write(&fx.host.apparmor_profiles, "snap.x (enforce)\n").unwrap()),
            "apparmor profile",
            "unprivileged user namespaces are restricted (apparmor_restrict_unprivileged_userns=1) and no loaded AppArmor profile covers".to_owned(),
        ),
        (
            "cpu",
            Box::new(move |fx: &Fixture| fs::write(controllers(fx), offered("cpu")).unwrap()),
            "cgroup v2 delegation",
            "is not offered the cpu controller (it is offered: cpuset io memory pids)".to_owned(),
        ),
        (
            "memory",
            Box::new(move |fx: &Fixture| fs::write(controllers(fx), offered("memory")).unwrap()),
            "cgroup v2 delegation",
            "is not offered the memory controller (it is offered: cpuset cpu io pids)".to_owned(),
        ),
        (
            "pids",
            Box::new(move |fx: &Fixture| fs::write(controllers(fx), offered("pids")).unwrap()),
            "cgroup v2 delegation",
            "is not offered the pids controller (it is offered: cpuset cpu io memory)".to_owned(),
        ),
    ];
    for (row, break_it, piece, words) in rows {
        let fx = Fixture::with_ids(&format!("crun-default-fails-{row}"), 990, 991);
        break_it(&fx);
        let started = start_on(&fx, &[]);
        let err = &started.ran.err;
        assert_eq!(started.ran.status(), code(2), "{row}: {err}");
        assert_eq!(started.sent, 0, "{row}: a hello was sent: {err}");
        assert_eq!(started.opened, 0, "{row}: a runtime was opened: {err}");
        assert!(
            err.contains("js-agent: the crun runtime: this host is not ready (1 missing):"),
            "{row}: {err}"
        );
        assert!(err.contains(&format!("\n  - {piece}: ")), "{row}: {err}");
        assert!(err.contains(&words), "{row}: {err}");
        assert!(err.ends_with(&format!("\n{NOT_READY}\n")), "{row}: {err}");
    }
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let fx = Fixture::with_ids("crun-default-passes", uid, gid);
    let started = start_on(&fx, &[]);
    assert!(started.opened > 0, "{}", started.ran.err);
    assert_eq!(started.sent, 1, "{}", started.ran.err);
    assert!(!started.ran.err.contains(NOT_READY), "{}", started.ran.err);
}

/// Proves the text of a refusal: the start check's refusal of a crun start names the missing
/// piece and never the engine (the whole program appends [`NOT_READY`] on a line of its own, as
/// the rows of `a_host_that_fails_the_crun_check_refuses_to_start` assert), and [`NOT_READY`]
/// says crun is this build's runtime and no other is tried, points at
/// docs/runbooks/crun-host-setup.md and names the opt-in to the engine. A crun start with
/// `--container-bin` and no `--allow-container-engine` is refused naming the flag and the opt-in
/// that selects the engine. It catches a refusal that hides the way to the engine, and one that
/// names the engine in place of the missing piece.
#[test]
fn the_refusal_of_a_crun_start_names_both_ways_forward() {
    let fx = Fixture::with_ids("crun-default-refusal", 990, 991);
    fs::write(&fx.host.apparmor_profiles, "snap.x (enforce)\n").unwrap();
    let (crun, state, scratch) = (
        fx.crun.display().to_string(),
        fx.state.display().to_string(),
        fx.scratch.display().to_string(),
    );
    let base = [
        "--job-user",
        Fixture::USER,
        "--crun-bin",
        &crun,
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ];
    let with = |extra: &[&str]| {
        let all = [extra, &base[..]].concat();
        start(&cli(&all), &fx.host, &fx.commands)
            .map(|_| ())
            .unwrap_err()
    };
    let refused = with(&[]);
    assert!(
        refused.starts_with(
            "the crun runtime: this host is not ready (1 missing):\n  - apparmor profile: "
        ),
        "{refused}"
    );
    assert!(!refused.contains("--allow-container-engine"), "{refused}");
    assert!(!refused.contains("docker"), "{refused}");
    for named in [
        "the runtime of this Linux build is crun",
        "docs/runbooks/crun-host-setup.md",
        "--allow-container-engine --container-bin <path>",
        "network none",
    ] {
        assert!(NOT_READY.contains(named), "{named}: {NOT_READY}");
    }
    let engine = with(&["--container-bin", "/usr/bin/docker"]);
    assert_eq!(
        engine,
        "--container-bin: needs --allow-container-engine (a flag of the container engine; \
         without it this agent runs crun)"
    );
}

/// Proves `--no-jobs`, the agent that runs no job: on Linux it registers and says hello with
/// none of the contract's runtime capabilities, where the same start without it is a crun start
/// that refuses this host (it has no job user); it refuses a flag of either runtime by name
/// before it sends anything, and the crun check's flags refuse it as not a crun start. It
/// catches a no-job agent that runs jobs anyway, one that ignores a flag of a runtime it does
/// not run, and a default that is the no-job agent.
#[test]
fn the_agent_that_runs_no_job_is_named_and_takes_no_runtime_flag() {
    let hello = |extra: &[&str]| {
        let (script, sent) = Script::new(vec![refused(403, "host-bound")]);
        let mut sleeper = CountingSleeper::stopping_at(1);
        let ran = run_on_defaulted(
            &flags(extra),
            &linux_host(),
            &ScriptConnector::of(script),
            &mut sleeper,
            &mut Lowest,
            (Arc::new(FakeMachine::new()), None),
        );
        let first = sent.borrow().first().map(|sent| sent.1.clone());
        (ran, first)
    };
    let (ran, sent) = hello(&["--no-jobs"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &sent.expect("a hello")["capabilities"];
    assert!(caps.get("runtime").is_none(), "{caps}");
    assert!(caps.get("exec").is_none(), "{caps}");
    let (ran, sent) = hello(&[]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.is_none(), "{}", ran.err);
    assert!(
        ran.err
            .contains("--job-user: required with the crun runtime"),
        "{}",
        ran.err
    );
    let refusals: [(&[&str], &str); 3] = [
        (
            &["--container-bin", "/bin/sh"],
            "js-agent: --container-bin: not with --no-jobs (an agent that runs no job)\n",
        ),
        (
            &["--job-user", Fixture::USER],
            "js-agent: --job-user: not with --no-jobs (an agent that runs no job)\n",
        ),
        (
            &["--native-helper-socket", "/native/helper.sock"],
            "--native-helper-socket: macOS only (a flag of the native runtime): this Linux \
             build runs its jobs with crun, or with the container engine of \
             --allow-container-engine\n",
        ),
    ];
    for (extra, said) in refusals {
        let all = [&["--no-jobs"][..], extra].concat();
        let (ran, sent) = hello(&all);
        assert_eq!(ran.status(), code(2), "{extra:?}: {}", ran.err);
        assert!(sent.is_none(), "{extra:?}: {}", ran.err);
        assert!(ran.err.ends_with(said), "{extra:?}: {}", ran.err);
    }
    let none = cli(&["--no-jobs", "--job-user", Fixture::USER]);
    let not_crun = js_agent::crun_host::flags(&none).map(|_| ()).unwrap_err();
    assert_eq!(
        not_crun,
        "the crun runtime: not this agent's (--no-jobs or --allow-container-engine)"
    );
}
