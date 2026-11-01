//! A native job through the lifecycle: the start, the exit, the stop and the wall limit
//! end the run as they end a container's, the user is removed with the run, and a start
//! the runtime cannot serve is refused with the code the server reads. The runtime is the
//! real one, over the fake helper.

use std::fs;
use std::sync::Arc;

use js_agent::bundle::{Admission, Placer};
use js_agent::lifecycle::{Event, Lifecycle};
use js_agent::native::NativeChecks;
use js_agent_wire::{Command, InputRef, JobSpec, Outcome, RunFile};
use js_mac_pool::seatbelt::AbsPath;
use serde_json::json;

use super::fake_helper::FakeHelper;
use super::{TERM, native};
use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{cleanup, config_in, names, reply, reply_json};
use crate::support::Scratch;
use js_agent::native::{NativeConfig, NativeRuntime};

/// A lifecycle over a native runtime over a fake helper.
pub(super) struct Native {
    pub(super) life: Lifecycle,
    pub(super) fake: FakeHelper,
    pub(super) clock: ManualClock,
    pub(super) dir: Scratch,
}

/// The most steps a test takes for the helper's thread to report an end.
const STEPS: u32 = 50_000_000;

fn canonical(dir: &Scratch, name: &str) -> AbsPath {
    AbsPath::new(&fs::canonicalize(dir.path(name)).unwrap()).unwrap()
}

impl Native {
    fn new(name: &str) -> Self {
        Self::with(name, None)
    }

    /// A lifecycle whose runtime holds the bundle store `bundles` names, and which judges
    /// starts that pin a bundle by the admission it names.
    pub(super) fn with(name: &str, bundles: Option<(Arc<Placer>, Admission)>) -> Self {
        Self::built(name, bundles, |_| {})
    }

    /// As [`Native::with`], with the runtime's configuration changed by `adjust`.
    pub(super) fn built(
        name: &str,
        bundles: Option<(Arc<Placer>, Admission)>,
        adjust: impl FnOnce(&mut NativeConfig),
    ) -> Self {
        Self::assembled(name, bundles, adjust, true)
    }

    /// As [`Native::built`], over a runtime opened without the refusal of a cap of `on` that
    /// has no check.
    pub(super) fn ungated(name: &str, adjust: impl FnOnce(&mut NativeConfig)) -> Self {
        Self::assembled(name, None, adjust, false)
    }

    fn assembled(
        name: &str,
        bundles: Option<(Arc<Placer>, Admission)>,
        adjust: impl FnOnce(&mut NativeConfig),
        gated: bool,
    ) -> Self {
        let dir = Scratch::new(&format!("native-life-{name}"));
        let mut config = config_in(&dir, false);
        let fake = FakeHelper::start(&dir);
        let mut runtime_config = super::config(&fake);
        runtime_config.read_denies = vec![canonical(&dir, "state"), canonical(&dir, "scratch")];
        adjust(&mut runtime_config);
        let opened = match gated {
            true => NativeRuntime::open(fake.client(), runtime_config, TERM),
            false => NativeRuntime::open_ungated(fake.client(), runtime_config, TERM),
        };
        let mut runtime = opened.unwrap();
        if let Some((store, admission)) = bundles {
            config.bundle = Some(admission);
            runtime = runtime.with_bundles(store);
        }
        let clock = ManualClock::default();
        let (life, _) = Lifecycle::adopt(
            config,
            Box::new(runtime),
            Box::new(NativeChecks),
            Box::new(clock.clone()),
        )
        .unwrap();
        Self {
            life,
            fake,
            clock,
            dir,
        }
    }

    pub(super) fn apply(&mut self, commands: Vec<Command>) -> Vec<String> {
        names(&self.life.apply(&reply(commands)))
    }

    pub(super) fn start(&mut self, spec: JobSpec) -> Vec<String> {
        self.apply(vec![Command::Start(Box::new(spec))])
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
        panic!("the lifecycle never got there: the fake helper never reported the end");
    }

    fn scratch_of(&self, run_id: &str) -> std::path::PathBuf {
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

/// Asserts the whole life of a native job: it starts, its exit makes a result with the
/// code and the tail of what it printed, the cleanup runs in the container order (the
/// helper is asked to kill and delete the user, the scratch directory goes, the checks
/// pass, the slot is freed), nothing of the user is left, and the acknowledged result is
/// forgotten. It catches a native job that runs without the lifecycle's accounting, a
/// result without its log, and a lease user left on the Mac.
#[test]
fn a_native_job_runs_to_its_result_and_its_user_is_removed() {
    let mut h = Native::new("run");
    h.fake.print(b"hello\n", b"");
    assert_eq!(h.start(job("r1", "/bin/echo")), ["started r1"]);
    assert_eq!(h.life.held().len(), 1);
    assert!(h.scratch_of("r1").is_dir());
    h.fake.exit("7.0", 0);
    let events = h.until(|life| life.busy_slots().is_empty());
    let mut want = vec!["result r1".to_owned()];
    want.extend(cleanup("r1"));
    assert_eq!(names(&events), want);
    let result = &h.life.results()[0];
    assert_eq!(
        (result.outcome, result.exit_code),
        (Outcome::Succeeded, Some(0))
    );
    assert_eq!(result.log_tail.as_ref().unwrap().stdout, "hello\n");
    assert_eq!(
        h.fake.verbs(),
        [
            "boot-reset",
            "user-create 7.0",
            "run 7.0",
            "kill-uid 7.0",
            "user-delete 7.0"
        ]
    );
    assert!(!h.scratch_of("r1").exists());
    assert!(!h.fake.home_of("7.0").exists());
    let ack = reply_json(&json!({ "result_acks": [{ "run_id": "r1", "ack": "ACCEPTED" }] }));
    h.life.apply(&ack);
    assert!(h.life.held().is_empty());
}

/// Asserts that a stop kills the user through the helper and the run ends as stopped, and
/// that the removal kills again before it deletes. It catches a stop that leaves the
/// program running and a result that reports the kill's signal as the job's own failure.
#[test]
fn a_stop_kills_the_user_and_the_run_ends_as_killed() {
    let mut h = Native::new("stop");
    h.start(job("r1", "/bin/sleep"));
    assert!(
        h.apply(vec![Command::Stop {
            run_id: "r1".to_owned()
        }])
        .is_empty()
    );
    h.until(|life| !life.results().is_empty());
    let result = &h.life.results()[0];
    assert_eq!((result.outcome, result.exit_code), (Outcome::Killed, None));
    assert_eq!(
        h.fake.verbs()[3..],
        ["kill-uid 7.0", "kill-uid 7.0", "user-delete 7.0"]
    );
}

/// Asserts that a run past its wall limit is killed and reported timed out. It catches a
/// native job that runs for ever.
#[test]
fn a_run_past_its_wall_limit_is_killed_and_timed_out() {
    let mut h = Native::new("wall");
    h.start(job("r1", "/bin/sleep"));
    h.clock.advance(60_000);
    h.until(|life| !life.results().is_empty());
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::TimedOut);
    assert_eq!(h.fake.verbs()[3], "kill-uid 7.0");
}

/// Asserts the starts this runtime refuses before it makes anything, each with the code
/// the server reads: a program outside the tool roots (`invalid-program`), inputs, a
/// working directory and outputs it cannot stage (`native-unsupported`), and run files,
/// which only a container is given (`files-need-container`). Nothing is made: the helper
/// sees no user, the scratch directory is not made and the booking is intact. It catches a
/// refused start that still costs a user, and a job whose inputs or files are dropped
/// without a word.
#[test]
fn a_start_the_runtime_cannot_serve_is_refused_and_makes_nothing() {
    let input = InputRef::Mount {
        root_name: "data".to_owned(),
        rel: "x".to_owned(),
        dest: "in".to_owned(),
    };
    let outputs = js_agent_wire::OutputSpec {
        path: "out".to_owned(),
        kind: js_agent_wire::OutputKind::File,
    };
    let table: Vec<(&str, JobSpec)> = vec![
        ("invalid-program", job("r1", "/opt/tool/run")),
        ("native-unsupported", {
            let mut spec = job("r1", "/bin/sh");
            spec.native.as_mut().unwrap().inputs.push(input);
            spec
        }),
        ("native-unsupported", {
            let mut spec = job("r1", "/bin/sh");
            spec.native.as_mut().unwrap().outputs.push(outputs);
            spec
        }),
        ("native-unsupported", {
            let mut spec = job("r1", "/bin/sh");
            spec.native.as_mut().unwrap().cwd = "sub".to_owned();
            spec
        }),
        ("files-need-container", {
            let mut spec = job("r1", "/bin/sh");
            spec.files = vec![RunFile {
                name: "token".to_owned(),
                contents: "secret".to_owned(),
            }];
            spec
        }),
    ];
    for (n, (code, spec)) in table.into_iter().enumerate() {
        let mut h = Native::new(&format!("refuse{n}"));
        assert_eq!(h.start(spec), [format!("refused r1 {code}")], "{code}");
        assert_eq!(h.fake.verbs(), ["boot-reset"]);
        assert!(!h.scratch_of("r1").exists());
        assert_eq!(h.life.free().cpu_millis, 4000);
        let result = &h.life.results()[0];
        assert_eq!(result.outcome, Outcome::FailedToStart);
        assert_eq!(result.detail.as_deref(), Some(code));
    }
}
