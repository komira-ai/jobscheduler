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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::dir::TestDir;
use xtask::release::stage::{DECLARATION_MEMBER, DECLARATION_SOURCE, StageArgs, run};

const TARGET: &str = "x86_64-unknown-linux-musl";

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

/// A checkout with the agent built and the files an archive takes.
fn checkout(tmp: &TestDir) -> &Path {
    let dir = tmp.path();
    let release = dir.join(format!("target/{TARGET}/release"));
    std::fs::create_dir_all(&release).expect("target");
    std::fs::write(release.join("js-agent"), "the agent").expect("binary");
    std::fs::write(dir.join("LICENSE"), "license").expect("license");
    let declaration = dir.join(DECLARATION_SOURCE);
    std::fs::create_dir_all(declaration.parent().expect("parent")).expect("packaging");
    std::fs::write(&declaration, "name = \"js-agent\"\n").expect("declaration");
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
