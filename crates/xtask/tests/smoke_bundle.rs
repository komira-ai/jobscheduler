//! The smoke bundle (`packaging/js-smoke-bundle/`, `release smoke-bundle`): the committed tree
//! packs to the digest its recipe names, and nothing else does; the bundle published to a
//! registry in this process and pulled back by digest is the tree, every byte, and a pulled
//! bundle that is not the tree is refused at the first difference; the runbook names what the
//! recipe publishes.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use js_oci::{Digest, Kind, Layout, Published, Tag};
use js_testkit::dir::TestDir;
use js_testkit::oci_registry::{Fixture, REPO};
use xtask::release::smoke_bundle::{
    self, Error, PushFlags, Recipe, compare, pack, printed_line, push_args,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The committed tree.
fn tree() -> PathBuf {
    root().join(smoke_bundle::DIR).join("tree")
}

fn recipe() -> Recipe {
    Recipe::committed().expect("the committed recipe")
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// A copy of the committed tree at `to`: `bin/smoke`, executable.
fn copy_tree(to: &Path) {
    fs::create_dir_all(to.join("bin")).expect("bin");
    fs::copy(tree().join("bin/smoke"), to.join("bin/smoke")).expect("copy");
    fs::set_permissions(to.join("bin/smoke"), fs::Permissions::from_mode(0o755)).expect("mode");
}

/// The committed tree packed into `<dir>/layout`, opened. `js-bundle` runs on a runtime of
/// its own, so the pack runs off this one's.
async fn packed(dir: &Path) -> Layout {
    let out = dir.join("layout");
    tokio::task::block_in_place(|| pack(&recipe(), &tree(), &out)).expect("pack");
    Layout::open(&out).await.expect("layout")
}

/// Proves the digest is a function of the committed tree and the recipe, and that the recipe
/// states it: the tree packed with the recipe gives exactly the recipe's digest, twice. This
/// is the test that refuses a change of the tree or the recipe that forgets the digest.
#[test]
fn the_committed_tree_packs_to_the_recipes_digest() {
    let dir = scratch("smoke-pack");
    let recipe = recipe();
    for out in ["a", "b"] {
        let line = pack(&recipe, &tree(), &dir.join(out)).expect("pack");
        assert_eq!(line, format!("packed {}", recipe.reference()));
    }
    assert_eq!(
        recipe.reference(),
        format!("ghcr.io/komira-ai/js/smoke:1@{}", recipe.digest)
    );
}

/// Proves a tree that changed without its digest is refused, naming both digests and what to
/// do. Catches a pack that takes whatever the tree gives.
#[test]
fn a_changed_tree_is_refused_until_the_recipe_names_its_digest() {
    let dir = scratch("smoke-changed");
    let changed = dir.join("tree");
    copy_tree(&changed);
    fs::write(changed.join("bin/smoke"), "#!/bin/sh\necho changed\n").expect("write");
    let pinned = recipe().digest;
    let refused = pack(&recipe(), &changed, &dir.join("layout")).expect_err("refused");
    let Error::Digest { packed, .. } = &refused else {
        panic!("not a digest refusal: {refused}");
    };
    assert_ne!(packed, &pinned);
    assert_eq!(
        refused.to_string(),
        format!(
            "the tree packs to {packed}, but bundle.toml says {pinned}: put the new digest in bundle.toml, with a new tag"
        )
    );
    let missing = pack(&recipe(), &dir.join("absent"), &dir.join("other")).expect_err("refused");
    assert!(matches!(missing, Error::Bundle(_)), "{missing}");
    let flag_like = Recipe {
        os_version: "--x".to_owned(),
        ..recipe()
    };
    let refused = pack(&flag_like, &changed, &dir.join("third")).expect_err("refused");
    assert!(
        matches!(&refused, Error::Recipe(why) if why.contains("--x")),
        "{refused}"
    );
}

/// Proves the recipe is `js-bundle pack` of one tree named after the repository's last part,
/// whose entrypoint of that name is `bin/<name>`: so a job that names the image and no
/// program runs it. Catches a recipe whose bundle a job cannot run by its name alone.
#[test]
fn the_recipe_is_one_tree_and_one_entrypoint_named_after_the_repository() {
    let recipe = recipe();
    assert_eq!(recipe.name(), "smoke");
    let args = recipe.pack_args(Path::new("/t"), Path::new("/o"));
    let text = args.join(" ");
    assert_eq!(
        text,
        format!(
            "js-bundle pack --out /o --name smoke --bundle-version 1 --os-version 11.0 --tree smoke=/t --license smoke=Apache-2.0 --source-url smoke={} --entrypoint smoke=${{tree:smoke}}/bin/smoke --annotation org.opencontainers.image.source=https://github.com/komira-ai/jobscheduler",
            recipe.source_url
        )
    );
    assert!(tree().join("bin").join(recipe.name()).is_file());
}

/// Proves a recipe that is not one is refused with the reason: a field it does not know, a
/// field it lacks, and a repository with no registry. Catches a typo in bundle.toml that
/// would publish a bundle with a field left at a default.
#[test]
fn a_recipe_that_is_not_one_is_refused() {
    let committed = smoke_bundle::RECIPE;
    let unknown = format!("{committed}\nextra = 1\n");
    let error = Recipe::parse(&unknown)
        .expect_err("unknown field")
        .to_string();
    assert!(
        error.starts_with("bundle.toml: ") && error.contains("extra"),
        "{error}"
    );
    let missing = committed.replace("tag = \"1\"\n", "");
    let error = Recipe::parse(&missing)
        .expect_err("missing field")
        .to_string();
    assert!(error.contains("tag"), "{error}");
    let bare = committed.replace("ghcr.io/komira-ai/js/smoke", "smoke");
    assert_eq!(
        Recipe::parse(&bare).expect_err("bare").to_string(),
        "bundle.toml: repository \"smoke\" is not REGISTRY/REPOSITORY"
    );
}

/// Proves the push is `js-bundle push` of the recipe's digest to its repository and tag, with
/// the credentials given and over https. Catches a push of whatever manifest the layout
/// lists, or to another tag.
#[test]
fn the_push_is_the_recipes_digest_to_its_repository_and_tag() {
    let recipe = recipe();
    let flags = PushFlags {
        layout: PathBuf::from("/l"),
        user: "u".to_owned(),
        password_file: PathBuf::from("/dev/stdin"),
    };
    let args = push_args(&recipe, &flags);
    assert_eq!(args.source.layout, PathBuf::from("/l"));
    assert_eq!(args.source.digest.as_deref(), Some(recipe.digest.as_str()));
    assert_eq!(args.to, "ghcr.io/komira-ai/js/smoke:1");
    assert_eq!(args.user.as_deref(), Some("u"));
    assert_eq!(args.password_file, Some(PathBuf::from("/dev/stdin")));
    assert!(!args.plain_http);
    assert_eq!(
        printed_line(Ok(()), b"pushed x\n").expect("line"),
        "pushed x"
    );
    let failed = printed_line(Err(js_bundle_cli::Error::Target("t".to_owned())), b"");
    assert_eq!(
        failed.expect_err("failed").to_string(),
        "js-bundle: --to \"t\" is not REGISTRY/REPOSITORY:TAG"
    );
}

/// Proves the check of a published bundle: the committed tree packed, published to a registry
/// in memory and pulled back by digest through it is the tree, entry for entry and byte for
/// byte, and the scratch directory is gone afterwards. Catches a check that passes on a
/// bundle it did not compare, or leaves the extracted tree behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_published_bundle_pulled_by_digest_is_the_committed_tree() {
    let dir = scratch("smoke-published");
    let layout = packed(&dir).await;
    let digest = Digest::parse(&recipe().digest).expect("digest");
    let fx = Fixture::in_memory();
    fx.behave(|b| b.token = Some("tok".to_owned()));
    let client = fx.client();
    let repository = client.repository(fx.location(REPO)).expect("repository");
    let tag = Tag::parse(&recipe().tag).expect("tag");
    let published = repository
        .publish(&layout, &digest, &Kind::bundle(), &tag)
        .await
        .expect("publish");
    assert_eq!(published, Published::Pushed);
    let extracted = dir.join("extracted");
    let entries = compare(&repository, &digest, &tree(), &extracted)
        .await
        .expect("the same");
    assert_eq!(entries, 2);
    assert!(!extracted.exists());
    assert_eq!(
        smoke_bundle::checked_line(&recipe(), entries),
        format!(
            "checked {}: 2 entries, each the tree's",
            recipe().reference()
        )
    );
}

/// Proves a pulled bundle that is not the tree is refused at the first difference in path
/// order, saying what each side has there: changed bytes, an entry only the bundle has, and
/// one only the tree has. Catches a comparison of names alone, or of one side's entries only.
#[tokio::test(flavor = "multi_thread")]
async fn a_pulled_bundle_that_is_not_the_tree_names_the_first_difference() {
    let dir = scratch("smoke-differs");
    let layout = packed(&dir).await;
    let digest = Digest::parse(&recipe().digest).expect("digest");
    let committed = fs::read(tree().join("bin/smoke")).expect("program");
    let sum = |bytes: &[u8]| {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(bytes))
    };

    let other = dir.join("other");
    copy_tree(&other);
    fs::write(other.join("bin/smoke"), b"#!/bin/sh\n").expect("write");
    let refused = compare(&layout, &digest, &other, &dir.join("x1"))
        .await
        .expect_err("differs");
    assert_eq!(
        refused.to_string(),
        format!(
            "bin/smoke: the pulled bundle has an executable file of {} bytes, sha256 {}, the tree has an executable file of 10 bytes, sha256 {}",
            committed.len(),
            sum(&committed),
            sum(b"#!/bin/sh\n")
        )
    );

    let more = dir.join("more");
    copy_tree(&more);
    fs::write(more.join("bin/extra"), b"x").expect("write");
    let refused = compare(&layout, &digest, &more, &dir.join("x2"))
        .await
        .expect_err("differs");
    assert_eq!(
        refused.to_string(),
        format!(
            "bin/extra: the pulled bundle has nothing, the tree has a file of 1 bytes, sha256 {}",
            sum(b"x")
        )
    );

    let less = dir.join("less");
    fs::create_dir_all(less.join("bin")).expect("bin");
    symlink("elsewhere", less.join("bin/link")).expect("link");
    fs::create_dir(less.join("bin/smoke")).expect("dir");
    let refused = compare(&layout, &digest, &less, &dir.join("x3"))
        .await
        .expect_err("differs");
    assert_eq!(
        refused.to_string(),
        "bin/link: the pulled bundle has nothing, the tree has a link to \"elsewhere\""
    );
    fs::remove_file(less.join("bin/link")).expect("unlink");
    let refused = compare(&layout, &digest, &less, &dir.join("x4"))
        .await
        .expect_err("differs");
    assert_eq!(
        refused.to_string(),
        format!(
            "bin/smoke: the pulled bundle has an executable file of {} bytes, sha256 {}, the tree has a directory",
            committed.len(),
            sum(&committed)
        )
    );
}

/// Proves the check holds the bundle to the recipe's shape and the tree to the format's
/// kinds: a bundle of two layers is refused, a tree that holds a fifo is refused, and a
/// scratch directory that exists already is refused (it is never written into). Catches a
/// check that compares one layer of several, or extracts over files it did not make.
#[tokio::test(flavor = "multi_thread")]
async fn bundles_trees_and_scratch_directories_the_check_does_not_take_are_refused() {
    let dir = scratch("smoke-refused");
    let one = dir.join("one");
    let two = dir.join("two");
    copy_tree(&one);
    copy_tree(&two);
    fs::write(two.join("bin/smoke"), b"#!/bin/sh\necho two\n").expect("write");
    let out = dir.join("two-layers");
    let args: Vec<String> = [
        "pack",
        "--out",
        &out.display().to_string(),
        "--name",
        "two",
        "--bundle-version",
        "1",
        "--os-version",
        "11.0",
        "--tree",
        &format!("one={}", one.display()),
        "--tree",
        &format!("two={}", two.display()),
        "--license",
        "one=MIT",
        "--license",
        "two=MIT",
        "--source-url",
        "one=https://example.invalid/one",
        "--source-url",
        "two=https://example.invalid/two",
    ]
    .map(str::to_owned)
    .to_vec();
    let mut printed = Vec::new();
    let command = <js_bundle_cli::Cli as clap::Parser>::try_parse_from(
        std::iter::once("js-bundle".to_owned()).chain(args),
    )
    .expect("flags")
    .command;
    tokio::task::block_in_place(|| js_bundle_cli::execute(command, &mut printed)).expect("pack");
    let two_digest = Digest::parse(String::from_utf8_lossy(&printed).trim()).expect("digest");
    let layout = Layout::open(&out).await.expect("layout");
    let refused = compare(&layout, &two_digest, &one, &dir.join("x1"))
        .await
        .expect_err("two layers");
    assert_eq!(
        refused.to_string(),
        "the bundle has 2 layers and its config 2; the smoke bundle has one"
    );

    let fifo = dir.join("fifo-tree");
    copy_tree(&fifo);
    rustix::fs::mknodat(
        rustix::fs::CWD,
        fifo.join("bin/pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .expect("fifo");
    let layout = packed(&dir).await;
    let digest = Digest::parse(&recipe().digest).expect("digest");
    let refused = compare(&layout, &digest, &fifo, &dir.join("x2"))
        .await
        .expect_err("fifo");
    assert_eq!(
        refused.to_string(),
        format!(
            "{}: neither a directory, a regular file nor a symbolic link",
            fifo.join("bin/pipe").display()
        )
    );

    let taken = dir.join("taken");
    fs::create_dir(&taken).expect("dir");
    fs::write(taken.join("keep"), b"mine").expect("write");
    let refused = compare(&layout, &digest, &tree(), &taken)
        .await
        .expect_err("exists");
    assert!(
        refused
            .to_string()
            .starts_with(&taken.display().to_string()),
        "{refused}"
    );
    assert_eq!(fs::read(taken.join("keep")).expect("kept"), b"mine");
}

/// Runs `cargo xtask release smoke-bundle` with `args`: the exit code, stdout and stderr.
fn xtask(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(XTASK)
        .args(["release", "smoke-bundle"])
        .args(args)
        .output()
        .expect("xtask");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Proves the three verbs as the release runs them: `pack` of the committed tree prints the
/// reference by digest and exits 0; `check` and `push` given what is not there fail with the
/// reason on stderr and exit 1, before anything reaches a registry (no network here). Catches
/// a verb that is not wired to its command, or a failure that exits 0.
#[test]
fn the_verbs_run_from_the_command_line() {
    let dir = scratch("smoke-cli");
    let out = dir.join("layout");
    let tree = tree();
    let (code, stdout, stderr) = xtask(&[
        "pack",
        "--tree",
        &tree.display().to_string(),
        "--out",
        &out.display().to_string(),
    ]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, format!("packed {}\n", recipe().reference()));

    let absent = dir.join("absent");
    let (code, stdout, stderr) = xtask(&[
        "check",
        "--tree",
        &absent.display().to_string(),
        "--scratch",
        &dir.join("scratch").display().to_string(),
    ]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        format!(
            "xtask: {}: No such file or directory (os error 2)\n",
            absent.display()
        )
    );

    let password = dir.join("password");
    fs::write(&password, "token\n").expect("password");
    let (code, _, stderr) = xtask(&[
        "push",
        "--layout",
        &absent.display().to_string(),
        "--user",
        "u",
        "--password-file",
        &password.display().to_string(),
    ]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.starts_with("xtask: js-bundle: image layout: "),
        "{stderr}"
    );
}

/// Proves the operator's runbook names what the recipe publishes: the reference by digest,
/// the `--image-allow` entry of the repository, and the allow-list line of the digest. Catches
/// a new digest or tag that leaves the runbook naming the old one.
#[test]
fn the_runbook_names_the_published_reference_and_the_agent_lines() {
    let recipe = recipe();
    let runbook =
        fs::read_to_string(root().join("docs/runbooks/mac-smoke-bundle.md")).expect("runbook");
    for line in [
        recipe.reference(),
        format!("--image-allow={}", recipe.repository),
        recipe.digest.clone(),
    ] {
        assert!(
            runbook.lines().any(|l| l.trim() == line),
            "the runbook has no line {line:?}"
        );
    }
    assert!(
        runbook.contains(&format!("--spec.image {} ", recipe.reference())),
        "the runbook's job names another image"
    );
}
