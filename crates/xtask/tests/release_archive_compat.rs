//! The Linux js-agent archive gains one member, `updater.d/js-agent.toml`, and keeps its name,
//! its top directory and its other members. Both archives here are made the way a release makes
//! them, and both are read by the program a host reads them with:
//!
//! - the archive of a previous release: the step that release ran (`mkdir`, `cp` of the binary
//!   and `LICENSE`, then GNU `tar` with the workflow's options and `gzip -n`);
//! - the archive of this change: `cargo xtask release stage`, then the same `tar` and `gzip`;
//! - the reader: GNU `tar -xzf`, as docs/release.md has a host unpack an archive, after which
//!   the host runs `<archive name>/js-agent`; and, for the declaration, the file
//!   `<archive name>/updater.d/js-agent.toml`, which this change adds and a previous release
//!   lacks.
//!
//! So a host that follows the previous release's steps unpacks a new archive to the same binary
//! and license, and a host that looks for the declaration finds none in a previous archive.
//!
//! The arm64 Linux archive is made by the same step with the real `gzip -n`, twice from one staged
//! tree, and the two must be the same bytes: the archiving half of the reproducibility the release
//! relies on (the other half, the comparison against a published asset, is `publish`'s `cmp`).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

use js_testkit::dir::TestDir;
use xtask::release::stage::{DECLARATION_MEMBER, DECLARATION_SOURCE, StageArgs, run};

const TARGET: &str = "x86_64-unknown-linux-musl";
const ARM_TARGET: &str = "aarch64-unknown-linux-musl";

/// The commit time the archives below are made with (`--mtime=@<t>`).
const COMMIT_TIME: u64 = 1_790_000_000;

/// The options of the release workflow's archive line after `-C "stage-$bin"`, as the
/// workflow spells them; [`archive_with_gzip`] runs the same with the variables filled in.
const WORKFLOW_ARCHIVE_OPTIONS: &str = r#"--sort=name --mtime="@$mtime" --owner=0 --group=0 --numeric-owner -cf - "$name" | gzip -n > "dist/$name.tar.gz""#;

/// `tar -C <stage> --sort=name --mtime=@<t> --owner=0 --group=0 --numeric-owner -cf - <name> |
/// gzip -n`, as the release workflow's build step runs it (gzip with no name or time is what
/// flate2 writes).
fn archive(stage: &Path, name: &str, out: &Path) -> PathBuf {
    let tarred = Command::new("tar")
        .arg("-C")
        .arg(stage)
        .args([
            "--sort=name",
            "--mtime=@1790000000",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
            "-",
            name,
        ])
        .output()
        .expect("tar");
    assert!(tarred.status.success(), "{tarred:?}");
    let file = out.join(format!("{name}.tar.gz"));
    let mut gz = flate2::write::GzEncoder::new(
        std::fs::File::create(&file).expect("archive file"),
        flate2::Compression::default(),
    );
    gz.write_all(&tarred.stdout).expect("gzip");
    gz.finish().expect("gzip");
    file
}

/// `tar -xzf <archive> -C <dir>`: what a host runs; the directory it unpacked to.
fn unpack(archive: &Path, into: &Path) -> PathBuf {
    std::fs::create_dir_all(into).expect("into");
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .expect("tar");
    assert!(out.status.success(), "{out:?}");
    into.to_owned()
}

/// What `tar -tzf` lists.
fn listed(archive: &Path) -> Vec<String> {
    let out = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .output()
        .expect("tar");
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A checkout with the agent built for `TARGET` and the files an archive takes.
fn checkout(tmp: &TestDir) -> &Path {
    checkout_for(tmp, TARGET)
}

/// A checkout with the agent built for `target` and the files an archive takes.
fn checkout_for<'a>(tmp: &'a TestDir, target: &str) -> &'a Path {
    let dir = tmp.path();
    let release = dir.join(format!("target/{target}/release"));
    std::fs::create_dir_all(&release).expect("target");
    std::fs::write(release.join("js-agent"), "the agent").expect("binary");
    std::fs::write(dir.join("LICENSE"), "license").expect("license");
    let declaration = dir.join(DECLARATION_SOURCE);
    std::fs::create_dir_all(declaration.parent().expect("parent")).expect("packaging");
    std::fs::write(&declaration, "name = \"js-agent\"\n").expect("declaration");
    // The modes a build leaves, whatever the umask: an executable binary, plain data files.
    for (file, mode) in [
        (release.join("js-agent"), 0o755),
        (dir.join("LICENSE"), 0o644),
        (declaration, 0o644),
    ] {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).expect("mode");
    }
    dir
}

/// A new archive unpacked as a host following a previous release's steps does gives the same
/// binary and license at the same paths; the declaration is the one member more. Catches a
/// layout change (a renamed top directory, a moved binary) that host would trip on.
#[test]
fn a_new_archive_unpacked_by_a_previous_releases_steps_gives_the_same_files() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "archive-new-read-old");
    let repo = checkout(&tmp);
    let stage = repo.join("stage");
    run(&StageArgs {
        bin: "js-agent".to_owned(),
        target: TARGET.to_owned(),
        version: "1.4.0".parse().expect("version"),
        out: stage.clone(),
        repo_dir: repo.to_owned(),
    })
    .expect("staged");
    let name = format!("js-agent-1.4.0-{TARGET}");
    let file = archive(&stage, &name, repo);
    assert_eq!(
        listed(&file),
        [
            format!("{name}/"),
            format!("{name}/LICENSE"),
            format!("{name}/js-agent"),
            format!("{name}/updater.d/"),
            format!("{name}/updater.d/js-agent.toml"),
        ]
    );
    let host = unpack(&file, &repo.join("host")).join(&name);
    assert_eq!(
        std::fs::read(host.join("js-agent")).expect("binary"),
        b"the agent"
    );
    assert_eq!(
        std::fs::read(host.join("LICENSE")).expect("license"),
        b"license"
    );
    assert_eq!(
        std::fs::read(host.join(DECLARATION_MEMBER)).expect("declaration"),
        b"name = \"js-agent\"\n"
    );
}

/// A previous release's archive, made by that release's own step, unpacks to its binary and
/// license, and holds no declaration: a host or an updater that looks for
/// `updater.d/js-agent.toml` finds the path absent, not another file. Catches a reader that
/// would need the declaration in every archive.
#[test]
fn a_previous_releases_archive_unpacks_to_its_binary_and_no_declaration() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "archive-old-read-new");
    let repo = checkout(&tmp);
    let name = format!("js-agent-1.3.0-{TARGET}");
    // The previous release's step: mkdir "stage-$bin/$name"; cp "target/$TARGET/release/$bin"
    // LICENSE "stage-$bin/$name/".
    let stage = repo.join("stage-js-agent");
    std::fs::create_dir_all(stage.join(&name)).expect("mkdir");
    for source in [
        format!("target/{TARGET}/release/js-agent"),
        "LICENSE".to_owned(),
    ] {
        let base = Path::new(&source).file_name().expect("a name");
        std::fs::copy(repo.join(&source), stage.join(&name).join(base)).expect("cp");
    }
    let file = archive(&stage, &name, repo);
    assert_eq!(
        listed(&file),
        [
            format!("{name}/"),
            format!("{name}/LICENSE"),
            format!("{name}/js-agent"),
        ]
    );
    let host = unpack(&file, &repo.join("host")).join(&name);
    assert_eq!(
        std::fs::read(host.join("js-agent")).expect("binary"),
        b"the agent"
    );
    assert!(!host.join(DECLARATION_MEMBER).exists());
    assert!(!host.join("updater.d").exists());
}

/// `tar -C <stage> --sort=name --mtime=@COMMIT_TIME --owner=0 --group=0 --numeric-owner -cf -
/// <name> | gzip -n`, run from `cwd`: the workflow's line, with the real `gzip`. The bytes made.
fn archive_with_gzip(stage: &Path, name: &str, cwd: &Path) -> Vec<u8> {
    let mut tar = Command::new("tar")
        .current_dir(cwd)
        .arg("-C")
        .arg(stage)
        .args([
            "--sort=name",
            &format!("--mtime=@{COMMIT_TIME}"),
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
            "-",
            name,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("tar");
    let gzipped = Command::new("gzip")
        .current_dir(cwd)
        .arg("-n")
        .stdin(tar.stdout.take().expect("tar's output"))
        .output()
        .expect("gzip");
    assert!(tar.wait().expect("tar").success(), "tar failed");
    assert!(gzipped.status.success(), "{gzipped:?}");
    gzipped.stdout
}

/// Sets the modification time of `path` and of everything under it to `secs`.
fn touch_all(path: &Path, secs: u64) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).expect("read dir") {
            touch_all(&entry.expect("entry").path(), secs);
        }
    }
    std::fs::File::open(path)
        .expect("open")
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .expect("set the time");
}

/// The arm64 Linux agent archive made twice from one staged tree, the second time from another
/// working directory and with every staged file and directory given another modification time, is
/// the same bytes; its gzip header has no name and no time, and its tar headers say owner 0/0, no
/// owner names, the commit time and the files' modes (the binary 0755, the others 0644); and
/// GNU `tar -xzf` unpacks it to the staged files, each with the commit time, the binary still
/// executable. Catches a name, owner, file time or gzip header field that leaks into
/// the archive, which would make a re-run's file differ from the published one and fail
/// `publish`, and an execute bit lost in packing or unpacking, which would ship an agent a host
/// cannot start.
#[test]
fn an_arm64_archive_is_byte_identical_when_made_again_and_reads_back_with_gnu_tar() {
    // The options here are the workflow's: a change there must come here too.
    let workflow = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml"),
    )
    .expect("read release.yml");
    assert!(
        workflow.contains(&format!(
            r#""$tar" -C "stage-$bin" {WORKFLOW_ARCHIVE_OPTIONS}"#
        )),
        "release.yml no longer archives with {WORKFLOW_ARCHIVE_OPTIONS}"
    );

    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "archive-arm64-again");
    let repo = checkout_for(&tmp, ARM_TARGET);
    let stage = repo.join("stage");
    run(&StageArgs {
        bin: "js-agent".to_owned(),
        target: ARM_TARGET.to_owned(),
        version: "1.4.0".parse().expect("version"),
        out: stage.clone(),
        repo_dir: repo.to_owned(),
    })
    .expect("staged");
    let name = format!("js-agent-1.4.0-{ARM_TARGET}");
    touch_all(&stage.join(&name), COMMIT_TIME - 1);
    let first = archive_with_gzip(&stage, &name, repo);
    let elsewhere = repo.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("another directory");
    touch_all(&stage.join(&name), COMMIT_TIME + 1);
    let second = archive_with_gzip(&stage, &name, &elsewhere);
    assert!(first == second, "the two archives differ");

    // The gzip header (RFC 1952): magic, deflate, no flag (so no name and no comment), and a
    // zero modification time.
    assert_eq!(first[..4], [0x1f, 0x8b, 8, 0], "{:?}", &first[..10]);
    assert_eq!(first[4..8], [0, 0, 0, 0], "{:?}", &first[..10]);

    // The tar headers.
    let mut headers = Vec::new();
    let mut modes = Vec::new();
    let mut read = tar::Archive::new(flate2::read::GzDecoder::new(first.as_slice()));
    for entry in read.entries().expect("entries") {
        let entry = entry.expect("entry");
        let header = entry.header();
        if header.entry_type().is_file() {
            modes.push((
                entry.path().expect("path").display().to_string(),
                header.mode().expect("mode") & 0o777,
            ));
        }
        headers.push((
            entry.path().expect("path").display().to_string(),
            header.uid().expect("uid"),
            header.gid().expect("gid"),
            header.mtime().expect("mtime"),
            header.username_bytes().is_none_or(<[u8]>::is_empty)
                && header.groupname_bytes().is_none_or(<[u8]>::is_empty),
        ));
    }
    let expected: Vec<(String, u64, u64, u64, bool)> = [
        format!("{name}/"),
        format!("{name}/LICENSE"),
        format!("{name}/js-agent"),
        format!("{name}/updater.d/"),
        format!("{name}/updater.d/js-agent.toml"),
    ]
    .into_iter()
    .map(|path| (path, 0, 0, COMMIT_TIME, true))
    .collect();
    assert_eq!(headers, expected);
    // The binary keeps its execute bits in the archive, and the data files get none.
    assert_eq!(
        modes,
        [
            (format!("{name}/LICENSE"), 0o644),
            (format!("{name}/js-agent"), 0o755),
            (format!("{name}/updater.d/js-agent.toml"), 0o644),
        ]
    );

    // What a host unpacks.
    let file = repo.join(format!("{name}.tar.gz"));
    std::fs::write(&file, &first).expect("archive file");
    assert_eq!(
        listed(&file),
        expected.iter().map(|e| e.0.clone()).collect::<Vec<_>>()
    );
    let host = unpack(&file, &repo.join("host")).join(&name);
    for (member, bytes) in [
        ("js-agent", &b"the agent"[..]),
        ("LICENSE", &b"license"[..]),
        (DECLARATION_MEMBER, &b"name = \"js-agent\"\n"[..]),
    ] {
        assert_eq!(
            std::fs::read(host.join(member)).expect(member),
            bytes,
            "{member}"
        );
        let modified = std::fs::metadata(host.join(member))
            .and_then(|m| m.modified())
            .expect("modified");
        assert_eq!(
            modified,
            UNIX_EPOCH + Duration::from_secs(COMMIT_TIME),
            "{member}"
        );
    }
    // What a host runs is executable once unpacked; the data files are not.
    let mode = |member: &str| {
        std::fs::metadata(host.join(member))
            .expect(member)
            .permissions()
            .mode()
    };
    assert_eq!(mode("js-agent") & 0o111, 0o111, "{:o}", mode("js-agent"));
    for member in ["LICENSE", DECLARATION_MEMBER] {
        assert_eq!(mode(member) & 0o111, 0, "{member}: {:o}", mode(member));
    }
}
