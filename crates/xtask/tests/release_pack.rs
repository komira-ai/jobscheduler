//! `release pack`, run in process on a scratch checkout, with `xtask-fake` as git (it prints the
//! file `cargo-messages` of its working directory, here the commit's time): the archives it
//! writes, read back with the `tar` and `flate2` crates, and each way it fails.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use js_testkit::dir::TestDir;
use xtask::release::pack::{PackArgs, archive, run};

const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const TARGET: &str = "aarch64-apple-darwin";
const COMMIT_TIME: u64 = 1_790_000_000;

/// A checkout with LICENSE and the built `js` and `js-agent`, whose git (the stand-in) prints
/// `time`.
fn checkout(name: &str, time: &str) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let bin = dir.path().join(format!("target/{TARGET}/release"));
    std::fs::create_dir_all(&bin).expect("mkdir");
    std::fs::write(dir.path().join("LICENSE"), "licence text\n").expect("write");
    for program in ["js", "js-agent"] {
        let path = bin.join(program);
        std::fs::write(&path, format!("{program} binary")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    std::fs::write(dir.path().join("cargo-messages"), time).expect("write");
    dir
}

fn args(repo: &Path, git: &str) -> PackArgs {
    PackArgs {
        bins: "js js-agent".to_owned(),
        target: TARGET.to_owned(),
        version: "1.4.0".parse().expect("version"),
        mtime_of: "abc123".to_owned(),
        out: repo.join("dist"),
        stage: repo.join("stage"),
        repo_dir: repo.to_owned(),
        git: git.to_owned(),
    }
}

/// Each entry of a `.tar.gz`: path, kind, mode, mtime, owner, group, owner name, bytes.
type Entry = (String, char, u32, u64, u64, u64, String, Vec<u8>);

fn read_back(bytes: &[u8]) -> Vec<Entry> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut tar)
        .expect("gzip");
    let mut archive = tar::Archive::new(tar.as_slice());
    let mut out = Vec::new();
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let h = entry.header().clone();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).expect("data");
        out.push((
            entry.path().expect("path").display().to_string(),
            if h.entry_type().is_dir() { 'd' } else { 'f' },
            h.mode().expect("mode"),
            h.mtime().expect("mtime"),
            h.uid().expect("uid"),
            h.gid().expect("gid"),
            h.username().expect("name").unwrap_or_default().to_owned(),
            data,
        ));
    }
    out
}

/// Catches an archive of a binary missing, named after another version or target, holding its
/// files in another order or under another directory, with the staged files' own times, owners or
/// names instead of the commit's time and 0/0, or with a mode lost (an agent a host cannot run);
/// and the git asked for something but the commit's committer time.
#[test]
fn each_binary_is_packed_with_the_commits_time_and_owner_0() {
    let dir = checkout("pack-ok", &format!("{COMMIT_TIME}\n"));
    let repo = dir.path();
    let lines = run(&args(repo, FAKE)).expect("packed");
    assert_eq!(lines.lines().count(), 2, "{lines}");
    let calls = std::fs::read_to_string(repo.join("target/debug/cargo-args")).expect("calls");
    assert_eq!(calls, "log -1 --format=%ct abc123\n");
    let mut names: Vec<String> = std::fs::read_dir(repo.join("dist"))
        .expect("dist")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "js-1.4.0-aarch64-apple-darwin.tar.gz",
            "js-agent-1.4.0-aarch64-apple-darwin.tar.gz"
        ]
    );
    let bytes = std::fs::read(repo.join("dist/js-agent-1.4.0-aarch64-apple-darwin.tar.gz"))
        .expect("archive");
    let top = "js-agent-1.4.0-aarch64-apple-darwin";
    let entry = |path: &str, kind, mode, data: &[u8]| -> Entry {
        (
            path.to_owned(),
            kind,
            mode,
            COMMIT_TIME,
            0,
            0,
            String::new(),
            data.to_vec(),
        )
    };
    assert_eq!(
        read_back(&bytes),
        [
            entry(&format!("{top}/"), 'd', 0o755, b""),
            entry(&format!("{top}/LICENSE"), 'f', 0o644, b"licence text\n"),
            entry(&format!("{top}/js-agent"), 'f', 0o755, b"js-agent binary"),
        ]
    );
    // The gzip header (RFC 1952): magic, deflate, no flag (no name), and no time.
    assert_eq!(bytes[..8], [0x1f, 0x8b, 8, 0, 0, 0, 0, 0]);
}

/// Catches an archive that depends on the staged files' own times or on where it is made: the
/// same staged tree packed twice, its files touched in between, is the same bytes.
#[test]
fn the_same_tree_packs_to_the_same_bytes() {
    let dir = checkout("pack-again", "1\n");
    let base = dir.path().join("tree");
    std::fs::create_dir_all(base.join("top/sub")).expect("mkdir");
    std::fs::write(base.join("top/b"), "b").expect("write");
    std::fs::write(base.join("top/sub/a"), "a").expect("write");
    let first = archive(&base, "top", COMMIT_TIME).expect("first");
    for path in ["top/b", "top/sub/a"] {
        std::fs::File::open(base.join(path))
            .expect("open")
            .set_modified(UNIX_EPOCH + Duration::from_secs(5))
            .expect("touch");
    }
    let second = archive(&base, "top", COMMIT_TIME).expect("second");
    assert!(first == second, "the two archives differ");
    let paths: Vec<String> = read_back(&first).into_iter().map(|e| e.0).collect();
    assert_eq!(paths, ["top/", "top/b", "top/sub/", "top/sub/a"]);
}

/// Catches a commit time git did not give taken as some time: git that cannot start, that fails,
/// or that prints something other than a number each end the run before anything is written.
#[test]
fn a_commit_time_git_does_not_give_ends_the_run() {
    let dir = checkout("pack-no-time", "yesterday\n");
    let repo = dir.path();
    let failed = |git: &str| run(&args(repo, git)).expect_err("refused").to_string();
    assert_eq!(
        failed(FAKE),
        format!("{FAKE} log -1 --format=%ct abc123 printed \"yesterday\", not a count")
    );
    let missing = repo.join("no-git").display().to_string();
    assert!(
        failed(&missing).starts_with(&format!("cannot run {missing}: ")),
        "{}",
        failed(&missing)
    );
    assert!(
        failed(XTASK).starts_with(&format!("{XTASK} log -1 --format=%ct abc123 failed ")),
        "{}",
        failed(XTASK)
    );
    assert!(!repo.join("dist").exists());
}

/// Catches a binary that was not built packed as an empty archive, and an archive directory that
/// cannot be made or written taken as written.
#[test]
fn a_binary_or_a_directory_that_is_not_there_ends_the_run() {
    let dir = checkout("pack-missing", &format!("{COMMIT_TIME}\n"));
    let repo = dir.path();
    let not_built = PackArgs {
        bins: "js-bundle".to_owned(),
        ..args(repo, FAKE)
    };
    let failed = run(&not_built).expect_err("refused").to_string();
    assert!(failed.contains("js-bundle"), "{failed}");
    std::fs::write(repo.join("blocked"), "").expect("write");
    let blocked = PackArgs {
        out: repo.join("blocked/dist"),
        ..args(repo, FAKE)
    };
    let failed = run(&blocked).expect_err("refused").to_string();
    assert!(
        failed.starts_with(&repo.join("blocked/dist").display().to_string()),
        "{failed}"
    );
    let file_out = PackArgs {
        out: repo.join("blocked"),
        ..args(repo, FAKE)
    };
    let failed = run(&file_out).expect_err("refused").to_string();
    assert!(
        failed.starts_with(&repo.join("blocked").display().to_string()),
        "{failed}"
    );
    let unread = archive(&PathBuf::from(repo), "no-such-dir", 0).expect_err("refused");
    assert!(unread.to_string().contains("no-such-dir"), "{unread}");
}

/// Catches the verb missing from the command line the workflow runs: `release pack` with a git
/// that cannot start ends 1 naming it.
#[test]
fn the_binary_runs_the_verb() {
    let dir = checkout("pack-cli", "1\n");
    let ran = std::process::Command::new(XTASK)
        .args([
            "release",
            "pack",
            "--bins",
            "js",
            "--target",
            TARGET,
            "--version",
            "1.4.0",
            "--mtime-of",
            "abc123",
            "--out",
            "dist",
            "--git",
            "/nonexistent/git",
        ])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert_eq!(ran.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&ran.stderr).starts_with("xtask: cannot run /nonexistent/git: "),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}
