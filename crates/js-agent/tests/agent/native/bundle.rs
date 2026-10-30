//! A native run that pins a bundle: the entrypoint it runs is a file of the bundle's trees,
//! its environment and arguments are the bundle's, the trees the bundle writes to are the
//! run's own copies, and the pins end with the run, however it ends. The runtime is the real
//! one, over the fake helper, with a store that holds a real bundle. Each test says the
//! defect it catches.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::PathBuf;
use std::sync::Arc;

use js_agent::bundle::{Admission, Needs, Placer};
use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime};
use js_agent_wire::{NetworkLevel, Outcome, refusal};
use js_bundle::DottedVersion;

use super::lifecycle::{Native, job};
use super::{Rig, native, parse};
use crate::bundle::fake::FakeHost;
use crate::bundle::fixture::{Bundle, Spec, no_registry, placer, settings};
use crate::lifecycle::{cleanup, names};
use crate::support::Scratch;

/// A rig whose runtime holds a store with one bundle in it.
struct Stored {
    rig: Rig,
    store: Arc<Placer>,
    bundle: Bundle,
    _dir: Scratch,
}

impl Stored {
    fn new(name: &str) -> Self {
        Self::of(name, Spec::standard())
    }

    fn of(name: &str, spec: Spec) -> Self {
        let dir = Scratch::new(&format!("native-bundle-{name}"));
        let src = dir.path("src");
        fs::create_dir(&src).unwrap();
        let bundle = spec.build(&src);
        let store = placer(&dir, no_registry());
        let layout = bundle.layout(&dir, "layout");
        store.ensure_layout(&layout, &bundle.digest()).unwrap();
        let Rig {
            fake,
            rt,
            dir: work,
        } = Rig::new(name);
        let rt = rt.with_bundles(Arc::clone(&store));
        Self {
            rig: Rig {
                fake,
                rt,
                dir: work,
            },
            store,
            bundle,
            _dir: dir,
        }
    }

    /// The directory of the store that holds the tree `tree`.
    fn tree(&self, tree: &str) -> PathBuf {
        let layer = self
            .bundle
            .config
            .layers
            .iter()
            .find(|layer| layer.tree.as_str() == tree)
            .unwrap();
        let trees = self.store.root().join("v1/objects/trees");
        trees.join(layer.diff_id.hex())
    }

    /// A start of `program args` that pins the bundle.
    fn spec(&self, run_id: &str, program: &str, args: &[&str]) -> RunSpec {
        let mut spec = self.rig.spec(run_id, native(program, args));
        spec.bundle = Some(self.bundle.pin());
        spec
    }

    /// Whether the run of lease `lease` holds no pin: the same run may pin the bundle again.
    fn unpinned(&self, lease: &str) -> bool {
        self.store.pin(&self.bundle.pin(), lease).is_ok()
    }
}

fn text(path: &std::path::Path) -> String {
    path.display().to_string()
}

/// Asserts what a start that runs `@bundle/tool` gives the helper: the file of the tool
/// tree as the program, the arguments with the bundle's placeholders resolved, the
/// environment of the bundle's rules (a tree that is only read is the store's own, one that
/// is written is the run's copy) with the job's own and `TMPDIR`, and a `trees` directory in
/// the home that is private to the user and holds the copy of the writable tree alone. It
/// catches a program resolved to the wrong file, a placeholder that reaches the program
/// unresolved, a tool that could write into the store, and a copy anyone else could read.
#[test]
fn a_start_runs_the_entrypoint_with_the_bundles_arguments_and_environment() {
    let mut s = Stored::new("start");
    let mut spec = s.spec(
        "r1",
        "@bundle/tool",
        &["--flag", "${tree:tool}/lib/data.txt"],
    );
    let Body::Native(body) = &mut spec.body else {
        panic!("a native body");
    };
    body.env.insert("X".to_owned(), "1".to_owned());
    let id = s.rig.rt.create_and_start(&spec).unwrap();
    assert_eq!(id, RunHandle("7.0".to_owned()));
    assert_eq!(
        s.rig.fake.verbs(),
        ["boot-reset", "user-create 7.0", "run 7.0"]
    );
    let home = s.rig.fake.home_of("7.0");
    let run = s.rig.fake.run_of("7.0");
    let tool = s.tree("tool");
    let tail = parse(&run.argv).tail;
    assert_eq!(
        tail,
        [
            text(&tool.join("bin/tool")),
            "--flag".to_owned(),
            text(&tool.join("lib/data.txt"))
        ]
    );
    for want in [
        ("TOOLROOT", text(&tool)),
        ("WORKROOT", text(&home.join("trees/work"))),
        ("TMPDIR", text(&home.join("tmp"))),
        ("X", "1".to_owned()),
    ] {
        let pair = (want.0.to_owned(), want.1.clone());
        assert!(run.env.contains(&pair), "{want:?} not in {:?}", run.env);
    }
    let mode = fs::metadata(home.join("trees"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o700);
    let copied = fs::read_to_string(home.join("trees/work/run.sh")).unwrap();
    assert_eq!(copied, "#!/bin/sh\necho work\n");
    assert!(!home.join("trees/tool").exists());
}

/// Asserts that an entrypoint in the tree the bundle writes to runs from the run's copy, not
/// from the store. It catches a program that runs in the store while its environment says the
/// copy.
#[test]
fn an_entrypoint_of_a_writable_tree_runs_from_the_runs_copy() {
    let mut s = Stored::new("writable");
    let spec = s.spec("r1", "@bundle/work", &[]);
    s.rig.rt.create_and_start(&spec).unwrap();
    let home = fs::canonicalize(s.rig.fake.home_of("7.0")).unwrap();
    let tail = parse(&s.rig.fake.run_of("7.0").argv).tail;
    assert_eq!(tail, [text(&home.join("trees/work/run.sh"))]);
}

/// Asserts that a program that is not the bundle's runs as it would without the bundle,
/// with the bundle's environment: the bundle's rules apply to every program of a run that
/// pins one. It catches a bundle that is pinned and then ignored.
#[test]
fn a_program_outside_the_bundle_still_gets_the_bundles_environment() {
    let mut s = Stored::new("outside");
    let spec = s.spec("r1", "/usr/bin/true", &[]);
    s.rig.rt.create_and_start(&spec).unwrap();
    let run = s.rig.fake.run_of("7.0");
    assert_eq!(parse(&run.argv).tail, ["/usr/bin/true"]);
    let toolroot = ("TOOLROOT".to_owned(), text(&s.tree("tool")));
    assert!(run.env.contains(&toolroot), "{:?}", run.env);
}

/// Asserts that the network a run gets is the lowest of what the start asks, what the
/// bundle's tools need and what the agent allows. It catches a start that asks for more than
/// the bundle's config says its tools need, and a bundle that raises the agent's cap.
#[test]
fn the_network_is_the_lowest_of_the_request_the_bundle_and_the_agent() {
    let loopback = "(allow network* (remote ip \"localhost:*\"))";
    let cases = [
        ("loopback", NetworkLevel::On, true),
        ("off", NetworkLevel::On, false),
        ("on", NetworkLevel::Loopback, true),
        ("loopback", NetworkLevel::Off, false),
    ];
    for (n, (needed, asked, has_loopback)) in cases.into_iter().enumerate() {
        let mut spec = Spec::standard();
        spec.network_max = needed;
        let mut s = Stored::of(&format!("net{n}"), spec);
        let mut run = s.spec("r1", "@bundle/tool", &[]);
        let Body::Native(body) = &mut run.body else {
            panic!("a native body");
        };
        body.network = asked;
        s.rig.rt.create_and_start(&run).unwrap();
        let profile = parse(&s.rig.fake.run_of("7.0").argv).profile;
        let said = format!("bundle {needed}, asked {asked:?}");
        assert_eq!(profile.contains(loopback), has_loopback, "{said}");
    }
}

/// Asserts that the trees stay pinned while the run exists and are released when it is
/// removed: another pin of the same lease is refused before and accepted after. It catches a
/// pin that is never released (the store could never evict the bundle) and one released while
/// the run still reads it.
#[test]
fn the_pins_live_as_long_as_the_run() {
    let mut s = Stored::new("pins");
    let spec = s.spec("r1", "@bundle/tool", &[]);
    let id = s.rig.rt.create_and_start(&spec).unwrap();
    assert!(!s.unpinned("7.0"), "the run holds its pins");
    s.rig.fake.exit("7.0", 0);
    s.rig.settle(&id);
    assert!(!s.unpinned("7.0"), "an ended run still holds its pins");
    s.rig.rt.remove(&id).unwrap();
    assert!(s.unpinned("7.0"), "the removed run released them");
}

/// Asserts that a start that fails before the user exists makes nothing (the helper is asked
/// for no user and the store has no pin of the run), each for what it is: a bundle the store
/// does not hold, a program the bundle lacks, a program that names a bundle when the start
/// pins none, and a start that pins a bundle on a runtime that has no store. It catches a
/// refusal that costs a lease user, and a pin left by a start that failed.
#[test]
fn a_start_that_fails_before_the_user_exists_makes_nothing() {
    let mut s = Stored::new("early");
    let mut unheld = s.spec("r1", "@bundle/tool", &[]);
    unheld.bundle = Some(format!("toolchain@sha256:{}", "e".repeat(64)));
    let cases = [
        (unheld, "No such file"),
        (s.spec("r2", "@bundle/gcc", &[]), "no entrypoint gcc"),
    ];
    for (spec, said) in cases {
        let why = s.rig.rt.create_and_start(&spec).unwrap_err();
        assert!(why.0.contains(said), "{why}");
        assert_eq!(s.rig.fake.verbs(), ["boot-reset"]);
        assert!(s.unpinned("7.0"));
    }
    let mut unpinned = s.rig.spec("r3", native("@bundle/tool", &[]));
    unpinned.bundle = None;
    let why = s.rig.rt.create_and_start(&unpinned).unwrap_err();
    assert!(why.0.contains("pins none"), "{why}");
    let mut bare = Rig::new("early-bare");
    let mut pinned = bare.spec("r4", native("/usr/bin/true", &[]));
    pinned.bundle = Some(s.bundle.pin());
    let why = bare.rt.create_and_start(&pinned).unwrap_err();
    assert!(why.0.contains("no bundle store"), "{why}");
    assert_eq!(bare.fake.verbs(), ["boot-reset"]);
}

/// Asserts every way a start fails once the user exists: an entrypoint that is a file that
/// cannot run, a directory, a file the tree lacks, a program that a store changed under the
/// agent has turned into a link out of its tree, a locked variable the job sets, and a helper
/// that refuses `run`. Each ends with the user killed and deleted and the pins released, and
/// the error says which. It catches a program run from outside the bundle, and a failed
/// start that leaves a user or a pin.
#[test]
fn a_start_that_fails_after_the_user_exists_removes_the_user_and_the_pins() {
    let cases = [
        ("@bundle/data", "not an executable file"),
        ("@bundle/dir", "not an executable file"),
        ("@bundle/absent", "No such file"),
        ("@bundle/tool", "leads out of its tree"),
        ("@bundle/work", "WORKROOT"),
        ("@bundle/work", "run: "),
    ];
    for (n, (program, said)) in cases.into_iter().enumerate() {
        let mut s = Stored::new(&format!("late{n}"));
        let mut spec = s.spec("r1", program, &[]);
        match n {
            3 => {
                let file = s.tree("tool").join("bin/tool");
                fs::remove_file(&file).unwrap();
                symlink("/bin/sh", &file).unwrap();
            }
            4 => {
                let Body::Native(body) = &mut spec.body else {
                    panic!("a native body");
                };
                body.env.insert("WORKROOT".to_owned(), "x".to_owned());
            }
            5 => s.rig.fake.refuse("run", "no"),
            _ => {}
        }
        let why = s.rig.rt.create_and_start(&spec).unwrap_err();
        assert!(why.0.contains(said), "{program}: {why}");
        assert_eq!(
            s.rig.fake.verbs()[..2],
            ["boot-reset", "user-create 7.0"],
            "{program}"
        );
        let verbs = s.rig.fake.verbs();
        assert_eq!(
            verbs[verbs.len() - 2..],
            ["kill-uid 7.0", "user-delete 7.0"],
            "{program}"
        );
        assert!(s.unpinned("7.0"), "{program}");
    }
}

/// Asserts what the runtime accepts of a native body before it makes anything: a program of
/// a bundle when it holds a store, `bundle-unsupported` when it holds none, and the staged
/// inputs that are refused either way. It catches a runtime that takes `@bundle/...` for a
/// path under a tool root, and one that says it can without a store.
#[test]
fn the_runtime_checks_a_bundle_program_against_its_store() {
    let s = Stored::new("check");
    assert!(s.rig.rt.check_native(&native("@bundle/tool", &[])).is_ok());
    let mut staged = native("@bundle/tool", &[]);
    staged.cwd = "sub".to_owned();
    let refused = s.rig.rt.check_native(&staged).unwrap_err();
    assert_eq!(refused.code, refusal::NATIVE_UNSUPPORTED);
    let bare = Rig::new("check-bare");
    let refused = bare
        .rt
        .check_native(&native("@bundle/tool", &[]))
        .unwrap_err();
    assert_eq!(refused.code, refusal::BUNDLE_UNSUPPORTED);
    assert!(s.rig.rt.bundles().is_some());
    assert!(bare.rt.bundles().is_none());
}

/// A lifecycle over a runtime with a store that holds the standard bundle, which judges
/// starts by an admission over a host that holds it too.
fn lifecycle(name: &str) -> (Native, Bundle, Arc<Placer>, Scratch) {
    let dir = Scratch::new(&format!("native-bundle-life-{name}"));
    let src = dir.path("src");
    fs::create_dir(&src).unwrap();
    let bundle = Spec::standard().build(&src);
    let store = placer(&dir, no_registry());
    let layout = bundle.layout(&dir, "layout");
    store.ensure_layout(&layout, &bundle.digest()).unwrap();
    let host = FakeHost::new();
    host.set(|state| {
        state.held = Ok(vec![bundle.digest()]);
        state.needs.insert(
            bundle.digest(),
            Needs {
                os_version: DottedVersion::parse("14.0").unwrap(),
                xcode: None,
                entrypoints: bundle.config.entrypoints.keys().cloned().collect(),
            },
        );
    });
    let admission = Admission::new(settings(&dir), host);
    let life = Native::with(name, Some((Arc::clone(&store), admission)));
    (life, bundle, store, dir)
}

/// A bundle job through the lifecycle: it starts, runs the entrypoint, ends with a result,
/// and the cleanup runs in order; afterwards the store holds no pin of the run and the helper
/// has no user of it. It catches a job whose bundle is pinned outside the lifecycle's
/// accounting, and a pin that outlives the result.
#[test]
fn a_bundle_job_runs_to_its_result_and_its_pins_are_released() {
    let (mut h, bundle, store, _dir) = lifecycle("run");
    let mut spec = job("r1", "@bundle/tool");
    spec.bundle = Some(bundle.pin());
    assert_eq!(h.start(spec), ["started r1"]);
    assert!(
        store.pin(&bundle.pin(), "7.0").is_err(),
        "pinned while it runs"
    );
    h.fake.exit("7.0", 0);
    let events = h.until(|life| life.busy_slots().is_empty());
    let mut want = vec!["result r1".to_owned()];
    want.extend(cleanup("r1"));
    assert_eq!(names(&events), want);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert!(
        store.pin(&bundle.pin(), "7.0").is_ok(),
        "released with the run"
    );
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
}

/// A bundle job that names a program the bundle lacks is refused `invalid-program` by the
/// lifecycle's admission, before the runtime is asked for anything. It catches an
/// entrypoint checked only after a user was made for it.
#[test]
fn a_bundle_job_for_a_program_the_bundle_lacks_is_refused_by_the_admission() {
    let (mut h, bundle, _store, _dir) = lifecycle("lacks");
    let mut spec = job("r1", "@bundle/gcc");
    spec.bundle = Some(bundle.pin());
    assert_eq!(h.start(spec), ["refused r1 invalid-program"]);
    assert_eq!(h.fake.verbs(), ["boot-reset"]);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::INVALID_PROGRAM));
}

/// Asserts that `TMPDIR` of a run that pins a bundle is the agent's, once, as it is for a run
/// that pins none, whatever the job's environment says. It catches a bundle run whose job
/// points the temporary files outside the home.
#[test]
fn the_agents_tmpdir_is_the_only_one_a_bundle_run_gets() {
    let mut s = Stored::new("tmpdir");
    let mut spec = s.spec("r1", "@bundle/tool", &[]);
    let Body::Native(body) = &mut spec.body else {
        panic!("a native body");
    };
    body.env
        .insert("TMPDIR".to_owned(), "/private/tmp".to_owned());
    s.rig.rt.create_and_start(&spec).unwrap();
    let tmp = text(&s.rig.fake.home_of("7.0").join("tmp"));
    let env = s.rig.fake.run_of("7.0").env;
    let given: Vec<_> = env.iter().filter(|(name, _)| name == "TMPDIR").collect();
    assert_eq!(given, [&("TMPDIR".to_owned(), tmp)]);
}
