//! The black-box tests of the released Mac package (docs/release.md, "The Mac package's black-box
//! tests"): J2, its first install, and J4 then J3, its update from the release before it with
//! tierstore's mount missing and then present. Each takes the packages as files, installs them
//! as hostkeeper's daemon does and drives the agent only from outside: its launchd job, the lease
//! server it joins and the jobs it runs. Each states its pass condition in numbers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hostkeeper_hook::Verdict;
use hostkeeper_steps::{Finished, Staged};
use js_testkit::dir::TestDir;

use crate::farm;
use crate::host::{HOST_ID, LeaseServer, NATIVE_ACCOUNTS, studio_with};
use crate::hostkeeper::{AGENT, Daemon, SERVICE, STATE_DIR, current_link, service_dir};
use crate::jobs;
use crate::look::{Diagnose, launchd_pid, launchd_state};
use crate::neighbours::Tierstore;
use crate::watch::{self, Watch};

/// J2: the most from the agent's first process to the host shown live with that agent in a
/// session.
pub const FIRST_SESSION: Duration = Duration::from_secs(60);

/// J3: the most from the start of the update's order to the host shown live with the agent of
/// the new version in a session.
pub const UPDATE_SESSION: Duration = Duration::from_secs(120);

/// The most a job is waited for to end, its bundle's first pull included: a bound on a hang.
const JOB_BOUND: Duration = Duration::from_secs(600);

/// How long J4's job runs: past the end of the refused update, so that the job is running all
/// through it.
const LONG_JOB_SECS: &str = "240";

/// How often J4 looks at the agent's process while the refused update runs.
const LOOK_EVERY: Duration = Duration::from_millis(250);

/// The agent's launchd job.
const AGENT_LABEL: &str = "js-agent";

/// A package the test installs: the file, its version, and whether the release signed it (a
/// package of the package repository) or not (one a branch's run built).
pub struct Package {
    /// The `.pkg`.
    pub pkg: PathBuf,
    /// Its version, which its `PackageInfo` and its agent's `--version` carry.
    pub version: String,
    /// Whether its signature is the release's.
    pub signed: bool,
}

impl Package {
    /// The daemon of a host ordered to install this package first.
    fn first(&self) -> Daemon {
        match self.signed {
            true => Daemon::signed(&AGENT, &self.pkg, &self.version),
            false => Daemon::new(&AGENT, &self.pkg, &self.version),
        }
    }

    /// The daemon of the host of `daemon`, ordered to install this package next.
    fn after(&self, daemon: &Daemon) -> Daemon {
        match self.signed {
            true => daemon.another_signed(&AGENT, &self.pkg, &self.version),
            false => daemon.another(&AGENT, &self.pkg, &self.version),
        }
    }
}

/// The host the tests start from: root, and js-agent never installed.
fn fresh(packages: &[&Package]) {
    assert!(
        rustix::process::geteuid().is_root(),
        "the black-box tests install packages and load launchd jobs: run them as root on a disposable Mac"
    );
    for package in packages {
        assert!(
            package.pkg.is_file(),
            "{} is not a file",
            package.pkg.display()
        );
    }
    assert!(
        !service_dir().exists(),
        "{} exists: this is not a host js-agent was never installed on",
        service_dir().display()
    );
    assert!(
        !Path::new(NATIVE_ACCOUNTS).exists(),
        "{NATIVE_ACCOUNTS} exists: this is not a host js-agent was never installed on"
    );
}

/// The daemon's record directory of the agent's order `n`.
fn store(n: u64) -> PathBuf {
    Path::new(STATE_DIR).join(format!("updates/{SERVICE}-{n}"))
}

/// The host: tierstore's stand-in served, the lease server started, and the Studio's files
/// written with the farm's flags.
fn host(scratch: &Path) -> (Tierstore, LeaseServer) {
    let tierstore = Tierstore::serve(scratch);
    let server = studio_with(scratch, &farm::flags);
    (tierstore, server)
}

/// A step run that succeeded, with nothing pending and no reason.
fn assert_succeeded(what: &str, done: &Finished) {
    assert_eq!(
        done.verdict,
        Verdict::Succeeded {
            promotion_pending: false
        },
        "{what}: {done:?}"
    );
    assert_eq!(done.reason, None, "{what}: {done:?}");
}

/// The agent and its root helper run under launchd (the package's third job, the pf anchor's,
/// loads the anchor and exits), and the host is live with the agent of `version`.
fn assert_runs(what: &str, server: &LeaseServer, version: &str) {
    for label in ["js-agent", "js-agent.session"] {
        assert_eq!(
            launchd_state(label).as_deref(),
            Some("running"),
            "{what}: {label} runs under launchd"
        );
    }
    let seen = server.host(HOST_ID);
    assert_eq!(seen["state"], "live", "{what}: {seen}");
    assert_eq!(seen["agent_version"], version, "{what}: {seen}");
}

/// Where the release link points, as its text.
fn current() -> Option<PathBuf> {
    std::fs::read_link(current_link()).ok()
}

/// A job of the smoke bundle's entrypoint that ends 0, with the line it prints.
fn smoke_job(what: &str, server: &LeaseServer) -> Duration {
    let began = Instant::now();
    let id = jobs::submit(server, None, &[what]);
    let out = jobs::assert_ends_zero(server, &id, JOB_BOUND);
    assert!(
        out.starts_with("js-smoke host=") && out.contains(&format!("args={what}")),
        "{what}: the job {id} printed {out:?}"
    );
    began.elapsed()
}

/// J2: the package's first install on a Mac of the farm's shape. hostkeeper's installer stages,
/// checks the signature of and seals the package, the hook's `prepare` sets the host up, its
/// `cutover` starts the agent on the farm's flags beside tierstore's served mount, and the order
/// succeeds.
///
/// Passes when: the order's verdict is `Succeeded`; the host is live with the agent of the
/// package's version in a session at most 60 s ([`FIRST_SESSION`]) after the agent's process was
/// first seen; and a job of the smoke bundle ends by itself with exit status 0 at its first
/// attempt.
///
/// Catches a package whose agent does not start on the farm's Mac flags (a flag it refuses, a
/// neighbour's path it misreads, a plist launchd refuses), one that starts and never joins, one
/// whose signature a Mac does not take, and one that joins and cannot run a job.
pub fn first_install(scratch: &Path, package: &Package) {
    fresh(&[package]);
    let dir = TestDir::under(scratch, "mac-package-first-install");
    let (_tierstore, server) = host(dir.path());
    let daemon = Arc::new(package.first());
    let _diagnose = Diagnose::new(Arc::clone(&daemon.ran), vec![store(1)]);

    let (staged, fresh) = daemon.stage();
    assert!(fresh, "the first order stages the package");
    let order = daemon.order(&staged, 1);
    let watch = Watch::start(server.session_reader());
    let began = Instant::now();
    let done = daemon.run(&order, daemon.switch(true), &store(1));
    let seen = watch.stop();
    println!(
        "J2: the first install of {}, as the host was seen from the order's start:\n{}",
        package.version,
        watch::lines(&seen, began)
    );
    assert_succeeded("J2: the first install", &done);
    assert_eq!(current(), Some(PathBuf::from(&package.version)));
    assert_runs("J2", &server, &package.version);
    let started = watch::first_new_process(&seen, None)
        .expect("J2: an agent process was seen")
        .at;
    let joined = watch::first_session_of(&seen, &package.version)
        .expect("J2: the host was seen live in a session")
        .at;
    let took = joined.saturating_duration_since(started);
    println!(
        "J2: the session began {:.1} s after the agent's process was first seen (at most {} s)",
        took.as_secs_f64(),
        FIRST_SESSION.as_secs()
    );
    assert!(
        took <= FIRST_SESSION,
        "J2: the session began {took:?} after the agent's start, more than {FIRST_SESSION:?}"
    );
    let ran = smoke_job("j2-first-install", &server);
    println!(
        "J2: a job of the smoke bundle ended 0, {:.1} s after its submit",
        ran.as_secs_f64()
    );
}

/// J4, then J3: the package's update from the release before it on a Mac of the farm's shape.
/// The previous package is installed first, as J2 installs a package, and runs a long job.
///
/// J4: tierstore's mount is taken away (no volume and no directory at the trees root) while the
/// job runs, and the update is ordered. Passes when: the order's verdict is `Deferred` with the
/// reason `host_setup_missing`, the hook's `cutover` never ran, the agent's process was the same
/// at every look (every 250 ms) from the order's start to its verdict, the host stayed live in the
/// same session with the previous version, the release link still names the previous version,
/// the job was still running at the verdict, and it ends by itself with exit status 0 at its
/// first attempt.
///
/// J3: the mount is served again and the update is ordered again. Passes when: the verdict is
/// `Succeeded`; the release link names the new version and `previous` the old; the host is live
/// with the agent of the new version in a session at most 120 s ([`UPDATE_SESSION`]) after the
/// order's start; and a job of the smoke bundle ends 0 at its first attempt.
///
/// J4 catches the update of the farm's Macs on the day the trees root was missing: the cutover
/// stopped the agent, the new agent's start check refused the missing mount, the rollback's start
/// of the old agent refused it too, and every Mac left the job service. J3 catches an update that
/// does not bring the agent of the new version up and joined, or leaves it unable to run a job.
pub fn update(scratch: &Path, package: &Package, previous: &Package) {
    assert_ne!(
        package.version, previous.version,
        "the update's two packages are of one version"
    );
    fresh(&[package, previous]);
    let dir = TestDir::under(scratch, "mac-package-update");
    let (tierstore, server) = host(dir.path());
    let first = Arc::new(previous.first());
    let stores = vec![store(1), store(2), store(3)];
    let _diagnose = Diagnose::new(Arc::clone(&first.ran), stores);

    let (installed, fresh) = first.stage();
    assert!(fresh, "the first order stages the previous package");
    let done = first.run(&first.order(&installed, 1), first.switch(true), &store(1));
    assert_succeeded("the first install of the previous release", &done);
    assert_runs("the previous release", &server, &previous.version);

    let next = Arc::new(package.after(&first));
    let (staged, fresh) = next.stage();
    assert!(fresh, "the update stages the new package");
    missing_mount(&server, &tierstore, &next, (&staged, &installed), previous);
    served_mount(&server, &tierstore, &next, (&staged, &installed), previous);
}

/// J4 (see [`update`]).
fn missing_mount(
    server: &LeaseServer,
    tierstore: &Tierstore,
    next: &Arc<Daemon>,
    (staged, installed): (&Staged, &Staged),
    previous: &Package,
) {
    let job = jobs::submit(server, Some("/bin/sleep"), &[LONG_JOB_SECS]);
    let found = jobs::wait_for(server, &job, jobs::RUNNING, JOB_BOUND);
    assert_eq!(found, jobs::RUNNING, "J4: the job runs before the update");
    let pid = launchd_pid(AGENT_LABEL);
    assert!(pid.is_some(), "J4: the previous agent has a process");
    let session = server.session_started(HOST_ID);
    assert!(session.is_some(), "J4: the previous agent is in a session");
    tierstore.take_away();

    let ran_before = next.ran.lock().map(|ran| ran.len()).unwrap_or_default();
    let order = next.order_over(staged, 2, (&previous.version, installed));
    let runner = {
        let next = Arc::clone(next);
        std::thread::spawn(move || next.run(&order, next.switch(true), &store(2)))
    };
    let mut looks = 0u64;
    while !runner.is_finished() {
        let now = launchd_pid(AGENT_LABEL);
        assert_eq!(
            now, pid,
            "J4: the update restarted the agent (its process was {pid:?}, is {now:?}) with tierstore's mount missing"
        );
        looks += 1;
        std::thread::sleep(LOOK_EVERY);
    }
    let done = runner.join().expect("the runner's thread");
    println!(
        "J4: the update with the mount missing ended {:?} ({:?}); the agent's process was {pid:?} at all {looks} looks",
        done.verdict, done.reason
    );
    assert!(
        matches!(done.verdict, Verdict::Deferred { .. }),
        "J4: the update is deferred: {done:?}"
    );
    assert_eq!(
        done.reason.as_deref(),
        Some("host_setup_missing"),
        "J4: {done:?}"
    );
    let units: Vec<String> = next
        .ran
        .lock()
        .map(|ran| {
            ran[ran_before..]
                .iter()
                .map(|step| step.unit.clone())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        units.iter().all(|unit| !unit.contains("-cutover-")),
        "J4: no cutover ran: {units:?}"
    );
    assert_eq!(launchd_pid(AGENT_LABEL), pid, "J4: the same agent runs");
    assert_eq!(
        server.session_started(HOST_ID),
        session,
        "J4: the agent kept its session"
    );
    assert_runs("J4", server, &previous.version);
    assert_eq!(current(), Some(PathBuf::from(&previous.version)));
    assert_eq!(
        jobs::state(server, &job),
        jobs::RUNNING,
        "J4: the job runs on through the refused update"
    );
    jobs::assert_ends_zero(server, &job, JOB_BOUND);
    println!("J4: the job that ran through the refused update ended 0 at its first attempt");
}

/// J3 (see [`update`]).
fn served_mount(
    server: &LeaseServer,
    tierstore: &Tierstore,
    next: &Arc<Daemon>,
    (staged, installed): (&Staged, &Staged),
    previous: &Package,
) {
    tierstore.attach();
    let order = next.order_over(staged, 3, (&previous.version, installed));
    let watch = Watch::start(server.session_reader());
    let began = Instant::now();
    let done = next.run(&order, next.switch(true), &store(3));
    let seen = watch.stop();
    let version = next.version().to_owned();
    println!(
        "J3: the update to {version}, as the host was seen from the order's start:\n{}",
        watch::lines(&seen, began)
    );
    assert_succeeded("J3: the update with the mount served", &done);
    assert_eq!(current(), Some(PathBuf::from(&version)));
    assert_eq!(
        std::fs::read_link(service_dir().join("previous")).ok(),
        Some(PathBuf::from(&previous.version)),
        "J3: previous names the version the update replaced"
    );
    assert_runs("J3", server, &version);
    let joined = watch::first_session_of(&seen, &version)
        .expect("J3: the host was seen live with the new agent in a session")
        .at;
    let took = joined.saturating_duration_since(began);
    println!(
        "J3: the new agent's session began {:.1} s after the order's start (at most {} s)",
        took.as_secs_f64(),
        UPDATE_SESSION.as_secs()
    );
    assert!(
        took <= UPDATE_SESSION,
        "J3: the new agent's session began {took:?} after the order's start, more than {UPDATE_SESSION:?}"
    );
    let ran = smoke_job("j3-update", server);
    println!(
        "J3: a job of the smoke bundle ended 0, {:.1} s after its submit",
        ran.as_secs_f64()
    );
}
