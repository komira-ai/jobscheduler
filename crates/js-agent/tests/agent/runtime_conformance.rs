//! The runtime conformance suite (`tests/runtime_suite`) over `FakeRuntime`: every case
//! runs in every test run. The same cases run over `CrunRuntime` and a real `crun` in the
//! `crun_runtime` test binary.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::rc::Rc;

use js_agent::lifecycle::{
    Capacity, Config, DirLayout, MountRoots, RunHandle, RunSpec, ScratchRoot, StateRoot,
};
use js_testkit::dir::TestDir;

use crate::lifecycle::fake::{FakeRuntime, Log, ManualClock};
use crate::runtime_suite::{self, Program, Shared, Subject, conformance};

/// The image of every run: a digest the fake never looks at.
const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct FakeSubject {
    dir: TestDir,
    fake: FakeRuntime,
    shared: Shared,
}

fn fake_subject(name: &str) -> FakeSubject {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let fake = FakeRuntime::new(&Log::default(), &ManualClock::default()).without_gpus();
    let shared: Shared = Rc::new(RefCell::new(fake.clone()));
    FakeSubject { dir, fake, shared }
}

impl FakeSubject {
    fn made(&self, parts: &[&str]) -> PathBuf {
        let path = parts
            .iter()
            .fold(self.dir.path().to_owned(), |p, part| p.join(part));
        fs::create_dir_all(&path).expect("make a directory");
        path
    }
}

impl Subject for FakeSubject {
    fn runtime(&self) -> Shared {
        Rc::clone(&self.shared)
    }

    fn image(&self) -> String {
        IMAGE.to_owned()
    }

    fn prepared(&mut self) {}

    fn scratch(&mut self, run_id: &str) -> PathBuf {
        self.made(&["runs", run_id])
    }

    fn sync(&mut self, run_id: &str) -> PathBuf {
        self.made(&["sync", run_id])
    }

    fn before(&mut self, _program: Program) {}

    fn after(&mut self, run_id: &str, program: Program) {
        self.fake
            .set_cgroup(run_id, &format!("/sys/fs/cgroup/fake/{run_id}"));
        match program {
            Program::Exit(code) => self.fake.exit(run_id, code),
            Program::OnTerm(code) => self.fake.on_term(run_id, code),
            Program::Hog { own_limit: true } => self.fake.exit_oom(run_id),
            Program::Hog { own_limit: false } => self.fake.exit(run_id, 137),
            Program::Hold | Program::IgnoreTerm => {}
        }
    }

    fn settled(&mut self, _id: &RunHandle) {}

    fn unstarted(&mut self, spec: &RunSpec) -> RunHandle {
        self.fake.stage(spec)
    }

    fn spoil(&mut self, _spec: &mut RunSpec) {
        self.fake.fail("create", 1);
    }

    fn config(&mut self) -> Config {
        let scratch = self.made(&["scratch"]);
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).expect("mode");
        let state = self.made(&["state"]);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("mode");
        let sync = self.made(&["sync"]);
        Config {
            capacity: Capacity {
                cpu_millis: 4000,
                memory_bytes: 8 << 30,
                disk_bytes: 10 << 30,
                gpus: [0, 1].into(),
            },
            allow_host_jobs: false,
            mounts: MountRoots::new(&[("sync".to_owned(), sync)]).expect("mount roots"),
            scratch: ScratchRoot::open_with(&scratch, DirLayout::Private).expect("scratch root"),
            files: None,
            state: StateRoot::open(&state).expect("state root"),
            stop_grace: runtime_suite::LONG,
            bundle: None,
            quota: None,
        }
    }
}

conformance!(fake_subject, any());

/// A runtime that gives only what the trait requires.
struct Bare;

impl js_agent::lifecycle::Runtime for Bare {
    fn create_and_start(
        &mut self,
        _spec: &RunSpec,
    ) -> Result<RunHandle, js_agent::lifecycle::RuntimeError> {
        Err(js_agent::lifecycle::RuntimeError::new("no"))
    }

    fn poll(
        &mut self,
        _id: &RunHandle,
    ) -> Result<js_agent::lifecycle::Status, js_agent::lifecycle::RuntimeError> {
        Ok(js_agent::lifecycle::Status::Gone)
    }

    fn stop(
        &mut self,
        _id: &RunHandle,
        _grace: std::time::Duration,
    ) -> Result<(), js_agent::lifecycle::RuntimeError> {
        Ok(())
    }

    fn remove(&mut self, _id: &RunHandle) -> Result<(), js_agent::lifecycle::RuntimeError> {
        Ok(())
    }

    fn list_labelled(
        &mut self,
    ) -> Result<Vec<js_agent::lifecycle::Found>, js_agent::lifecycle::RuntimeError> {
        Ok(Vec::new())
    }
}

/// Proves the defaults of the trait are what the documentation says: a runtime that says
/// nothing of GPUs supports them, has nothing to check of its network, has no control group
/// for a container, keeps no log, is always ready for an image and lists the ids it lists.
/// Catches a default that changes under the runtimes that rely on it.
#[test]
fn a_runtime_that_overrides_nothing_gets_the_documented_defaults() {
    use js_agent::lifecycle::{Prepared, Runtime as _};
    let mut bare = Bare;
    let id = RunHandle("c1".to_owned());
    assert!(bare.supports_gpus());
    assert_eq!(bare.check_egress(), Ok(()));
    assert_eq!(bare.cgroup(&id), None);
    assert_eq!(bare.log_tail(&id), Ok(None));
    assert_eq!(bare.prepare("image", None), Prepared::Ready);
    assert_eq!(bare.list_ids(), Ok(Vec::new()));
}
