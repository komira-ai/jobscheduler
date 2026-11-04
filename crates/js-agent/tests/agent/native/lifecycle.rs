//! A native job through the lifecycle: the start, the exit, the stop and the wall limit
//! end the run as they end a container's, the user is removed with the run, and a start
//! the runtime cannot serve is refused with the code the server reads. The runtime is the
//! real one, over the fake helper.

use std::fs;
use std::sync::Arc;

use js_agent::bundle::{Admission, Placer, Registry, Settings};
use js_agent::lifecycle::{Event, Lifecycle};
use js_agent::native::{Ceiling, Enforcer, NativeChecks};
use js_agent_wire::{Command, InputRef, JobSpec, NetworkLevel, Outcome, RunFile, RunSpec};
use js_mac_pool::seatbelt::AbsPath;
use js_oci::{Allow, Digest};
use js_types::{Arch, Os};
use serde_json::json;

use super::fake_helper::FakeHelper;
use super::pf_gate::{Fake, Verdict};
use super::{TERM, native, parse};
use crate::bundle::image::{Served, rig};
use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{cleanup, config_in, names, reply, reply_json};
use crate::support::Scratch;
use js_agent::native::{NativeConfig, NativeRuntime};

/// A named change to the agent's bundle settings, made against the registry served.
type SettingsCase = (&'static str, fn(&mut Settings, &Served));
/// A named change to a start.
type SpecCase = (&'static str, fn(&mut JobSpec));

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

/// Asserts that a start named by its image, an image alone and an image with a `run`,
/// `os` and `arch`, is refused by a native runtime that holds no bundle store with
/// `image-unsupported-here` before anything is made: the helper sees no user, no run
/// directory is made and the booking is intact. It catches an image start answered
/// `native-unsupported` (the code of a native body this runtime cannot stage), one that
/// reaches the helper and fails late, and a runtime without a store that claims it can run
/// the image.
#[test]
fn an_image_start_on_the_native_path_is_refused_with_a_typed_code() {
    let image_only = crate::lifecycle::job("r1");
    let mut with_run = crate::lifecycle::job("r1");
    with_run.args = Vec::new();
    with_run.run = Some(RunSpec::from(native("/bin/sh", &[])));
    with_run.os = Some(Os::Macos);
    with_run.arch = Some(Arch::Arm64);
    for (n, spec) in [image_only, with_run].into_iter().enumerate() {
        let mut h = Native::new(&format!("image{n}"));
        assert_eq!(h.start(spec), ["refused r1 image-unsupported-here"], "{n}");
        assert_eq!(h.fake.verbs(), ["boot-reset"]);
        assert!(!h.scratch_of("r1").exists());
        assert_eq!(h.life.free().cpu_millis, 4000);
        let result = &h.life.results()[0];
        assert_eq!(result.outcome, Outcome::FailedToStart);
        assert_eq!(result.detail.as_deref(), Some("image-unsupported-here"));
    }
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

/// A lifecycle over a runtime with a bundle store that fetches from a registry in memory
/// (`--bundle-pull missing`), whose admission allows the registry's `team/` namespace for
/// images unless a test says otherwise, and the registry that serves the bundle by an index.
struct Imaged {
    h: Native,
    served: Served,
    store: Arc<Placer>,
    _dir: Scratch,
}

/// As [`imaged_with`], with the registry's namespace allowed and nothing changed.
fn imaged(name: &str) -> Imaged {
    imaged_with(name, true, |_, _| {}, |_| {})
}

/// A lifecycle over a store and its registry. The image entry of the registry's namespace is
/// allowed only when `allowed` says so; `change` then alters the bundle settings and
/// `adjust` the runtime's configuration.
fn imaged_with(
    name: &str,
    allowed: bool,
    change: impl FnOnce(&mut Settings, &Served),
    adjust: impl FnOnce(&mut NativeConfig),
) -> Imaged {
    let dir = Scratch::new(&format!("native-image-{name}"));
    let served = Served::new(&dir);
    let entries = if allowed {
        vec![served.entry()]
    } else {
        Vec::new()
    };
    let (store, admission) = rig(&dir, &served, &entries, |rules| change(rules, &served));
    let h = Native::built(name, Some((Arc::clone(&store), admission)), adjust);
    Imaged {
        h,
        served,
        store,
        _dir: dir,
    }
}

impl Imaged {
    /// The image reference that names the index.
    fn image(&self) -> String {
        self.served.image()
    }

    /// Starts a job that begins the fetch of the bundle (refused `bundle-pending`) and waits
    /// until the store holds it.
    fn fetched(&mut self) {
        let spec = image_job("r0", &self.image(), "@bundle/tool");
        assert_eq!(self.h.start(spec), ["refused r0 bundle-pending"]);
        self.store.settle();
    }

    /// The result of the run `run_id`.
    fn result_of(&self, run_id: &str) -> (Outcome, Option<String>) {
        let results = self.h.life.results();
        let found = results.iter().find(|result| result.run_id == run_id);
        let result = found.expect("a result for the run");
        (result.outcome, result.detail.clone())
    }

    /// Asserts the helper was asked for nothing but its boot reset: no user was made.
    fn made_no_user(&self) {
        assert_eq!(self.h.fake.verbs(), ["boot-reset"]);
    }
}

/// A start named by `image` that runs `program` of the bundle it resolves to, on macOS and
/// arm64.
fn image_job(run_id: &str, image: &str, program: &str) -> JobSpec {
    let mut spec = crate::lifecycle::job(run_id);
    spec.image = image.to_owned();
    spec.args = Vec::new();
    spec.run = Some(RunSpec::from(native(program, &[])));
    spec.os = Some(Os::Macos);
    spec.arch = Some(Arch::Arm64);
    spec
}

/// The names in the directory `dir`, ascending.
fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Two jobs that name the same image run from the one pinned bundle and the registry is
/// asked once: the first start begins the fetch (refused `bundle-pending`), and the jobs that
/// follow, one after the other and then two at once, fetch nothing more (the request log of
/// the registry does not grow), see the same tree paths for the read-only trees, and each
/// has its own copy of the writable tree; the pins end with the runs. It catches a second
/// job that fetches the bundle again, a writable tree shared between two runs, and a pin
/// that outlives its run.
#[test]
fn a_second_job_reuses_the_pinned_bundle() {
    let mut s = imaged("reuse");
    s.fetched();
    let image = s.image();
    let fetched = s.served.fx.requests();
    let artifact = &s.served.bundle.artifact;
    let manifest = artifact.digest.to_string();
    let blobs = std::iter::once(&artifact.config).chain(&artifact.layers);
    let mut wanted = vec![format!("/manifests/{manifest}")];
    wanted.extend(blobs.map(|blob| format!("/blobs/{}", Digest::of(blob))));
    for path in wanted {
        let asked = fetched.iter().filter(|line| line.contains(&path)).count();
        assert_eq!(asked, 1, "{path} was asked once: {fetched:?}");
    }
    assert_eq!(
        s.h.start(image_job("r1", &image, "@bundle/tool")),
        ["started r1"]
    );
    let tool = parse(&s.h.fake.run_of("7.0").argv).tail[0].clone();
    s.h.fake.exit("7.0", 0);
    s.h.until(|life| life.busy_slots().is_empty());
    for (run_id, lease) in [("r2", "7.1"), ("r3", "7.2")] {
        let started = s.h.start(image_job(run_id, &image, "@bundle/tool"));
        assert_eq!(started, [format!("started {run_id}")]);
        assert!(
            s.store.pin(&image, lease).is_err(),
            "{lease} holds the pin while it runs"
        );
        let run = s.h.fake.run_of(lease);
        assert_eq!(parse(&run.argv).tail[0], tool, "{lease} runs the same tree");
        let home = s.h.fake.home_of(lease);
        let work = home.join("trees/work");
        let copy = ("WORKROOT".to_owned(), work.display().to_string());
        assert!(run.env.contains(&copy), "{:?}", run.env);
        assert!(work.join("run.sh").is_file(), "{lease} has its own copy");
    }
    assert_ne!(
        s.h.fake.home_of("7.1").join("trees/work"),
        s.h.fake.home_of("7.2").join("trees/work")
    );
    for lease in ["7.1", "7.2"] {
        s.h.fake.exit(lease, 0);
    }
    s.h.until(|life| life.busy_slots().is_empty());
    for lease in ["7.0", "7.1", "7.2"] {
        assert!(
            s.store.pin(&image, lease).is_ok(),
            "{lease} released its pin"
        );
    }
    assert_eq!(s.served.fx.requests(), fetched, "no job fetched again");
}

/// Two starts that name the same missing index pin before its fetch ends are both refused
/// `bundle-pending` and share the one fetch: the registry is asked for the index and for the
/// manifest it lists once each, and the bundle is held afterwards. It catches a second
/// download begun for a pin whose fetch is already running.
#[test]
fn two_starts_racing_on_a_missing_index_pin_share_one_fetch() {
    let mut s = imaged("race");
    let image = s.image();
    for run_id in ["r0", "r1"] {
        let spec = image_job(run_id, &image, "@bundle/tool");
        assert_eq!(
            s.h.start(spec),
            [format!("refused {run_id} bundle-pending")]
        );
    }
    s.store.settle();
    let asked = s.served.fx.requests();
    for digest in [s.served.index.to_string(), s.served.bundle.digest()] {
        let path = format!("/manifests/{digest}");
        let count = asked.iter().filter(|line| line.contains(&path)).count();
        assert_eq!(count, 1, "{path} was asked once: {asked:?}");
    }
    assert!(s.store.pin(&image, "lease-1").is_ok(), "the bundle is held");
}

/// A layer the registry serves with one byte changed makes the background fetch fail: the
/// start that began it is refused `bundle-pending`, the next is refused
/// `bundle-fetch-failed` with the failure reported once, no user is made, and the store
/// holds no tree, manifest, config or index alias. It catches a bundle that is stored and
/// run although a byte did not hash to its digest, and a failure that is never reported.
#[test]
fn a_bundle_blob_that_fails_its_digest_is_refused() {
    let mut s = imaged("flipped");
    let layer = &s.served.bundle.artifact.layers[0];
    let mut flipped = layer.clone();
    flipped[0] ^= 1;
    s.served.fx.publish_blob_as(&Digest::of(layer), &flipped);
    let image = s.image();
    assert_eq!(
        s.h.start(image_job("r1", &image, "@bundle/tool")),
        ["refused r1 bundle-pending"]
    );
    s.store.settle();
    assert_eq!(
        s.h.start(image_job("r2", &image, "@bundle/tool")),
        ["refused r2 bundle-fetch-failed"]
    );
    let want = (
        Outcome::FailedToStart,
        Some("bundle-fetch-failed".to_owned()),
    );
    assert_eq!(s.result_of("r2"), want);
    s.made_no_user();
    let objects = s.store.root().join("v1/objects");
    for dir in ["trees", "manifests", "configs", "indexes"] {
        assert!(entries(&objects.join(dir)).is_empty(), "{dir}");
    }
}

/// An image on none of the `--image-allow` entries is refused `image-not-allowed` before any
/// request reaches the registry and before any user is made: with no entry at all, with an
/// entry on another namespace of the same registry, and with the registry listed only in the
/// bundle registries (`--bundle-registry-allow`), which the legacy pin reads and an image
/// does not. It catches an image fetched from a registry the operator did not allow for
/// images.
#[test]
fn a_non_allowed_prefix_is_refused() {
    let cases: [SettingsCase; 3] = [
        ("none", |_, _| {}),
        ("other", |rules, served| {
            let other = format!("{}/other", served.fx.at(js_testkit::oci_registry::REGISTRY));
            rules.image_allow = vec![Allow::parse(&other).unwrap()];
        }),
        ("bundle-list", |rules, served| {
            rules.registries = vec![Registry {
                host: served.fx.at(js_testkit::oci_registry::REGISTRY),
                prefix: "team".to_owned(),
                private: false,
            }];
        }),
    ];
    for (name, change) in cases {
        let mut s = imaged_with(&format!("refused-{name}"), false, change, |_| {});
        let spec = image_job("r1", &s.image(), "@bundle/tool");
        assert_eq!(s.h.start(spec), ["refused r1 image-not-allowed"], "{name}");
        assert!(s.served.fx.requests().is_empty(), "{name}");
        assert_eq!(
            s.result_of("r1"),
            (Outcome::FailedToStart, Some("image-not-allowed".to_owned())),
            "{name}"
        );
        s.made_no_user();
    }
}

/// An image whose bundle is held but whose `run` names an entrypoint the bundle lacks is
/// refused `invalid-program` before the runtime is asked: the helper is never asked for a
/// user, and no pin is left. It catches an entrypoint checked after a user was made for it,
/// and an image start whose program is read from nowhere.
#[test]
fn an_image_reference_whose_entrypoint_is_missing_makes_no_user() {
    let mut s = imaged("no-entrypoint");
    s.fetched();
    let spec = image_job("r1", &s.image(), "@bundle/gcc");
    assert_eq!(s.h.start(spec), ["refused r1 invalid-program"]);
    s.made_no_user();
    assert!(s.store.pin(&s.image(), "7.0").is_ok());
}

/// A start named by its image that this runtime cannot run is refused with the code that says
/// why before its bundle is fetched, though the registry would serve it and the image is
/// allowed: an image with no `run` (`image-unsupported-here`, a bundle has no command of its
/// own), a `run` that stages inputs, collects outputs or names a working directory
/// (`native-unsupported`), and a `run` with run files (`files-need-container`). The registry
/// sees no request and no user is made. It catches an image start that begins a download for
/// a job that can never run, and one answered with the wrong code.
#[test]
fn an_image_start_the_runtime_cannot_run_is_refused_before_its_bundle_is_fetched() {
    let table: [SpecCase; 5] = [
        ("image-unsupported-here", |spec| spec.run = None),
        ("native-unsupported", |spec| {
            spec.run.as_mut().unwrap().cwd = "sub".to_owned();
        }),
        ("native-unsupported", |spec| {
            spec.run.as_mut().unwrap().inputs.push(InputRef::Mount {
                root_name: "data".to_owned(),
                rel: "x".to_owned(),
                dest: "in".to_owned(),
            });
        }),
        ("native-unsupported", |spec| {
            spec.run
                .as_mut()
                .unwrap()
                .outputs
                .push(js_agent_wire::OutputSpec {
                    path: "out".to_owned(),
                    kind: js_agent_wire::OutputKind::File,
                });
        }),
        ("files-need-container", |spec| {
            spec.files = vec![RunFile {
                name: "token".to_owned(),
                contents: "secret".to_owned(),
            }];
        }),
    ];
    for (n, (code, shape)) in table.into_iter().enumerate() {
        let mut s = imaged(&format!("cannot{n}"));
        let mut spec = image_job("r1", &s.image(), "@bundle/tool");
        shape(&mut spec);
        assert_eq!(s.h.start(spec), [format!("refused r1 {code}")], "{code}");
        assert!(s.served.fx.requests().is_empty(), "{code}");
        s.made_no_user();
    }
}

/// An image run that asks for the network is held to the packet-filter check as a native
/// body is: refused `egress-unchecked` while the anchor is not enforced, started when it is.
/// It catches a `run` that skips the network check because it has no `native` body.
#[test]
fn an_image_run_that_asks_for_the_network_is_checked_against_the_anchor() {
    let check = Fake::new(Verdict::PfDisabled);
    let enforcer: Arc<dyn Enforcer> = check.clone();
    let mut s = imaged_with(
        "egress",
        true,
        |_, _| {},
        move |config| {
            config.network = Ceiling::On;
            config.egress = Some(enforcer);
        },
    );
    s.fetched();
    let mut networked = image_job("r1", &s.image(), "@bundle/tool");
    networked.run.as_mut().unwrap().network = NetworkLevel::On;
    assert_eq!(
        s.h.start(networked.clone()),
        ["refused r1 egress-unchecked"]
    );
    let verbs = s.h.fake.verbs();
    assert!(
        !verbs.iter().any(|verb| verb.starts_with("user-create")),
        "{verbs:?}"
    );
    check.answer(Verdict::Enforced);
    networked.run_id = "r2".to_owned();
    assert_eq!(s.h.start(networked), ["started r2"]);
}

/// A start that carries both a `bundle` pin and a `run` is skipped as invalid by the wire
/// (`invalid-bundle`: a bundle needs a native body and no image) and fetches nothing. It
/// catches an image start that is also a bundle start being admitted twice or run.
#[test]
fn a_start_with_a_run_and_a_bundle_pin_is_invalid() {
    let mut s = imaged("both");
    let image = s.image();
    let mut spec = image_job("r1", &image, "@bundle/tool");
    spec.bundle = Some(image);
    assert_eq!(s.h.start(spec), ["invalid #0 invalid-bundle"]);
    assert!(s.served.fx.requests().is_empty());
    s.made_no_user();
}
