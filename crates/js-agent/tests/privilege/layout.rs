//! Duty 4: the directory layout, read from the job's side. A bind mount of a lease's
//! directory is made by `crun` as the job's host id, which needs search permission on every
//! directory from `/` down. The podman rule (a `0700` root) denies that; the layout rule of
//! `DirLayout::Searchable` (`0711` root and lease directories, job-owned subdirectories)
//! grants it and still stops the job's ids from listing the other leases or reading what
//! the agent keeps for itself.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::Path;

use js_agent::lifecycle::{DirLayout, FilesRoot, ScratchRoot};
use js_agent_wire::RunFile;

use crate::harness::{JOB, Kv, STRANGER, arg, kv, make_dir, require_root, run_as, work_dir};

fn errno<T>(result: &std::io::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(why) => format!("errno:{}", why.raw_os_error().unwrap_or(-1)),
    }
}

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "walk" => {
            let root = arg("ROOT");
            let lease = root.join("r1");
            let work = lease.join("work");
            Some(vec![
                kv("stat_work", errno(&fs::metadata(&work))),
                kv("write_work", errno(&fs::write(work.join("out"), "x"))),
                kv("list_root", errno(&fs::read_dir(&root))),
                kv("list_lease", errno(&fs::read_dir(&lease))),
                kv(
                    "agent_private",
                    errno(&fs::read(lease.join("agent-only").join("note"))),
                ),
            ])
        }
        "read_file" => {
            let file = arg("FILE");
            let read = fs::read_to_string(&file);
            Some(vec![
                kv("read", errno(&read)),
                kv("text", read.unwrap_or_default()),
                kv(
                    "list_root",
                    errno(&fs::read_dir(file.parent().unwrap().parent().unwrap())),
                ),
            ])
        }
        "plant" => {
            // The job owns its handed-over run directory: it can open it again and leave
            // links in it that point at things only root can touch.
            let dir = arg("DIR");
            let (file, target) = (arg("TARGET_FILE"), arg("TARGET_DIR"));
            let chmod =
                fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
            let sub = fs::create_dir(dir.join("sub"));
            Some(vec![
                kv("chmod", errno(&chmod)),
                kv("sub", errno(&sub)),
                kv("link_file", errno(&symlink(&file, dir.join("link-file")))),
                kv("link_dir", errno(&symlink(&target, dir.join("link-dir")))),
                kv(
                    "link_nested",
                    errno(&symlink(&target, dir.join("sub").join("link-dir"))),
                ),
                kv("link_up", errno(&symlink("..", dir.join("sub").join("up")))),
            ])
        }
        _ => None,
    }
}

/// A scratch root of `layout` with the lease `r1`: a `work` directory given to the job's
/// ids (as the give helper leaves it) and an `agent-only` directory the agent keeps.
fn lease(work: &Path, layout: DirLayout) -> std::path::PathBuf {
    let root = work.join("scratch");
    let mode = if layout == DirLayout::Private {
        0o700
    } else {
        0o711
    };
    make_dir(&root, mode, 0);
    let scratch = ScratchRoot::open_with(&root, layout).unwrap();
    let lease = scratch.create("r1").unwrap();
    make_dir(&lease.join("work"), 0o700, JOB);
    make_dir(&lease.join("agent-only"), 0o700, 0);
    fs::write(lease.join("agent-only").join("note"), "keep out").unwrap();
    root
}

const EACCES: &str = "errno:13";

/// BEFORE: below a private (`0700`) scratch root the job's ids cannot even reach the
/// directory the agent gave them: `crun`'s bind mount would fail. It is the reason the
/// podman layout cannot carry over to the crun runtime.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn below_a_private_root_the_job_ids_cannot_reach_their_own_directory() {
    let work = work_dir("priv-layout-before");
    let root = lease(work.path(), DirLayout::Private);
    let report = run_as(JOB, &[], "walk", &[("ROOT", &root)]);
    report.expect("stat_work", EACCES);
    report.expect("write_work", EACCES);
}

/// AFTER: below a `0711` root with `0711` leases the job's ids reach and write their own
/// directory, cannot list the root (so they learn no other lease's name) or the lease, and
/// cannot read what the agent keeps in the lease for itself (a `0700` directory of root's).
/// It catches a layout that opens more than search permission.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn below_a_searchable_root_the_job_ids_reach_their_directory_and_nothing_else() {
    let work = work_dir("priv-layout-after");
    let root = lease(work.path(), DirLayout::Searchable);
    let report = run_as(JOB, &[], "walk", &[("ROOT", &root)]);
    report.expect("stat_work", "ok");
    report.expect("write_work", "ok");
    report.expect("list_root", EACCES);
    report.expect("list_lease", EACCES);
    report.expect("agent_private", EACCES);
}

/// The run files of a searchable root are closed to everyone until the hand-over: the job's
/// ids read nothing before it, read their contents after it, and a different user reads
/// nothing even after it and cannot list the root. The directory stays `0700`-closed all the
/// way (the agent's, then the job's), reached through a `0711` root. It catches secrets
/// readable by any user who can walk to the path, and a hand-over that gives the job less
/// than it needs.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn run_files_are_unreadable_until_handed_over_and_then_readable_by_the_job_alone() {
    require_root();
    let work = work_dir("priv-layout-files");
    let root = work.path().join("files");
    let files = FilesRoot::open_with(&root, DirLayout::Searchable).unwrap();
    let dir = files
        .write(
            "r1",
            &[RunFile {
                name: "grant".into(),
                contents: "token-123".into(),
            }],
        )
        .unwrap();
    let file = dir.join("grant");

    let before = run_as(JOB, &[], "read_file", &[("FILE", &file)]);
    before.expect("read", EACCES);
    before.expect("list_root", EACCES);

    files.hand_over("r1", JOB, JOB).unwrap();
    let job = run_as(JOB, &[], "read_file", &[("FILE", &file)]);
    job.expect("read", "ok");
    job.expect("text", "token-123");
    job.expect("list_root", EACCES);

    let stranger = run_as(STRANGER, &[], "read_file", &[("FILE", &file)]);
    stranger.expect("read", EACCES);
}

/// The job owns the directory of its run files after the hand-over, so it can plant links
/// in it that point outside (at a root-owned file, a root-owned directory with a file in
/// it, and `..`). Removing the run (`remove`) and sweeping the root (`sweep`) as root must
/// delete the links and never follow them: the targets keep their content, owner and mode.
/// It catches a removal that follows a link or `chown`s or `chmod`s through one.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn links_planted_by_the_job_in_its_run_files_are_removed_and_never_followed() {
    require_root();
    let work = work_dir("priv-layout-links");
    // Root-owned things outside the run files that a followed link would reach.
    let outside = work.path().join("outside");
    make_dir(&outside, 0o755, 0);
    let target_file = outside.join("secret");
    fs::write(&target_file, "root's").unwrap();
    fs::set_permissions(
        &target_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let target_dir = outside.join("keep");
    make_dir(&target_dir, 0o700, 0);
    fs::write(target_dir.join("inside"), "root's too").unwrap();

    let root = work.path().join("files");
    let files = FilesRoot::open_with(&root, DirLayout::Searchable).unwrap();
    let grant = || {
        [RunFile {
            name: "grant".into(),
            contents: "token".into(),
        }]
    };
    let intact = || {
        assert_eq!(fs::read_to_string(&target_file).unwrap(), "root's");
        let meta = fs::metadata(&target_file).unwrap();
        assert_eq!((meta.uid(), meta.mode() & 0o7777), (0, 0o600));
        assert_eq!(
            fs::read_to_string(target_dir.join("inside")).unwrap(),
            "root's too"
        );
        let meta = fs::metadata(&target_dir).unwrap();
        assert_eq!((meta.uid(), meta.mode() & 0o7777), (0, 0o700));
        let meta = fs::metadata(target_dir.join("inside")).unwrap();
        assert_eq!(meta.uid(), 0);
    };
    for run in ["r1", "r2"] {
        let dir = files.write(run, &grant()).unwrap();
        files.hand_over(run, JOB, JOB).unwrap();
        let report = run_as(
            JOB,
            &[],
            "plant",
            &[
                ("DIR", &dir),
                ("TARGET_FILE", &target_file),
                ("TARGET_DIR", &target_dir),
            ],
        );
        for key in [
            "chmod",
            "sub",
            "link_file",
            "link_dir",
            "link_nested",
            "link_up",
        ] {
            report.expect(key, "ok");
        }
    }
    intact();

    files.remove("r1").unwrap();
    assert!(!files.of("r1").exists());
    intact();

    assert_eq!(files.sweep(["kept"]).unwrap(), 1);
    assert!(!files.of("r2").exists());
    intact();
}
