//! `release stage`: the directory each release archive is made of, laid down from a checkout of
//! the test's own, with exactly the files `stage::members` lists; the refusals, each naming the
//! path, that leave a directory that exists as it was; names refused before anything is
//! written; and a failed run that leaves nothing behind, so a retry after the cause is fixed
//! succeeds.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use js_testkit::dir::TestDir;
use xtask::release::Error;
use xtask::release::stage::{
    DECLARATION_MEMBER, DECLARATION_SOURCE, Member, StageArgs, members, run,
};

const LINUX: &str = "x86_64-unknown-linux-musl";
const MAC: &str = "aarch64-apple-darwin";

/// A checkout of its own, unique to the test, with the built binaries of both targets, the
/// license and the declaration.
fn checkout(name: &str) -> TestDir {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let dir = tmp.path();
    for target in [LINUX, MAC] {
        let release = dir.join(format!("target/{target}/release"));
        std::fs::create_dir_all(&release).expect("target dir");
        for bin in ["js-agent", "js"] {
            let file = release.join(bin);
            std::fs::write(&file, format!("{bin} for {target}")).expect("binary");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("mode");
        }
    }
    std::fs::write(dir.join("LICENSE"), "license").expect("license");
    let declaration = dir.join(DECLARATION_SOURCE);
    std::fs::create_dir_all(declaration.parent().expect("a parent")).expect("packaging");
    std::fs::write(&declaration, "name = \"js-agent\"\n").expect("declaration");
    tmp
}

fn args(repo: &Path, bin: &str, target: &str) -> StageArgs {
    StageArgs {
        bin: bin.to_owned(),
        target: target.to_owned(),
        version: "1.2.3".parse().expect("a version"),
        out: repo.join("stage"),
        repo_dir: repo.to_owned(),
    }
}

/// Every file and directory under `dir`, relative to it, sorted; empty when it is absent.
fn tree(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(next) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            let rel = path
                .strip_prefix(dir)
                .expect("below")
                .to_str()
                .expect("utf-8");
            if path.is_dir() {
                found.push(format!("{rel}/"));
                todo.push(path);
            } else {
                found.push(rel.to_owned());
            }
        }
    }
    found.sort();
    found
}

/// The Linux js-agent archive holds its binary (still executable), the license and the
/// declaration at `updater.d/js-agent.toml`, each with the bytes of the checkout's file, and
/// nothing else. Catches an archive the updater cannot read the rights of a release from.
#[test]
fn the_linux_agent_archive_holds_the_binary_the_license_and_the_declaration() {
    let tmp = checkout("stage-linux-agent");
    let repo = tmp.path();
    let line = run(&args(repo, "js-agent", LINUX)).expect("staged");
    let top = repo.join(format!("stage/js-agent-1.2.3-{LINUX}"));
    assert_eq!(
        line,
        format!(
            "staged {}: js-agent, LICENSE, updater.d/js-agent.toml",
            top.display()
        )
    );
    assert_eq!(
        tree(&repo.join("stage")),
        [
            format!("js-agent-1.2.3-{LINUX}/"),
            format!("js-agent-1.2.3-{LINUX}/LICENSE"),
            format!("js-agent-1.2.3-{LINUX}/js-agent"),
            format!("js-agent-1.2.3-{LINUX}/updater.d/"),
            format!("js-agent-1.2.3-{LINUX}/updater.d/js-agent.toml"),
        ]
    );
    for (member, source) in [
        ("js-agent", format!("target/{LINUX}/release/js-agent")),
        ("LICENSE", "LICENSE".to_owned()),
        (DECLARATION_MEMBER, DECLARATION_SOURCE.to_owned()),
    ] {
        assert_eq!(
            std::fs::read(top.join(member)).expect("staged file"),
            std::fs::read(repo.join(source)).expect("source"),
            "{member}"
        );
    }
    let mode = std::fs::metadata(top.join("js-agent"))
        .expect("binary")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "{mode:o}");
}

/// Every other archive holds the binary and the license only: the Mac agent's (a Mac runs no
/// generic updater) and the client's.
#[test]
fn every_other_archive_holds_the_binary_and_the_license_only() {
    for (bin, target) in [("js-agent", MAC), ("js", LINUX), ("js", MAC)] {
        let tmp = checkout(&format!("stage-other-{bin}-{target}"));
        let repo = tmp.path();
        let line = run(&args(repo, bin, target)).expect("staged");
        assert!(line.ends_with(&format!(": {bin}, LICENSE")), "{line}");
        assert_eq!(
            tree(&repo.join("stage")),
            [
                format!("{bin}-1.2.3-{target}/"),
                format!("{bin}-1.2.3-{target}/LICENSE"),
                format!("{bin}-1.2.3-{target}/{bin}"),
            ]
        );
        assert_eq!(
            members(bin, target),
            [
                Member {
                    source: format!("target/{target}/release/{bin}"),
                    path: bin.to_owned(),
                },
                Member {
                    source: "LICENSE".to_owned(),
                    path: "LICENSE".to_owned(),
                },
            ]
        );
    }
}

/// A `--bin` or `--target` that is not a plain name (empty, absolute, with a separator, a
/// parent or current directory, hidden or flag-like) is refused by its flag before anything is
/// written: `--out` is not even made. Catches a name that would put the archive's directory
/// outside `--out`.
#[test]
fn a_bin_or_target_that_is_not_a_plain_name_is_refused_before_anything_is_written() {
    let tmp = checkout("stage-names");
    let repo = tmp.path();
    let before = tree(repo);
    let bad = [
        "", "/abs", "/", "a/b", "..", ".", "../up", ".hidden", "-flag", "a b", "é",
    ];
    for word in bad {
        for (flag, a) in [
            ("--bin", args(repo, word, LINUX)),
            ("--target", args(repo, "js-agent", word)),
        ] {
            match run(&a) {
                Err(e @ Error::BadName { .. }) => assert_eq!(
                    e.to_string(),
                    format!(
                        "{flag} {word:?} is not a name: ASCII letters, digits, `.`, `_` and `-`, starting with a letter or digit"
                    )
                ),
                other => panic!("{flag} {word:?}: {other:?}"),
            }
            assert_eq!(tree(repo), before, "{flag} {word:?}");
        }
    }
    // The names a release uses pass, `.`, `_` and `-` inside them included: this one gets as
    // far as the copy, which fails for a binary the checkout lacks, and leaves nothing.
    assert!(matches!(
        run(&args(repo, "js-lease-server", "x86_64-unknown-linux-musl")),
        Err(Error::Io { .. })
    ));
    assert!(
        !repo
            .join("stage/js-lease-server-1.2.3-x86_64-unknown-linux-musl")
            .exists()
    );
}

/// The archive's directory must be new: staging it again is refused, naming it, and leaves what
/// is there. Catches an archive made of two runs' files.
#[test]
fn an_archive_directory_that_exists_is_refused_and_left_as_it_was() {
    let tmp = checkout("stage-again");
    let repo = tmp.path();
    run(&args(repo, "js-agent", LINUX)).expect("staged");
    let before = tree(&repo.join("stage"));
    let top = repo.join(format!("stage/js-agent-1.2.3-{LINUX}"));
    match run(&args(repo, "js-agent", LINUX)) {
        Err(Error::Io { path, why }) => {
            assert_eq!(path, top.display().to_string());
            assert!(why.contains("exists"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tree(&repo.join("stage")), before);
}

/// A file of the checkout that is missing is refused by its path, also after the files before
/// it were copied (the declaration is the last), and the run leaves no archive directory
/// behind: once the file is there, the same run succeeds. Catches a half-staged directory that
/// a retry would refuse, or archive.
#[test]
fn a_missing_source_is_refused_by_its_path_leaves_nothing_and_a_retry_succeeds() {
    for gone in [
        format!("target/{LINUX}/release/js-agent"),
        "LICENSE".to_owned(),
        DECLARATION_SOURCE.to_owned(),
    ] {
        let tmp = checkout(&format!("stage-missing-{}", gone.replace('/', "-")));
        let repo = tmp.path();
        let kept = std::fs::read(repo.join(&gone)).expect("source");
        std::fs::remove_file(repo.join(&gone)).expect("remove");
        match run(&args(repo, "js-agent", LINUX)) {
            Err(Error::Io { path, .. }) => {
                assert_eq!(path, repo.join(&gone).display().to_string());
            }
            other => panic!("{gone}: {other:?}"),
        }
        // `--out` was made; the archive's directory is gone again.
        assert_eq!(tree(&repo.join("stage")), Vec::<String>::new(), "{gone}");
        std::fs::write(repo.join(&gone), kept).expect("put back");
        run(&args(repo, "js-agent", LINUX)).unwrap_or_else(|e| panic!("{gone}: {e}"));
        assert_eq!(tree(&repo.join("stage")).len(), 5, "{gone}");
    }
}

/// An output directory that cannot be made (its parent is a file) is refused by its path.
#[test]
fn an_output_directory_that_cannot_be_made_is_refused() {
    let tmp = checkout("stage-out-under-a-file");
    let repo = tmp.path();
    let mut a = args(repo, "js-agent", LINUX);
    a.out = repo.join("LICENSE/stage");
    match run(&a) {
        Err(Error::Io { path, .. }) => assert_eq!(path, a.out.display().to_string()),
        other => panic!("{other:?}"),
    }
}

/// The verb on the command line: the flags, exit 0 with the line on stdout, 1 with the reason
/// on stderr, 2 for a version that is not `MAJOR.MINOR.PATCH`.
#[test]
fn the_command_line_runs_stage() {
    let tmp = checkout("stage-cli");
    let repo = tmp.path();
    let xtask = |version: &str| {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["release", "stage", "--bin", "js-agent", "--target", LINUX])
            .args(["--version", version, "--out"])
            .arg(repo.join("stage"))
            .arg("--repo-dir")
            .arg(repo)
            .output()
            .expect("xtask")
    };
    let out = xtask("1.2.3");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).ends_with("updater.d/js-agent.toml\n"),
        "{out:?}"
    );
    assert!(
        repo.join(format!(
            "stage/js-agent-1.2.3-{LINUX}/updater.d/js-agent.toml"
        ))
        .is_file()
    );
    let out = xtask("1.2.3");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("xtask: "),
        "{out:?}"
    );
    let out = xtask("1.2");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}
