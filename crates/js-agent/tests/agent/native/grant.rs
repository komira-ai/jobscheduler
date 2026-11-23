//! Full Disk Access on the native path: the question the agent answers for itself at start,
//! the capability that reports the answer, and the refusal of every native job unless the
//! agent holds the grant. The agent asks the operating system for it itself; without it the
//! runtime has no account pool, and the lifecycle over a runtime of the test's pool refuses
//! every native start before an account, a journal line or a scratch directory is made.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::{fs, io};

use js_agent::bundle::{Admission, Placer};
use js_agent::lifecycle::{Event, Lifecycle, NoChecks, Runtime as _};
use js_agent::native::grant;
use js_agent_wire::{Command, Hello, JobSpec, Outcome, Phase, ProtocolRange, WireShape, refusal};
use js_caps::FULL_DISK_ACCESS;
use js_mac_session::full_disk_access::FullDiskAccess;

use super::{FakeAccounts, FakeLauncher, Rig, native};
use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{config_in, names, reply};
use crate::support::Scratch;

/// A lifecycle over the native runtime of a [`Rig`]: its pool is on the rig's fake host and
/// its supervisor is the rig's fake launcher.
pub(super) struct Over {
    pub(super) life: Lifecycle,
    pub(super) launcher: Arc<FakeLauncher>,
    pub(super) accounts: Arc<FakeAccounts>,
    pub(super) dir: Scratch,
}

/// The most steps a test takes for a run to end.
const STEPS: u32 = 50_000_000;

impl Over {
    /// A lifecycle over `rig`'s runtime, which holds the bundle store `bundles` names and
    /// judges starts that pin a bundle by the admission it names.
    pub(super) fn of(rig: Rig, bundles: Option<(Arc<Placer>, Admission)>) -> Self {
        let Rig {
            accounts,
            launcher,
            mut rt,
            dir,
            ..
        } = rig;
        let mut config = config_in(&dir, false);
        if let Some((store, admission)) = bundles {
            config.bundle = Some(admission);
            rt = rt.with_bundles(store);
        }
        let (life, _) = Lifecycle::adopt(
            config,
            Box::new(rt),
            Box::new(NoChecks),
            Box::new(ManualClock::default()),
        )
        .unwrap();
        Self {
            life,
            launcher,
            accounts,
            dir,
        }
    }

    /// Applies a start of `spec` and returns the names of the events.
    pub(super) fn start(&mut self, spec: JobSpec) -> Vec<String> {
        names(
            &self
                .life
                .apply(&reply(vec![Command::Start(Box::new(spec))])),
        )
    }

    /// Steps the lifecycle until `done`, and returns the events of the steps.
    pub(super) fn until(&mut self, done: impl Fn(&Lifecycle) -> bool) -> Vec<Event> {
        let mut events = Vec::new();
        for _ in 0..STEPS {
            events.extend(self.life.step());
            if done(&self.life) {
                return events;
            }
            std::thread::yield_now();
        }
        panic!("the lifecycle never got there: the run never ended");
    }

    /// The scratch directory of the run `run_id`.
    pub(super) fn scratch_of(&self, run_id: &str) -> std::path::PathBuf {
        fs::canonicalize(self.dir.path("scratch"))
            .unwrap()
            .join(run_id)
    }
}

/// A native start of `program`: no image, one CPU, a minute.
pub(super) fn job(run_id: &str, program: &str) -> JobSpec {
    let mut spec = crate::lifecycle::job(run_id);
    spec.image = String::new();
    spec.args = Vec::new();
    spec.native = Some(native(program, &[]));
    spec
}

/// A hello of a Mac host with no capabilities but its os.
fn hello() -> Hello {
    Hello {
        agent_process: None,
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: "mac-1".into(),
        pool: "macs".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            gpus: 0,
            disk_bytes: 0,
            gpu_class: None,
            max_leases: None,
            accelerator: None,
        },
        capabilities: BTreeMap::from([("os".to_owned(), "macos".to_owned())]),
        gpu_devices: Vec::new(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
        identity: None,
    }
}

/// Asserts that the agent judges its own listing of the privacy database's directory: a
/// listing that worked is `granted`, a refusal for want of permission is `not-granted`, and a
/// missing directory or any other failure is `undetermined`. Catches an agent that reads a
/// refusal as a grant, or a missing directory (a host where the check cannot work) as one
/// that lacks the grant.
#[test]
fn the_agent_judges_its_own_listing() {
    assert_eq!(grant::ask(Ok(())), FullDiskAccess::Granted);
    for (kind, state) in [
        (io::ErrorKind::PermissionDenied, FullDiskAccess::NotGranted),
        (io::ErrorKind::NotFound, FullDiskAccess::Undetermined),
        (io::ErrorKind::NotADirectory, FullDiskAccess::Undetermined),
        (io::ErrorKind::Other, FullDiskAccess::Undetermined),
    ] {
        assert_eq!(grant::ask(Err(kind.into())), state, "{kind:?}");
    }
}

/// Asserts that the listing reads a directory, with or without entries, and that a missing
/// path and a file are errors of their own kind, which the judgement then calls
/// `undetermined`. The directory it lists on a Mac is the privacy database's. Catches a
/// listing that answers `Ok` for a path that is not there or for a file, and one that fails
/// on an empty directory.
#[test]
fn the_listing_reads_the_directory_and_nothing_in_it() {
    let dir = Scratch::new("grant-list");
    let empty = dir.path("empty");
    fs::create_dir(&empty).unwrap();
    assert_eq!(grant::list(&empty).map_err(|e| e.kind()), Ok(()));
    let full = dir.path("full");
    fs::create_dir(&full).unwrap();
    fs::write(full.join("TCC.db"), "x").unwrap();
    assert_eq!(grant::list(&full).map_err(|e| e.kind()), Ok(()));
    let missing = dir.path("missing");
    assert_eq!(
        grant::list(&missing).map_err(|e| e.kind()),
        Err(io::ErrorKind::NotFound)
    );
    assert_eq!(
        grant::ask(grant::list(&missing)),
        FullDiskAccess::Undetermined
    );
    let file = full.join("TCC.db");
    assert_eq!(
        grant::list(&file).map_err(|e| e.kind()),
        Err(io::ErrorKind::NotADirectory)
    );
    assert_eq!(
        grant::PRIVACY_DIR,
        "/Library/Application Support/com.apple.TCC"
    );
}

/// Asserts that the capability `full-disk-access` is written into a hello as the state's own
/// spelling, replaces a value that was there, and leaves the hello valid. Catches a host
/// listing that shows no grant state or a spelling the job service does not show verbatim.
#[test]
fn the_state_is_reported_as_a_capability() {
    for (state, text) in [
        (FullDiskAccess::Granted, "granted"),
        (FullDiskAccess::NotGranted, "not-granted"),
        (FullDiskAccess::Undetermined, "undetermined"),
    ] {
        let mut hello = hello();
        hello
            .capabilities
            .insert(FULL_DISK_ACCESS.to_owned(), "stale".to_owned());
        grant::report(&mut hello.capabilities, state);
        assert_eq!(hello.capabilities[FULL_DISK_ACCESS], text);
        assert_eq!(hello.capabilities.len(), 2);
        assert_eq!(hello.validate(), Ok(()), "{text}");
    }
    assert_eq!(FULL_DISK_ACCESS, "full-disk-access");
}

/// Asserts that admission refuses a native body on a runtime whose agent lacks the grant
/// with `no-full-disk-access`, naming the state and the profile to install, and that it does
/// so before every other check: a program under no tool root, a network level above the
/// ceiling and a bundle program on a runtime with no store are all answered the same way. A
/// granted runtime admits the same body. Catches a refusal that comes after another check
/// (so the operator is told of a lesser fault first) and a start admitted without the grant.
#[test]
fn a_runtime_without_the_grant_refuses_every_native_body_first() {
    for state in [FullDiskAccess::NotGranted, FullDiskAccess::Undetermined] {
        let rig = Rig::with("grant-check", |config| config.full_disk_access = state);
        let mut loud = native("/opt/tool/run", &[]);
        loud.network = js_agent_wire::NetworkLevel::Open;
        for body in [
            native("/usr/bin/true", &[]),
            native("/opt/tool/run", &[]),
            native("@bundle/bin/tool", &[]),
            loud,
        ] {
            let refused = rig.rt.check_native(&body).unwrap_err();
            assert_eq!(refused.code, refusal::NO_FULL_DISK_ACCESS);
            assert_eq!(
                refused.why,
                format!(
                    "js-agent does not hold Full Disk Access ({}): install the privacy \
                     profile packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig \
                     through device management and restart the agent",
                    state.as_str()
                )
            );
        }
    }
    let granted = Rig::new("grant-check-granted");
    assert_eq!(granted.rt.check_native(&native("/usr/bin/true", &[])), Ok(()));
    let refused = granted.rt.check_native(&native("/opt/tool/run", &[]));
    assert_eq!(refused.unwrap_err().code, refusal::INVALID_PROGRAM);
}

/// Asserts, through the lifecycle over the real runtime, that a native job start on a host
/// whose state is `not-granted` or `undetermined` is refused `no-full-disk-access`, answered
/// `failed_to_start` with the code as its detail, and makes nothing: no account is lent (the
/// pool's records are as they were), the supervisor is asked for nothing, no lease journal is
/// written, no scratch directory is made and the booking is intact; and that the same start
/// on a granted host runs and does write the journal. Catches a refused job that still costs
/// an account, and a Mac that runs jobs without the grant.
#[test]
fn a_native_job_without_the_grant_is_refused_and_makes_nothing() {
    for state in [FullDiskAccess::NotGranted, FullDiskAccess::Undetermined] {
        let rig = Rig::with("grant-refused", |config| {
            config.full_disk_access = state;
        });
        let before = rig.accounts.records();
        let mut h = Over::of(rig, None);
        assert_eq!(
            h.start(job("r1", "/bin/echo")),
            [format!("refused r1 {}", refusal::NO_FULL_DISK_ACCESS)]
        );
        assert!(h.launcher.launched().is_empty());
        assert_eq!(h.accounts.records(), before);
        assert!(!h.dir.path("native-leases").exists());
        assert!(!h.scratch_of("r1").exists());
        assert_eq!(h.life.free().cpu_millis, 4000);
        let result = &h.life.results()[0];
        assert_eq!(result.outcome, Outcome::FailedToStart);
        assert_eq!(result.detail.as_deref(), Some(refusal::NO_FULL_DISK_ACCESS));
    }
    let mut granted = Over::of(Rig::new("grant-granted"), None);
    assert_eq!(granted.start(job("r1", "/bin/echo")), ["started r1"]);
    assert!(granted.dir.path("native-leases").exists());
    assert_eq!(granted.launcher.launched().len(), 1);
    assert_eq!(granted.launcher.last().program, "/bin/echo");
}
