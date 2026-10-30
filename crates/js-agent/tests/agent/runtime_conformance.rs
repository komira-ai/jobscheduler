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
use js_agent_wire::LogTail;
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
            Program::Say { hold } => {
                let tail = LogTail {
                    stdout: runtime_suite::OUT_LINE.to_owned(),
                    stderr: runtime_suite::ERR_LINE.to_owned(),
                    truncated: false,
                };
                self.fake.print(run_id, tail);
                if !hold {
                    self.fake.exit(run_id, 0);
                }
            }
            Program::Flood => {
                let end = runtime_suite::FLOOD_END;
                self.fake.print(
                    run_id,
                    LogTail {
                        stdout: format!("{}{end}", "0123456789\n".repeat(700)),
                        stderr: String::new(),
                        truncated: true,
                    },
                );
                self.fake.exit(run_id, 0);
            }
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
