//! The inputs and the working directory of a native run: mount inputs copied into `exec/`
//! and given to the run's account before its program starts, write-denied by the profile; a
//! `cwd` resolved below `exec/` (the working directory the supervisor is asked to start the
//! program in), and one that leaves it refused before anything starts.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use js_agent::lifecycle::{ResolvedMount, Runtime};
use js_agent::native::Journal;
use js_agent_wire::{InputRef, Placement, TreeUnavailable, refusal};

use super::lifecycle::Native;
use super::{Rig, native};

/// An input copied from `host` to `dest` below `exec/`, as the lifecycle resolves a mount
/// input.
fn input(host: &Path, dest: &str) -> ResolvedMount {
    ResolvedMount {
        host: host.to_path_buf(),
        target: dest.to_owned(),
        ro: true,
    }
}

/// A source tree below the rig's directory: `name/` holding `file` with `text`.
fn source(rig: &Rig, name: &str, file: &str, text: &str) -> PathBuf {
    let dir = rig.dir.path(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(file), text).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Makes every directory below `dir` writable again, so the test's directory can be removed:
/// a staged input directory is `0500`, which only root (the agent) can empty.
fn unlock(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            unlock(&path);
        }
    }
}

/// The run directory `exec` of the account the `n`th launch (from 0) of `rig` ran as.
fn exec_of(rig: &Rig, n: usize) -> PathBuf {
    let launched = &rig.launcher.launched()[n];
    rig.home_of(launched.var("USER").expect("the account"))
        .join("exec")
}

/// The working directory the `n`th launch of `rig` was asked to start its program in.
fn cwd_of(rig: &Rig, n: usize) -> PathBuf {
    rig.launcher.launched()[n].cwd.clone()
}

/// The lines of the lease journal of `rig` (a lease is added `+` and done `-`), empty when it
/// was never written.
fn leases(rig: &Rig) -> Vec<String> {
    let text = fs::read_to_string(rig.dir.path("native-leases")).unwrap_or_default();
    text.lines()
        .filter(|line| !line.starts_with("= "))
        .map(str::to_owned)
        .collect()
}

/// Asserts that no supervisor was asked for and the pool lends nothing: what a start that
/// fails after its account is lent leaves, the account being retired.
fn nothing_started(rig: &Rig) {
    assert!(rig.launcher.launched().is_empty(), "no supervisor");
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
}

/// Asserts that a start with two inputs (a directory and a single file, one below a `dest`
/// with a part that is not there) finds both below `exec/` with their contents once the
/// start is done, each directory `0500` and each file `0400` (`0500` for an executable one),
/// that the profile the supervisor is given denies writes below each `dest`, and that the
/// program is started in `exec/`. It catches inputs that are not staged, left writable to the
/// job, or a tool among them that can no longer be run.
#[test]
fn two_inputs_are_staged_into_exec_read_only() {
    let mut rig = Rig::new("inputs");
    let tree = source(&rig, "src-a", "x", "one");
    let single = source(&rig, "src-b", "data.txt", "two").join("data.txt");
    let tool = tree.join("tool");
    fs::write(&tool, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let mut spec = rig.spec("r1", native("/usr/bin/true", &[]));
    let third = source(&rig, "src-c", "y", "three");
    spec.mounts = vec![
        input(&tree, "in/a"),
        input(&single, "two.txt"),
        input(&third, "in/c"),
    ];
    rig.rt.create_and_start(&spec).unwrap();
    let exec = exec_of(&rig, 0);
    assert_eq!(fs::read_to_string(exec.join("in/a/x")).unwrap(), "one");
    assert_eq!(fs::read_to_string(exec.join("two.txt")).unwrap(), "two");
    assert_eq!(fs::read_to_string(exec.join("in/c/y")).unwrap(), "three");
    assert_eq!(mode(&exec.join("in")), 0o700);
    assert_eq!(mode(&exec.join("in/a")), 0o500);
    assert_eq!(mode(&exec.join("in/a/x")), 0o400);
    assert_eq!(
        mode(&exec.join("in/a/tool")),
        0o500,
        "an executable stays executable"
    );
    assert_eq!(mode(&exec.join("two.txt")), 0o400);
    let run = rig.launcher.last();
    for dest in ["in/a", "two.txt"] {
        let path = exec.join(dest).display().to_string();
        let param = run.params.iter().find(|(_, value)| *value == path);
        let (name, _) = param.unwrap_or_else(|| panic!("{path} in {:?}", run.params));
        let denied = format!("(deny file-write* (subpath (param \"{name}\")))");
        assert!(run.profile.contains(&denied), "{denied} in {}", run.profile);
    }
    assert_eq!(run.cwd, fs::canonicalize(&exec).unwrap());
    unlock(&rig.settings.homes);
}

/// Asserts that a link in an input is copied as the link it is (never followed), and that a
/// `cwd` through a link that stays below `exec/` is the directory it resolves to. It catches
/// a copy that reads through a link out of the mount root, and a working directory refused
/// for a link that does not leave the run directory.
#[test]
fn an_input_link_is_copied_as_a_link_and_a_cwd_may_follow_one_inside() {
    let mut rig = Rig::new("inputs-link");
    let tree = source(&rig, "src", "x", "one");
    fs::create_dir(tree.join("real")).unwrap();
    std::os::unix::fs::symlink("real", tree.join("here")).unwrap();
    let mut body = native("/usr/bin/true", &[]);
    body.cwd = "in/here".to_owned();
    let mut spec = rig.spec("r1", body);
    spec.mounts = vec![input(&tree, "in")];
    rig.rt.create_and_start(&spec).unwrap();
    let exec = exec_of(&rig, 0);
    assert_eq!(
        fs::read_link(exec.join("in/here")).unwrap(),
        Path::new("real")
    );
    let real = fs::canonicalize(exec.join("in/real")).unwrap();
    assert_eq!(cwd_of(&rig, 0), real);
    unlock(&rig.settings.homes);
}

/// Asserts that `cwd = "sub"` runs in `exec/sub`, made `0700` for the user because it was
/// not there, and that `./sub/deeper` names a directory below it the same way. It catches a
/// `cwd` ignored (the run starts in `exec/`) and one that must exist beforehand.
#[test]
fn a_cwd_runs_in_its_directory_below_exec() {
    let mut rig = Rig::in_dir("cwd", 2, |_, _| {});
    let mut body = native("/usr/bin/true", &[]);
    body.cwd = "sub".to_owned();
    rig.rt
        .create_and_start(&rig.spec("r1", body.clone()))
        .unwrap();
    let exec = exec_of(&rig, 0);
    assert_eq!(cwd_of(&rig, 0), fs::canonicalize(exec.join("sub")).unwrap());
    assert_eq!(mode(&exec.join("sub")), 0o700);
    body.cwd = "./sub/deeper".to_owned();
    rig.rt.create_and_start(&rig.spec("r2", body)).unwrap();
    let exec = exec_of(&rig, 1);
    let want = fs::canonicalize(exec.join("sub/deeper")).unwrap();
    assert_eq!(cwd_of(&rig, 1), want);
}

/// Asserts that a `cwd` that leaves `exec/` is refused with the escape error and nothing is
/// started: `../x` and an absolute path before any account is lent (no lease is written),
/// and a link (from an input) that resolves outside after the account is lent, which is then
/// retired with its lease done and the supervisor never asked. Admission answers the first
/// two `invalid-job`. It catches a job that starts in the account's home, in `/`, or
/// anywhere a link points.
#[test]
fn a_cwd_that_escapes_exec_is_refused_and_nothing_starts() {
    let mut rig = Rig::new("cwd-escape");
    for cwd in ["../x", "/etc"] {
        let mut body = native("/usr/bin/true", &[]);
        body.cwd = cwd.to_owned();
        let refused = rig.rt.check_native(&body).unwrap_err();
        assert_eq!(refused.code, refusal::INVALID_JOB);
        let error = rig.rt.create_and_start(&rig.spec("r1", body)).unwrap_err();
        let want = format!("native.cwd {cwd:?} escapes the run directory");
        assert_eq!((error.0, refused.why), (want.clone(), want));
    }
    nothing_started(&rig);
    assert_eq!(leases(&rig), Vec::<String>::new());
    let tree = rig.dir.path("src");
    fs::create_dir(&tree).unwrap();
    std::os::unix::fs::symlink("/", tree.join("out")).unwrap();
    let mut body = native("/usr/bin/true", &[]);
    body.cwd = "in/out/etc".to_owned();
    let mut spec = rig.spec("r1", body);
    spec.mounts = vec![input(&tree, "in")];
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(
        error.0,
        "native.cwd \"in/out/etc\" escapes the run directory"
    );
    nothing_started(&rig);
    assert_eq!(leases(&rig), ["+ 7.0", "- 7.0"]);
    unlock(&rig.settings.homes);
}

/// Asserts the working directories that cannot be used and fail the start before the
/// supervisor is asked: a part that is a file, and a link that leads nowhere. It catches a
/// start that runs in `exec/` when its `cwd` cannot be had.
#[test]
fn a_cwd_that_is_not_a_directory_fails_the_start() {
    let mut rig = Rig::new("cwd-file");
    let tree = source(&rig, "src", "file", "text");
    std::os::unix::fs::symlink("missing", tree.join("dangling")).unwrap();
    for (cwd, said) in [
        ("in/file", "native.cwd \"in/file\": not a directory"),
        ("in/dangling", "native.cwd \"in/dangling\": "),
    ] {
        let mut body = native("/usr/bin/true", &[]);
        body.cwd = cwd.to_owned();
        let mut spec = rig.spec("r1", body);
        spec.mounts = vec![input(&tree, "in")];
        let error = rig.rt.create_and_start(&spec).unwrap_err();
        assert!(error.0.starts_with(said), "{cwd}: {}", error.0);
    }
    nothing_started(&rig);
    unlock(&rig.settings.homes);
}

/// Asserts that an input that cannot be staged fails the start before the supervisor is
/// asked, naming the input, and the account is retired: a source that is not there, two inputs at one `dest`, and
/// a source that is neither a file, a directory nor a link (a device). It catches a run that
/// starts without an input it named.
#[test]
fn an_input_that_cannot_be_staged_fails_the_start() {
    let mut rig = Rig::new("inputs-bad");
    let tree = source(&rig, "src", "x", "one");
    // A character device: neither a file, a directory nor a link, on Linux and macOS.
    let device = std::path::Path::new("/dev/null");
    let cases = [
        (
            vec![input(&rig.dir.path("missing"), "in")],
            "an input: in: ",
        ),
        (
            vec![input(&tree, "twice"), input(&tree, "twice")],
            "an input: twice: ",
        ),
        (vec![input(device, "s")], "an input: s: "),
    ];
    for (n, (inputs, said)) in cases.into_iter().enumerate() {
        let mut spec = rig.spec(&format!("r{n}"), native("/usr/bin/true", &[]));
        spec.mounts = inputs;
        let error = rig.rt.create_and_start(&spec).unwrap_err();
        assert!(error.0.starts_with(said), "{n}: {}", error.0);
    }
    nothing_started(&rig);
    let done = ["+ 7.0", "- 7.0", "+ 7.1", "- 7.1", "+ 7.2", "- 7.2"];
    assert_eq!(leases(&rig), done);
    unlock(&rig.settings.homes);
}

/// Asserts what admission answers inputs: a mount input passes, a `stage` input is refused
/// `native-unsupported` (this agent keeps no stage trees). It catches a stage input dropped
/// without a word.
#[test]
fn a_stage_input_is_refused_and_a_mount_input_admitted() {
    let rig = Rig::new("inputs-check");
    let mut mounted = native("/bin/sh", &[]);
    mounted.inputs.push(InputRef::Mount {
        root_name: "data".to_owned(),
        rel: "x".to_owned(),
        dest: "in".to_owned(),
    });
    assert_eq!(rig.rt.check_native(&mounted), Ok(()));
    let mut staged = native("/bin/sh", &[]);
    staged.inputs.push(InputRef::Stage {
        stage_id: format!("sha256:{}", "a".repeat(64)),
        dest: "in".to_owned(),
        placement: Default::default(),
    });
    let refused = rig.rt.check_native(&staged).unwrap_err();
    assert_eq!(refused.code, refusal::NATIVE_UNSUPPORTED);
    let why = "this agent keeps no stage trees: only mount inputs are staged";
    assert_eq!(refused.why, why);
}

/// A retired tree input at `dest`.
fn tree(dest: &str, placement: Placement) -> InputRef {
    InputRef::Tree {
        digest: format!("sha256:{}", "b".repeat(64)),
        dest: dest.to_owned(),
        placement,
        unavailable: TreeUnavailable::Fail,
    }
}

/// Asserts that admission refuses a start holding a retired `tree` input
/// `native-unsupported`, saying it is retired: read-only at a `dest`, writable at the exec
/// root, alone and after a mount input that passes. It catches a tree input passed over
/// when the inputs are staged, so that the job runs without it.
#[test]
fn a_tree_input_is_refused_native_unsupported() {
    let rig = Rig::new("inputs-tree");
    let mounted = InputRef::Mount {
        root_name: "data".to_owned(),
        rel: "x".to_owned(),
        dest: "in".to_owned(),
    };
    let cases = [
        vec![tree("in", Placement::ReadOnly)],
        vec![tree("", Placement::Writable)],
        vec![mounted, tree("t", Placement::ReadOnly)],
    ];
    for inputs in cases {
        let mut body = native("/bin/sh", &[]);
        body.inputs = inputs;
        let refused = rig.rt.check_native(&body).unwrap_err();
        assert_eq!(refused.code, refusal::NATIVE_UNSUPPORTED, "{body:?}");
        let why = "a tree input is retired: only mount inputs are staged";
        assert_eq!(refused.why, why);
    }
}

/// Asserts the lifecycle's part: a native start's mount input is resolved below the agent's
/// mount root and staged at its `dest`, and one whose root the agent does not have is refused
/// `invalid-mount` before an account is lent. It catches a mount input handed to
/// the runtime unresolved, or resolved outside the agent's roots.
#[test]
fn the_lifecycle_resolves_a_mount_input_below_its_root() {
    let mut h = Native::with("inputs-life", None);
    fs::create_dir(h.dir.path("data").join("x")).unwrap();
    fs::write(h.dir.path("data").join("x/file"), "hi").unwrap();
    let mut spec = super::lifecycle::job("r1", "/usr/bin/true");
    let mount = |root: &str| InputRef::Mount {
        root_name: root.to_owned(),
        rel: "x".to_owned(),
        dest: "in".to_owned(),
    };
    spec.native.as_mut().unwrap().inputs.push(mount("data"));
    assert_eq!(h.start(spec.clone()), ["started r1"]);
    let staged = h.home_of(0).join("exec/in/file");
    assert_eq!(fs::read_to_string(staged).unwrap(), "hi");
    spec.run_id = "r2".to_owned();
    spec.native.as_mut().unwrap().inputs = vec![mount("other")];
    assert_eq!(
        h.start(spec),
        [format!("refused r2 {}", refusal::INVALID_MOUNT)]
    );
    assert_eq!(h.launcher.launched().len(), 1, "no second launch");
    let lent = Journal::new(&h.dir.path("native-leases")).live().unwrap();
    assert_eq!(lent, ["7.0"], "no second lease");
    unlock(&h.settings.homes);
}

/// Asserts that a `cwd` whose part cannot even be looked up (a name longer than the file
/// system allows) fails the start before the supervisor is asked, naming the `cwd`. It catches a lookup error
/// taken for a part that is not there, which would be made.
#[test]
fn a_cwd_that_cannot_be_looked_up_fails_the_start() {
    let mut rig = Rig::new("cwd-long");
    let mut body = native("/usr/bin/true", &[]);
    body.cwd = "a".repeat(300);
    let error = rig.rt.create_and_start(&rig.spec("r1", body)).unwrap_err();
    let said = format!("native.cwd {:?}: ", "a".repeat(300));
    assert!(error.0.starts_with(&said), "{}", error.0);
    nothing_started(&rig);
}
