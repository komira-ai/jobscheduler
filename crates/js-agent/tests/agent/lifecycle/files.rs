//! Run files on the agent's side: written before the container exists, with the modes the
//! container's user needs and nobody else can change, handed to the runtime as a
//! read-only directory, refused for host jobs, removed with the run, and written once per
//! run however often the start is sent.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent_wire::{Isolation, JobSpec, RunFile};

use super::{Harness, Options, job, names};

fn file(name: &str, contents: &str) -> RunFile {
    RunFile {
        name: name.into(),
        contents: contents.into(),
    }
}

fn with_files(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.files = vec![
        file("put-url", "https://store.example/put?sig=abc"),
        file("grant", "token-123\n"),
    ];
    spec
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn files_root(h: &Harness) -> &js_agent::lifecycle::FilesRoot {
    h.config.files.as_ref().unwrap()
}

fn files_dir(h: &Harness, run_id: &str) -> std::path::PathBuf {
    files_root(h).of(run_id)
}

/// The runtime is handed the files directory (not the scratch directory, which the job
/// writes) with the contents as sent, each file `0444` and the directory `0555`; a run
/// without files is handed none and no directory is made. Catches contents or names
/// altered, modes a container user cannot read or that let the job's own user (the agent
/// is the owner) rewrite them, files placed inside the job-writable scratch directory, and
/// a directory made for every run.
#[test]
fn files_are_written_before_the_container_with_closed_modes() {
    let mut h = Harness::new("files-write");
    h.start(with_files("r1"));
    h.start(job("r2"));
    let created = h.fake.created();
    let dir = files_dir(&h, "r1");
    assert_eq!(created[0].files.as_deref(), Some(dir.as_path()));
    assert_eq!(created[1].files, None);
    assert!(!files_dir(&h, "r2").exists());
    assert!(!dir.starts_with(h.scratch_of("r1")));
    assert_eq!(
        fs::read_to_string(dir.join("put-url")).unwrap(),
        "https://store.example/put?sig=abc"
    );
    assert_eq!(
        fs::read_to_string(dir.join("grant")).unwrap(),
        "token-123\n"
    );
    assert_eq!(mode(&dir), 0o555);
    assert_eq!(mode(&dir.join("put-url")), 0o444);
    assert_eq!(mode(&dir.join("grant")), 0o444);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    assert!(h.scratch_of("r1").read_dir().unwrap().next().is_none());
}

/// A host job with files is refused with `files-need-container`, nothing is created and
/// no directory is written. Catches secrets handed to a job that runs on the host with no
/// mount to read them, or written for a job that then fails.
#[test]
fn a_host_job_with_files_is_refused() {
    let mut h = Harness::with(
        "files-host",
        Options {
            allow_host_jobs: true,
            ..Options::default()
        },
    );
    let mut spec = with_files("r1");
    spec.isolation = Isolation::Host;
    let events = names(&h.start(spec));
    assert_eq!(events, ["refused r1 files-need-container"]);
    assert!(h.fake.created().is_empty());
    assert!(!files_dir(&h, "r1").exists());
    assert!(!h.marker().exists(), "the refused job tainted nothing");
}

/// When the run is cleaned up the files directory goes with the scratch directory, though
/// it is `0555` with `0444` files. Catches a removal that cannot unlink in a closed
/// directory (leaving a presigned URL on disk) and a cleanup that fails and taints.
#[test]
fn cleanup_removes_the_closed_files_directory() {
    let mut h = Harness::new("files-cleanup");
    h.start(with_files("r1"));
    let dir = files_dir(&h, "r1");
    assert!(dir.is_dir());
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"scratch removed r1".to_owned()),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| e.starts_with("tainted")),
        "{events:?}"
    );
    assert!(!dir.exists());
}

/// A start that is sent again, while the run is held and after it has ended, creates one
/// container, leaves the files as they were (same inode, so not rewritten) and does not
/// fail. Catches a second start that rewrites or duplicates the files or errors on the
/// existing directory.
#[test]
fn a_resent_start_does_not_rewrite_the_files() {
    use std::os::unix::fs::MetadataExt;
    let mut h = Harness::new("files-resend");
    h.start(with_files("r1"));
    let dir = files_dir(&h, "r1");
    let inode = fs::metadata(dir.join("grant")).unwrap().ino();
    let again = names(&h.start(with_files("r1")));
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(fs::metadata(dir.join("grant")).unwrap().ino(), inode);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    // Once the run has ended and been acknowledged, a resend is skipped as retired and
    // writes nothing.
    h.fake.exit("r1", 0);
    h.life.step();
    h.ack("r1", "accepted");
    h.life.step();
    let late = names(&h.start(with_files("r1")));
    assert!(!late.iter().any(|e| e.starts_with("started")), "{late:?}");
    assert!(!dir.exists());
}

/// A restart sweeps the files directory of a run no container owns and keeps that of an
/// adopted run. Catches orphaned secrets surviving an agent restart, and a sweep that
/// deletes the files from under a running container.
#[test]
fn restart_sweeps_orphan_files_and_keeps_the_adopted_runs() {
    let mut h = Harness::new("files-sweep");
    h.start(with_files("r1"));
    let kept = files_dir(&h, "r1");
    let orphan = files_dir(&h, "gone");
    fs::create_dir(&orphan).unwrap();
    fs::write(orphan.join("x"), "x").unwrap();
    fs::set_permissions(orphan.join("x"), fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&orphan, fs::Permissions::from_mode(0o555)).unwrap();
    let events = h.restart().unwrap();
    assert_eq!(names(&events), ["adopted r1"]);
    assert!(!orphan.exists());
    assert_eq!(fs::read_dir(&kept).unwrap().count(), 2);
}

/// A files directory left by an earlier process for the same run id is replaced by the
/// run's new one. Catches `O_EXCL` tripping over a stale directory.
#[test]
fn a_stale_files_directory_is_replaced() {
    let mut h = Harness::new("files-stale");
    let stale = files_dir(&h, "r1");
    fs::create_dir(&stale).unwrap();
    fs::write(stale.join("put-url"), "old").unwrap();
    h.start(with_files("r1"));
    assert_eq!(
        fs::read_to_string(stale.join("put-url")).unwrap(),
        "https://store.example/put?sig=abc"
    );
}

/// The files root refuses to write a name that is not one path component, even though the
/// wire already refuses it, and leaves nothing behind. Catches an agent that trusts the
/// wire check alone with a path.
#[test]
fn write_refuses_a_name_with_a_slash_and_leaves_nothing() {
    let h = Harness::new("files-name");
    let root = files_root(&h);
    for bad in ["", ".", "..", "../x", "a/b", "a\0b"] {
        let files = [file("ok", "x"), file(bad, "x")];
        let why = root.write("r9", &files).unwrap_err();
        assert_eq!(why.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
        assert!(!root.of("r9").exists(), "the partial directory is removed");
    }
    assert!(!root.path().join("x").exists());
}

/// A write that fails half way (the second file of the same name cannot be created
/// `O_EXCL`) removes the directory it made, so the first file's secret is not left on the
/// file system, and the error holds neither a name nor any contents. Catches a failed
/// start that leaves a readable `0444` secret behind.
#[test]
fn a_failed_write_removes_its_partial_directory() {
    let h = Harness::new("files-partial");
    let root = files_root(&h);
    let twice = [file("tok", "SECRET-PLANTED-7f3a"), file("tok", "other")];
    let why = root.write("r9", &twice).unwrap_err();
    assert_eq!(why.kind(), std::io::ErrorKind::AlreadyExists);
    let shown = format!("{why} {why:?}");
    assert!(
        !shown.contains("SECRET") && !shown.contains("tok"),
        "{shown}"
    );
    assert!(!root.of("r9").exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

/// A start whose files cannot be written (the files root is replaced by a file after the
/// agent started, so the directory cannot be made whoever runs the test) is a
/// `failed_to_start` with the `scratch-failed` detail and a finding, creates no
/// container, reports and logs nothing of the file's name or contents, and is cleaned up
/// once the root is back. Catches a start that goes on without its files, a failure that
/// is not reported, and an error text that carries a secret.
#[test]
fn files_that_cannot_be_written_fail_the_start_without_leaking() {
    let mut h = Harness::new("files-fail");
    let root = files_root(&h).path().to_owned();
    fs::remove_dir(&root).unwrap();
    fs::write(&root, "in the way").unwrap();
    let mut spec = with_files("r1");
    spec.files = vec![file("secret-name", "SECRET-PLANTED-7f3a")];
    let events = h.start(spec);
    assert_eq!(names(&events), ["finding r1 scratch-failed", "result r1"]);
    assert_eq!(
        h.life.results()[0].detail.as_deref(),
        Some("scratch-failed")
    );
    assert!(h.fake.created().is_empty(), "no container was created");
    let shown = format!("{events:?} {:?} {:?}", h.life.results(), h.life.held());
    assert!(
        !shown.contains("SECRET") && !shown.contains("secret-name"),
        "{shown}"
    );
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(names(&h.life.step()).last().unwrap(), "freed r1");
}

/// A planted secret reaches nothing the agent can print: not the `ContainerSpec` the
/// runtime is handed (the debug form), not any event, result or held run (the forms the
/// worker logs), and not the debug form of the start. Catches a derived Debug or a log
/// line that would put a presigned URL in a journal.
#[test]
fn a_planted_secret_is_printed_nowhere() {
    let mut h = Harness::new("files-planted");
    let mut spec = job("r1");
    spec.files = vec![file("tok", "SECRET-PLANTED-7f3a")];
    let events = h.start(spec.clone());
    h.fake.exit("r1", 0);
    let more = h.life.step();
    let shown = [
        format!("{spec:?}"),
        format!("{:?}", h.fake.created()),
        format!("{events:?}"),
        format!("{more:?}"),
        format!("{:?}", h.life.results()),
        format!("{:?}", h.life.held()),
        format!("{:?}", h.log.borrow()),
    ];
    for text in shown {
        assert!(!text.contains("SECRET-PLANTED"), "{text}");
    }
}

/// An agent with no files root refuses a start that carries files, with
/// `files-unsupported`, and creates nothing. Catches an agent that runs the task without
/// the files it was sent.
#[test]
fn an_agent_without_a_files_root_refuses_files() {
    let mut h = Harness::new("files-none");
    let mut config = h.config.clone();
    config.files = None;
    let (life, _) = js_agent::lifecycle::Lifecycle::adopt(
        config,
        Box::new(h.fake.clone()),
        Box::new(js_agent::lifecycle::NoChecks),
        Box::new(h.clock.clone()),
    )
    .unwrap();
    h.life = life;
    let events = names(&h.start(with_files("r1")));
    assert_eq!(events, ["refused r1 files-unsupported"]);
    assert!(h.fake.created().is_empty());
}
