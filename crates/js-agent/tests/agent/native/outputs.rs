//! The declared outputs of a native run, copied out of its working directory when the run
//! is removed: after the kill and before the user (and its home) is deleted, without
//! following a link, within the run's caps, and without stopping the teardown when one
//! cannot be copied.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use js_agent::lifecycle::{Event, Runtime as _};
use js_agent_wire::{Outcome, OutputKind, OutputSpec};

use super::lifecycle::{Native, job};
use super::{Rig, native};
use crate::lifecycle::{cleanup, names};

fn output(path: &str, kind: OutputKind) -> OutputSpec {
    OutputSpec {
        path: path.to_owned(),
        kind,
    }
}

/// A rig whose run `r1` (lease `7.0`) declares `outputs` and has started; returns the rig
/// and the run's working directory, where the test writes as the job.
fn started(rig_name: &str, max_file_bytes: u64, outputs: Vec<OutputSpec>) -> (Rig, PathBuf) {
    let mut rig = Rig::with(rig_name, |config| config.max_file_bytes = max_file_bytes);
    let mut body = native("/bin/sh", &[]);
    body.outputs = outputs;
    let spec = rig.spec("r1", body);
    rig.rt.create_and_start(&spec).unwrap();
    let exec = rig.fake.home_of("7.0").join("exec");
    (rig, exec)
}

/// Where the outputs of the run `r1` of `rig` are copied to.
fn collected(rig: &Rig) -> PathBuf {
    rig.dir.path("scratch").join("r1").join("outputs")
}

fn handle() -> js_agent::lifecycle::RunHandle {
    js_agent::lifecycle::RunHandle("7.0".to_owned())
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The lines of the lease journal of `rig`.
fn journal(rig: &Rig) -> Vec<String> {
    let text = fs::read_to_string(rig.dir.path("native-leases")).unwrap_or_default();
    text.lines().map(str::to_owned).collect()
}

/// Asserts that a declared output is copied after the kill and before the user is deleted:
/// when the helper's `user-delete` arrives, the copy is already in the run's scratch
/// directory with the bytes the job wrote, the helper saw `kill-uid` and then
/// `user-delete`, the copy stays (`0600`, the agent's) after the home is gone, and the
/// journal says the lease is done. A file output in a directory of the working directory
/// is copied to the same place below `outputs`. It catches a collection after the deletion
/// (the home and the file would be gone) and a collection that never runs.
#[test]
fn outputs_are_collected_before_the_user_is_deleted() {
    let declared = vec![
        output("out.txt", OutputKind::File),
        output("logs/run.log", OutputKind::File),
    ];
    let (mut rig, exec) = started("outputs-order", 1 << 20, declared);
    let into = collected(&rig);
    rig.fake.read_at_delete(&into.join("out.txt"));
    fs::write(exec.join("out.txt"), b"the job's result\n").unwrap();
    fs::create_dir(exec.join("logs")).unwrap();
    fs::write(exec.join("logs/run.log"), b"log").unwrap();
    rig.fake.exit("7.0", 0);
    rig.settle(&handle());
    rig.rt.remove(&handle()).unwrap();
    let read = rig.fake.read_when_deleted();
    assert_eq!(read, Some(Some(b"the job's result\n".to_vec())));
    let verbs = rig.fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 2..],
        ["kill-uid 7.0", "user-delete 7.0"]
    );
    assert!(!rig.fake.home_of("7.0").exists());
    assert_eq!(
        fs::read(into.join("out.txt")).unwrap(),
        b"the job's result\n"
    );
    assert_eq!(mode(&into.join("out.txt")), 0o600);
    assert_eq!(fs::read(into.join("logs/run.log")).unwrap(), b"log");
    assert_eq!(mode(&into.join("logs")), 0o700);
    assert_eq!(journal(&rig), ["+ 7.0", "- 7.0"]);
}

/// Asserts that a directory output is copied as its regular files, a nested directory's
/// included, and that no link is followed: a link in the tree to a file outside the home
/// is not copied and is a failure; a link declared as the output itself is a failure; a
/// link on the way to an output is a failure; and a socket in the tree is not copied and
/// is a failure. The removal still deletes the user and fails with every failure by path,
/// and none of what the links point at is copied. It catches a root agent steered by a
/// job's link to read outside the job's home.
#[test]
fn a_directory_output_copies_regular_files_and_skips_links() {
    let declared = vec![
        output("tree", OutputKind::Dir),
        output("single", OutputKind::File),
        output("via/x.txt", OutputKind::File),
    ];
    let (mut rig, exec) = started("outputs-links", 1 << 20, declared);
    let outside = rig.dir.path("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"not the job's").unwrap();
    fs::write(outside.join("x.txt"), b"not the job's").unwrap();
    let tree = exec.join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("a.txt"), b"a").unwrap();
    fs::write(tree.join("sub/b.txt"), b"bb").unwrap();
    symlink(outside.join("secret"), tree.join("link")).unwrap();
    symlink(outside.join("secret"), exec.join("single")).unwrap();
    symlink(&outside, exec.join("via")).unwrap();
    // A socket made where its path is short, then moved into the tree.
    let short = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("o{}", std::process::id()));
    drop(UnixListener::bind(&short).unwrap());
    fs::rename(&short, tree.join("sock")).unwrap();
    rig.fake.exit("7.0", 0);
    rig.settle(&handle());
    let why = rig.rt.remove(&handle()).unwrap_err();
    assert_eq!(
        why.0,
        "outputs not copied: tree/link: a symbolic link: not copied; \
         tree/sock: a special file: not copied; single: a symbolic link: not followed; \
         via/x.txt: a symbolic link on the way to it"
    );
    let into = collected(&rig);
    assert_eq!(fs::read(into.join("tree/a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(into.join("tree/sub/b.txt")).unwrap(), b"bb");
    for absent in ["tree/link", "tree/sock", "single", "via"] {
        assert!(fs::symlink_metadata(into.join(absent)).is_err(), "{absent}");
    }
    assert_eq!(rig.fake.verbs().last().unwrap(), "user-delete 7.0");
    assert_eq!(journal(&rig), ["+ 7.0", "- 7.0"]);
}

/// Asserts that an output over the run's largest file is not copied past the cap (no part
/// of it is kept), that the removal still kills and deletes the user and marks the journal
/// done, and that it then fails naming the path and the cap, not the bytes. A removal the
/// helper refused first (`user-delete`) and that is tried again copies nothing twice and
/// reports the failure once, and a removal after that is fine. It catches a failure that
/// returns early and leaves the user, and a copy that is cut short and kept.
#[test]
fn an_output_over_the_cap_is_a_collection_failure_and_teardown_still_happens() {
    let declared = vec![output("big", OutputKind::File)];
    let (mut rig, exec) = started("outputs-cap", 10, declared);
    fs::write(exec.join("big"), b"0123456789A").unwrap();
    rig.fake.exit("7.0", 0);
    rig.settle(&handle());
    rig.fake.refuse("user-delete", "busy");
    let first = rig.rt.remove(&handle()).unwrap_err();
    assert!(first.0.starts_with("user-delete: "), "{}", first.0);
    assert!(first.0.contains("busy"), "{}", first.0);
    assert!(!first.0.contains("outputs"), "{}", first.0);
    rig.fake.allow("user-delete");
    let why = rig.rt.remove(&handle()).unwrap_err();
    assert_eq!(
        why.0,
        "outputs not copied: big: over the 10 bytes it may hold: not copied"
    );
    assert!(!collected(&rig).join("big").exists());
    let verbs = rig.fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 4..],
        [
            "kill-uid 7.0",
            "user-delete 7.0",
            "kill-uid 7.0",
            "user-delete 7.0"
        ]
    );
    assert_eq!(journal(&rig), ["+ 7.0", "- 7.0"]);
    rig.rt.remove(&handle()).unwrap();
}

/// Asserts that outputs the job did not make are not an error: a missing file, a missing
/// directory and a file below a missing directory are each skipped, the removal succeeds
/// and nothing is made below the scratch directory; and a run whose home is already gone
/// has nothing to copy either. It catches a job failed for an output it chose not to write.
#[test]
fn a_missing_declared_output_is_not_an_error() {
    let declared = vec![
        output("never", OutputKind::File),
        output("nothing", OutputKind::Dir),
        output("deep/er/file", OutputKind::File),
    ];
    let (mut rig, _) = started("outputs-missing", 1 << 20, declared.clone());
    rig.fake.exit("7.0", 0);
    rig.settle(&handle());
    rig.rt.remove(&handle()).unwrap();
    assert!(!collected(&rig).exists());

    let (mut rig, _) = started("outputs-homeless", 1 << 20, declared);
    fs::remove_dir_all(rig.fake.home_of("7.0")).unwrap();
    rig.rt.remove(&handle()).unwrap();
    assert!(!collected(&rig).exists());
}

/// Asserts the failures of an output that is not what it was declared as, or cannot be
/// copied where it goes: a file output that is a directory, a directory output that is a
/// file, a file on the way to an output, a path that is not relative and plain (`..`,
/// `/`, an empty name), a second output to a place already copied to, and a tree deeper
/// than 64 directories (what is above the limit is copied). Each is named in the removal's
/// error, in order. It catches an output copied as what it is not, a path that climbs out
/// of the working directory, and a tree walked without bound.
#[test]
fn an_output_that_is_not_what_was_declared_is_a_failure() {
    let declared = vec![
        output("adir", OutputKind::File),
        output("afile", OutputKind::Dir),
        output("afile/x", OutputKind::File),
        output("../escape", OutputKind::File),
        output("/abs", OutputKind::File),
        output("a//b", OutputKind::File),
        output("afile", OutputKind::File),
        output("afile", OutputKind::File),
        output("deep", OutputKind::Dir),
    ];
    let (mut rig, exec) = started("outputs-kinds", 1 << 20, declared);
    fs::create_dir(exec.join("adir")).unwrap();
    fs::write(exec.join("afile"), b"f").unwrap();
    let mut deep = exec.join("deep");
    for _ in 0..64 {
        deep = deep.join("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.parent().unwrap().join("kept"), b"k").unwrap();
    rig.fake.exit("7.0", 0);
    rig.settle(&handle());
    let why = rig.rt.remove(&handle()).unwrap_err();
    let deepest = format!("deep{}", "/d".repeat(64));
    let failures = [
        "adir: not a regular file".to_owned(),
        "afile: not a directory".to_owned(),
        "afile/x: a regular file on the way to it".to_owned(),
        "../escape: not a relative path of plain names".to_owned(),
        "/abs: not a relative path of plain names".to_owned(),
        "a//b: not a relative path of plain names".to_owned(),
        "afile: File exists (os error 17)".to_owned(),
        format!("{deepest}: deeper than 64 directories"),
    ];
    assert_eq!(
        why.0,
        format!("outputs not copied: {}", failures.join("; "))
    );
    let into = collected(&rig);
    assert_eq!(fs::read(into.join("afile")).unwrap(), b"f");
    let kept = into.join(format!("deep{}/kept", "/d".repeat(63)));
    assert_eq!(fs::read(kept).unwrap(), b"k");
}

/// Asserts that a native run whose output cannot be copied is cleaned up like any other
/// through the lifecycle: the result is the job's exit, the failure is the finding
/// `outputs-failed` naming the path, the cleanup runs in its order, the host is not tainted
/// (nothing of the run is left: its user is deleted), and the next start runs. It catches a
/// job that taints the host for good by declaring an output and writing a link there.
#[test]
fn a_collection_failure_is_a_finding_and_leaves_the_host_untainted() {
    let mut h = Native::with_files("outputs-life", |_| {}, None);
    let mut spec = job("r1", "/bin/sh");
    spec.native.as_mut().unwrap().outputs = vec![output("out", OutputKind::File)];
    assert_eq!(h.start(spec), ["started r1"]);
    let exec = h.fake.home_of("7.0").join("exec");
    symlink("elsewhere", exec.join("out")).unwrap();
    h.fake.exit("7.0", 0);
    let events = h.until(|life| life.busy_slots().is_empty());
    let mut want = vec![
        "result r1".to_owned(),
        "finding r1 outputs-failed".to_owned(),
    ];
    want.extend(cleanup("r1"));
    assert_eq!(names(&events), want);
    let found = events.iter().find_map(|event| match event {
        Event::Finding(finding) => Some(finding.message.clone()),
        _ => None,
    });
    let said = "outputs not copied: out: a symbolic link: not followed";
    assert_eq!(found.as_deref(), Some(said));
    assert_eq!(h.life.results()[0].outcome, Outcome::Succeeded);
    assert!(!h.life.is_tainted());
    assert!(!h.fake.home_of("7.0").exists());
    assert_eq!(h.start(job("r2", "/bin/sh")), ["started r2"]);
}

/// Asserts that a file output with a second name (a hard link to a file outside the home,
/// which a job may make to a file it cannot read) is not copied and is a failure, while the
/// file it names is untouched. It catches the root agent copying a file the job could not
/// read itself.
#[test]
fn a_file_with_a_second_name_is_not_copied() {
    let declared = vec![output("hard", OutputKind::File)];
    let (mut rig, exec) = started("outputs-hard", 1 << 20, declared);
    let outside = rig.dir.path("outside");
    fs::write(&outside, b"not the job's").unwrap();
    fs::hard_link(&outside, exec.join("hard")).unwrap();
    rig.fake.exit("7.0", 0);
    rig.settle(&handle());
    let why = rig.rt.remove(&handle()).unwrap_err();
    assert_eq!(
        why.0,
        "outputs not copied: hard: a file with 2 names: not copied"
    );
    assert!(!collected(&rig).join("hard").exists());
    assert_eq!(fs::read(&outside).unwrap(), b"not the job's");
}
