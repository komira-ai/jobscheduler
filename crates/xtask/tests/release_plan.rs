//! `release plan` against real git repositories and the real `cargo metadata`: each test
//! is a history a push to main can have, and what the release should do about it.

mod support;

use std::path::PathBuf;
use std::process::Command;

use support::{Repo, field};
use xtask::release::{Error, plan};

/// A repository whose first commit is released as v0.1.0, as this project's was.
fn released(name: &str) -> Repo {
    let repo = Repo::new(name);
    repo.tag("v0.1.0");
    repo
}

/// Catches a first release that is blocked or numbered wrongly: with no tag yet, the
/// workspace version is released as it is, and nothing exists to be re-run.
#[test]
fn with_no_tag_the_first_release_is_the_workspace_version() {
    let repo = Repo::new("first");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(field(&said, "version"), "0.1.0");
    assert_eq!(field(&said, "tag"), "v0.1.0");
    assert_eq!(field(&said, "tag_exists"), "false");
    assert_eq!(
        field(&said, "reason"),
        "no release tag yet: the first release"
    );
}

/// Catches an output injection: a shipped file whose name holds a line end and looks like
/// another output (`release=false`) must not become one in the file the workflow reads.
#[test]
fn a_file_name_cannot_add_an_output() {
    let repo = released("injection");
    repo.write("crates/app/src/a\nrelease=false\n.rs", "// code\n");
    repo.commit("a name with line ends");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(said.lines().count(), 5, "{said}");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(
        field(&said, "reason"),
        "crates/app/src/a?release=false?.rs changed since v0.1.0"
    );
}

/// Catches a release for a change that ships nothing: documentation (root or inside a
/// crate), a workflow, a shipped crate's tests, and the source of a crate no shipped binary
/// depends on (the build, coverage and conformance tooling) must not cut a version.
#[test]
fn changes_that_ship_nothing_are_not_released() {
    let repo = released("nothing");
    repo.write("docs/release.md", "words\n");
    repo.write("README.md", "more words\n");
    repo.write("crates/app/README.md", "about app\n");
    repo.write(".github/workflows/ci.yml", "name: ci\n");
    repo.write("crates/app/tests/cli.rs", "// a test\n");
    repo.write("crates/tools/src/lib.rs", "// tooling\n");
    repo.write("crates/fake/src/lib.rs", "// only tests link this\n");
    repo.commit("things that do not ship");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "false");
    assert_eq!(
        field(&said, "reason"),
        "nothing that ships changed since v0.1.0"
    );
    // The version and tag are still those the next release would have.
    assert_eq!(field(&said, "version"), "0.1.1");
}

/// Catches a shipped change that is not released: the source of the shipped crate, the
/// source of a crate it depends on, its manifest and the root inputs each cut the next
/// patch version, and the reason names the file.
#[test]
fn a_change_to_shipped_code_releases_the_next_patch() {
    for (name, path) in [
        ("app-src", "crates/app/src/lib.rs"),
        ("lib-src", "crates/lib/src/lib.rs"),
        ("app-manifest", "crates/app/Cargo.toml"),
        ("lock", "Cargo.lock"),
    ] {
        let repo = released(name);
        repo.write("docs/release.md", "words\n");
        // A comment is valid in all of them (the manifests must stay readable).
        let before = std::fs::read_to_string(repo.dir.join(path)).expect("file");
        repo.write(path, &format!("{before}\n# changed {name}\n"));
        repo.commit("change");
        let said = plan::run(&repo.plan()).expect("plan");
        assert_eq!(field(&said, "release"), "true", "{path}");
        assert_eq!(field(&said, "version"), "0.1.1", "{path}");
        assert_eq!(field(&said, "tag"), "v0.1.1", "{path}");
        assert_eq!(field(&said, "tag_exists"), "false", "{path}");
        assert_eq!(
            field(&said, "reason"),
            format!("{path} changed since v0.1.0")
        );
    }
}

/// Catches a file compiled into a binary from outside `src/` (an `include_str!` of a
/// sample configuration) or a cargo setting under `.cargo/` that is not released.
#[test]
fn a_compiled_in_file_outside_src_releases() {
    for (name, path) in [
        ("example", "crates/app/example.toml"),
        ("cargo-config", ".cargo/config.toml"),
    ] {
        let repo = released(name);
        repo.write(path, "# changed\n");
        repo.commit("change");
        let said = plan::run(&repo.plan()).expect("plan");
        assert_eq!(field(&said, "release"), "true", "{path}");
        assert_eq!(
            field(&said, "reason"),
            format!("{path} changed since v0.1.0")
        );
    }
}

/// Catches a file that is compiled in (`include_str!`, `include_bytes!`, `include!`) from
/// a place the blanket exclusions cover (`examples/`, markdown, `tests/`) not releasing:
/// what a shipped source includes ships, whatever its location; the same kind of file that
/// nothing includes, and the sources of tests, still do not.
#[test]
fn a_file_a_shipped_source_includes_releases_wherever_it_is() {
    let repo = Repo::new("included");
    repo.write(
        "crates/app/src/lib.rs",
        "pub const A: &str = include_str!(\"../examples/sample.toml\");\n\
         pub const B: &[u8] = include_bytes!(\n    \"../help.md\"\n);\n\
         include!(\"./gen/table.rs\");\n\
         pub const C: &str = include_str!(\"../../lib/tests/data.txt\");\n",
    );
    repo.write("crates/app/src/gen/table.rs", "// table\n");
    repo.write("crates/app/examples/sample.toml", "a = 1\n");
    repo.write("crates/app/examples/unused.toml", "a = 1\n");
    repo.write("crates/app/help.md", "help\n");
    repo.write("crates/lib/tests/data.txt", "data\n");
    // A test may include whatever it likes, in any form: it is not compiled into a binary.
    repo.write(
        "crates/app/tests/t.rs",
        "include_str!(concat!(\"a\", \"b\"));\n",
    );
    repo.commit("includes");
    repo.tag("v0.1.0");
    for path in [
        "crates/app/examples/sample.toml",
        "crates/app/help.md",
        "crates/lib/tests/data.txt",
    ] {
        repo.write(path, "changed\n");
        repo.commit("change an included file");
        let said = plan::run(&repo.plan()).expect("plan");
        assert_eq!(field(&said, "release"), "true", "{path}");
        assert_eq!(
            field(&said, "reason"),
            format!("{path} changed since v0.1.0")
        );
        repo.git(&["tag", "-d", "v0.1.0"]);
        repo.tag("v0.1.0");
    }
    repo.write("crates/app/examples/unused.toml", "changed\n");
    repo.commit("change a file nothing includes");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "false");
}

/// Catches an include that cannot be read as a path being skipped: the plan cannot tell
/// what such a source compiles in, so it fails rather than ship less; and a source it
/// cannot read fails too.
#[test]
fn an_include_that_is_not_a_plain_path_fails_the_plan() {
    let repo = released("include-bad");
    repo.write(
        "crates/app/src/lib.rs",
        "pub const A: &str = include_str!(concat!(env!(\"X\"), \"/a.toml\"));\n",
    );
    repo.commit("computed include");
    let Err(Error::Include { file, line }) = plan::run(&repo.plan()) else {
        panic!("a computed include should fail the plan")
    };
    assert_eq!((file.as_str(), line), ("crates/app/src/lib.rs", 1));

    repo.write("crates/app/src/lib.rs", "");
    repo.write("crates/app/src/extra.rs", "");
    repo.commit("fixed, and a source added");
    std::fs::remove_file(repo.dir.join("crates/app/src/extra.rs")).expect("remove");
    let result = plan::run(&repo.plan());
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
}

/// Catches a minor or major bump that is ignored (the release would be a patch above the
/// tag), and one that is repeated (after the bump is released, the next push must be a
/// patch above it, not the bumped number again).
#[test]
fn a_workspace_version_above_the_tag_is_released_as_it_is() {
    let repo = released("bump");
    repo.write(
        "Cargo.toml",
        &std::fs::read_to_string(repo.dir.join("Cargo.toml"))
            .expect("manifest")
            .replace("version = \"0.1.0\"", "version = \"0.2.0\""),
    );
    repo.commit("raise the version");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(field(&said, "version"), "0.2.0");
    assert_eq!(field(&said, "reason"), "Cargo.toml changed since v0.1.0");

    repo.tag("v0.2.0");
    repo.write("crates/lib/src/lib.rs", "// next\n");
    repo.commit("code");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "version"), "0.2.1");
}

/// Catches ordering tags as text or by date: v0.1.10 is above v0.1.9, and the range is
/// measured from the highest tag, so a shipped change between two old tags is not released
/// again. Tags that are not releases are ignored.
#[test]
fn the_range_starts_at_the_highest_version_tag() {
    let repo = released("order");
    repo.write("crates/app/src/lib.rs", "// shipped\n");
    repo.commit("code");
    repo.tag("v0.1.9");
    repo.write("docs/a.md", "b\n");
    repo.commit("docs");
    repo.tag("v0.1.10");
    repo.tag("vnext");
    repo.write("docs/a.md", "c\n");
    repo.commit("more docs");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "false");
    assert_eq!(
        field(&said, "reason"),
        "nothing that ships changed since v0.1.10"
    );
    assert_eq!(field(&said, "version"), "0.1.11");
}

/// Catches a release of the same change twice: a push made after code that changed nothing
/// that ships does not release (the code before it was released by its own run), and a
/// docs-only push after an unreleased code push still releases that code.
#[test]
fn unreleased_code_is_released_by_a_later_docs_push() {
    let repo = released("later");
    repo.write("crates/lib/src/lib.rs", "// code\n");
    repo.commit("code that was never released");
    repo.write("docs/a.md", "words\n");
    repo.commit("docs");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(
        field(&said, "reason"),
        "crates/lib/src/lib.rs changed since v0.1.0"
    );
}

/// Catches a re-run that makes a second version: when the latest tag already names the
/// commit, the run is that release's, with its version and `tag_exists` so the tag is not
/// made again, even though nothing changed since the tag.
#[test]
fn a_rerun_of_a_started_release_keeps_its_tag() {
    let repo = released("rerun");
    repo.write("crates/lib/src/lib.rs", "// code\n");
    repo.commit("code");
    repo.tag("v0.1.1");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(field(&said, "version"), "0.1.1");
    assert_eq!(field(&said, "tag"), "v0.1.1");
    assert_eq!(field(&said, "tag_exists"), "true");
    assert_eq!(
        field(&said, "reason"),
        "v0.1.1 already names this commit: a re-run of its release"
    );
}

/// Catches a build attested as the wrong commit: the attestation names the commit the run
/// is at (the tip of main), so when main has moved past the commit that passed `ci`, that
/// commit is not released and the newer push releases its change.
#[test]
fn a_commit_main_has_moved_past_is_not_released() {
    let repo = released("moved");
    repo.write("crates/app/src/lib.rs", "// code\n");
    let head = repo.commit("code");
    let mut args = repo.plan();
    args.tip = "0".repeat(40);
    let said = plan::run(&args).expect("plan");
    assert_eq!(field(&said, "release"), "false");
    assert_eq!(
        field(&said, "reason"),
        format!(
            "main is at {} now, past {head}: the push that moved it releases this change",
            "0".repeat(40)
        )
    );
}

/// Catches a re-run of an old release run that publishes an old commit as the newest
/// version: a commit that a later release tag is at or above is not released, whether or
/// not that commit has its own tag, even when the tip it is told is the commit itself.
#[test]
fn a_commit_below_a_later_release_is_not_released() {
    for (name, tag_old) in [("below-untagged", false), ("below-tagged", true)] {
        let repo = released(name);
        repo.write("crates/app/src/lib.rs", "// first\n");
        let old = repo.commit("first");
        if tag_old {
            repo.tag("v0.1.1");
        }
        repo.write("crates/app/src/lib.rs", "// second\n");
        repo.commit("second");
        repo.tag("v0.1.2");
        repo.git(&["checkout", "--quiet", &old]);
        let said = plan::run(&repo.plan()).expect("plan");
        assert_eq!(field(&said, "release"), "false", "{name}");
        assert_eq!(
            field(&said, "reason"),
            format!("v0.1.2 is a release past {old}: a newer release exists"),
            "{name}"
        );
    }
}

/// Catches a re-run of an old release run when the live tip is told truthfully: main is
/// at the newer commit, so the old commit is not released.
#[test]
fn a_rerun_of_an_old_release_run_finds_main_moved() {
    let repo = released("old-run");
    repo.write("crates/app/src/lib.rs", "// first\n");
    let old = repo.commit("first");
    repo.tag("v0.1.1");
    repo.write("crates/app/src/lib.rs", "// second\n");
    let newer = repo.commit("second");
    repo.tag("v0.1.2");
    repo.git(&["checkout", "--quiet", &old]);
    let mut args = repo.plan();
    args.tip = newer.clone();
    let said = plan::run(&args).expect("plan");
    assert_eq!(field(&said, "release"), "false");
    assert_eq!(
        field(&said, "reason"),
        format!("main is at {newer} now, past {old}: the push that moved it releases this change")
    );
}

/// Catches the range starting at a tag the commit does not contain: a higher tag on a
/// side branch (or pushed by mistake) is not the latest release of this history, so the
/// version counts from the highest tag merged into the commit.
#[test]
fn a_higher_tag_that_is_not_merged_is_ignored() {
    let repo = released("side-tag");
    repo.git(&["checkout", "--quiet", "-b", "side"]);
    repo.write("crates/lib/src/lib.rs", "// side\n");
    repo.commit("side");
    repo.tag("v0.9.0");
    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("crates/app/src/lib.rs", "// main\n");
    repo.commit("main");
    let said = plan::run(&repo.plan()).expect("plan");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(field(&said, "version"), "0.1.1");
    assert_eq!(field(&said, "tag_exists"), "false");
    assert_eq!(
        field(&said, "reason"),
        "crates/app/src/lib.rs changed since v0.1.0"
    );
}

/// Catches a file that left a shipped directory going unnoticed: git reports a move as a
/// rename and shows only the new name, so the old name (in a shipped directory) must be
/// listed too. Deleting a shipped file changes the binary as well.
#[test]
fn moving_or_deleting_a_shipped_file_releases() {
    for (name, destination) in [("move-out", Some("docs/helper.txt")), ("delete", None)] {
        let repo = Repo::new(name);
        let text = "// a helper that is long enough\n// to be recognised as the same file\n";
        repo.write("crates/lib/src/helper.rs", text);
        repo.commit("helper");
        repo.tag("v0.1.0");
        repo.git(&["rm", "--quiet", "crates/lib/src/helper.rs"]);
        if let Some(to) = destination {
            repo.write(to, text);
        }
        repo.commit("remove the helper from the crate");
        let said = plan::run(&repo.plan()).expect("plan");
        assert_eq!(field(&said, "release"), "true", "{name}");
        assert_eq!(
            field(&said, "reason"),
            "crates/lib/src/helper.rs changed since v0.1.0",
            "{name}"
        );
    }
}

/// Catches a dry run that is skipped for the very change it is meant to exercise (a pull
/// request that only touches the workflow), or that hides the decision: it says released
/// and still reports what the real run would have done.
#[test]
fn a_dry_run_always_builds_and_says_what_it_decided() {
    let repo = released("dry");
    repo.write(".github/workflows/release.yml", "name: release\n");
    repo.commit("workflow");
    let mut args = repo.plan();
    args.dry_run = true;
    let said = plan::run(&args).expect("plan");
    assert_eq!(field(&said, "release"), "true");
    assert_eq!(field(&said, "tag_exists"), "false");
    assert_eq!(
        field(&said, "reason"),
        "dry run (decided: nothing that ships changed since v0.1.0)"
    );
}

/// Catches a plan about one commit made in a checkout of another.
#[test]
fn the_checkout_must_be_the_head() {
    let repo = Repo::new("wrong");
    let mut args = repo.plan();
    args.head = "1".repeat(40);
    assert_eq!(
        plan::run(&args),
        Err(Error::WrongCheckout {
            expected: "1".repeat(40),
            actual: repo.head()
        })
    );
    assert!(
        Error::WrongCheckout {
            expected: "a".into(),
            actual: "b".into()
        }
        .to_string()
        .contains("not at --head a")
    );
}

/// Catches an output file that is replaced (a workflow step's other outputs would be lost)
/// or not written, and a failure to write it that passes.
#[test]
fn the_lines_are_appended_to_the_output_file() {
    let repo = Repo::new("output");
    let out = repo.dir.join("../output.txt");
    std::fs::write(&out, "earlier=1\n").expect("earlier");
    let mut args = repo.plan();
    args.output = Some(out.clone());
    let said = plan::run(&args).expect("plan");
    let written = std::fs::read_to_string(&out).expect("output");
    assert_eq!(written, format!("earlier=1\n{said}\n"));

    args.output = Some(repo.dir.join("no/such/dir/output"));
    assert!(matches!(plan::run(&args), Err(Error::Io { .. })));
}

/// Catches a plan that guesses when its inputs are wrong: a shipped crate that does not
/// exist, a checkout that is not a repository, programs that are not there, and a
/// manifest that is gone or has no version each fail with the reason.
#[test]
fn bad_inputs_fail_with_a_reason() {
    let repo = released("bad");
    repo.write("crates/app/src/lib.rs", "// code\n");
    repo.commit("code");

    let mut args = repo.plan();
    args.ship = vec!["nope".to_owned()];
    assert_eq!(
        plan::run(&args),
        Err(Error::UnknownCrate("nope".to_owned()))
    );

    let mut args = repo.plan();
    args.git = PathBuf::from("/nonexistent/git");
    assert!(matches!(plan::run(&args), Err(Error::Spawn { .. })));

    let mut args = repo.plan();
    args.cargo = PathBuf::from("/nonexistent/cargo");
    assert!(matches!(plan::run(&args), Err(Error::Spawn { .. })));

    let mut args = repo.plan();
    // Not a repository (and not inside one).
    args.repo_dir = PathBuf::from("/");
    assert!(matches!(plan::run(&args), Err(Error::Failed { .. })));

    let mut args = repo.plan();
    args.cargo = PathBuf::from("true");
    assert!(matches!(plan::run(&args), Err(Error::Metadata(_))));

    std::fs::write(repo.dir.join("Cargo.toml"), "[workspace]\n").expect("manifest");
    assert!(matches!(plan::run(&repo.plan()), Err(Error::Manifest(_))));
    std::fs::remove_file(repo.dir.join("Cargo.toml")).expect("remove");
    assert!(matches!(plan::run(&repo.plan()), Err(Error::Io { .. })));
}

/// Catches the verbs not being wired to the command line: the flags, the defaults, the exit
/// status 0 with the lines on stdout, 1 with the reason on stderr, 2 for a bad version.
#[test]
fn the_command_line_runs_plan_and_stamp() {
    let xtask = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .expect("xtask")
    };
    let repo = released("cli");
    repo.write("crates/app/src/lib.rs", "// code\n");
    repo.commit("code");
    let dir = repo.dir.to_str().expect("utf-8");
    let head = repo.head();

    let out = xtask(&[
        "release",
        "plan",
        "--head",
        &head,
        "--tip",
        &head,
        "--repo-dir",
        dir,
        "--ship",
        "app",
        "--cargo",
        env!("CARGO"),
        "--dry-run",
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let said = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(field(&said, "release"), "true");

    // The default shipped crates are this project's, which the fixture does not have.
    let out = xtask(&[
        "release",
        "plan",
        "--head",
        &head,
        "--tip",
        &head,
        "--repo-dir",
        dir,
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("crate js-lease-server is not a workspace member")
    );

    let out = xtask(&["release", "stamp", "--to", "0.4.2", "--repo-dir", dir]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let manifest = std::fs::read_to_string(repo.dir.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("version = \"0.4.2\""), "{manifest}");

    let out = xtask(&["release", "stamp", "--to", "0.4", "--repo-dir", dir]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = xtask(&[
        "release",
        "stamp",
        "--to",
        "0.4.3",
        "--repo-dir",
        "/nonexistent",
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}
