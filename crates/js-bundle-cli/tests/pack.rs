//! `pack`: what it writes, that it writes the same bytes for the same input, and what it
//! refuses.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;

use common::{pack_args, packed, run, sample_tree, strings, text, work};

#[test]
fn pack_prints_the_manifest_digest_of_a_layout_it_made() {
    let work = work("pack-prints");
    let (layout, digest) = packed(&work, "out", "a");
    assert_eq!(fs::read_dir(&work).unwrap().count(), 2, "{work:?}");
    assert_eq!(digest.len(), "sha256:".len() + 64);
    assert!(digest.starts_with("sha256:"));
    assert_eq!(common::manifest_digest(&layout), digest);
    assert_eq!(
        fs::read_to_string(layout.join("oci-layout")).unwrap(),
        r#"{"imageLayoutVersion":"1.0.0"}"#
    );
    let manifest = common::manifest(&layout);
    assert_eq!(
        manifest["artifactType"],
        "application/vnd.komira.js.bundle.v1"
    );
    assert_eq!(
        manifest["layers"][0]["mediaType"],
        "application/vnd.komira.js.bundle.layer.v1.tar+zstd"
    );
}

#[test]
fn the_same_input_twice_gives_identical_digests_and_bytes() {
    let work = work("pack-twice");
    let (first, first_digest) = packed(&work, "one", "same");
    let (second, second_digest) = packed(&work, "two", "same");
    assert_eq!(first_digest, second_digest);
    assert_eq!(
        fs::read(first.join("index.json")).unwrap(),
        fs::read(second.join("index.json")).unwrap()
    );
    let layer = common::layer_digest(&first, 0);
    assert_eq!(layer, common::layer_digest(&second, 0));
    assert_eq!(
        fs::read(common::blob_path(&first, &layer)).unwrap(),
        fs::read(common::blob_path(&second, &layer)).unwrap()
    );
}

#[test]
fn different_content_gives_a_different_digest() {
    let work = work("pack-differs");
    let (_, one) = packed(&work, "one", "a");
    let (_, two) = packed(&work, "two", "b");
    assert_ne!(one, two);
}

#[test]
fn modes_other_than_the_execute_bit_do_not_change_the_digest() {
    let work = work("pack-modes");
    let (_, plain) = packed(&work, "one", "a");
    let tree = work.join("two-tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    fs::set_permissions(tree.join("lib/data.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(tree.join("bin/tool"), fs::Permissions::from_mode(0o700)).unwrap();
    let out = work.join("two");
    let second = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(second.digest(), plain);
}

#[test]
fn trees_become_layers_in_the_order_of_the_flags() {
    let work = work("pack-order");
    let (one, two) = (work.join("t1"), work.join("t2"));
    fs::create_dir(&one).unwrap();
    fs::create_dir(&two).unwrap();
    sample_tree(&one, "one");
    sample_tree(&two, "two");
    let forward = run(&pack_args(
        &work.join("f"),
        &[("alpha", &one), ("beta", &two)],
    ));
    let backward = run(&pack_args(
        &work.join("b"),
        &[("beta", &two), ("alpha", &one)],
    ));
    assert_eq!(forward.code, Some(0), "{}", forward.stderr);
    assert_ne!(forward.digest(), backward.digest());
    let config = common::config(&work.join("f"));
    assert_eq!(config["layers"][0]["tree"], "alpha");
    assert_eq!(config["layers"][1]["tree"], "beta");
}

fn refuses(build: impl FnOnce(&Path), reason: &str) {
    let work = work("pack-refuses");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    build(&tree);
    let out = work.join("out");
    let run = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains(reason), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(!out.exists(), "a refused pack left a layout");
}

#[test]
fn a_name_with_a_space_is_refused_with_the_rule() {
    refuses(
        |tree| fs::write(tree.join("a b"), "x").unwrap(),
        "has a byte outside 0x21..=0x7e",
    );
}

#[test]
fn an_appledouble_name_is_refused() {
    refuses(
        |tree| fs::write(tree.join("._hidden"), "x").unwrap(),
        "starts with `._`",
    );
}

#[test]
fn a_finder_leftover_is_refused() {
    refuses(
        |tree| fs::write(tree.join(".DS_Store"), "x").unwrap(),
        "Finder leftover",
    );
}

/// Two names that differ only in case are refused. A volume that folds case (the default
/// on a Mac) cannot hold both names, so there the second write replaces the first and the
/// tree is an ordinary one: the expected result follows from what the directory holds.
#[test]
fn names_that_differ_only_in_case_are_refused_where_a_volume_can_hold_both() {
    let work = work("pack-case");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("A"), "x").unwrap();
    fs::write(tree.join("a"), "y").unwrap();
    let folded = usize::from(fs::read_dir(&tree).unwrap().count() == 1);
    let run = run(&pack_args(&work.join("out"), &[("main", &tree)]));
    assert_eq!(run.code, [Some(1), Some(0)][folded], "{}", run.stderr);
    assert!(
        run.stderr.contains(["differs only in case", ""][folded]),
        "{}",
        run.stderr
    );
}

#[test]
fn a_symlinked_tree_directory_is_refused() {
    let work = work("pack-rootlink");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let link = work.join("link");
    symlink(&tree, &link).unwrap();
    let out = work.join("out");
    let run = run(&pack_args(&out, &[("main", &link)]));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains("is a symlink"), "{}", run.stderr);
    assert!(!out.exists());
}

#[test]
fn a_set_id_file_is_refused() {
    refuses(
        |tree| {
            fs::write(tree.join("su"), "x").unwrap();
            fs::set_permissions(tree.join("su"), fs::Permissions::from_mode(0o4755)).unwrap();
        },
        "sets a set-id or sticky bit",
    );
}

#[test]
fn a_hard_linked_file_is_refused() {
    refuses(
        |tree| {
            fs::write(tree.join("one"), "x").unwrap();
            fs::hard_link(tree.join("one"), tree.join("two")).unwrap();
        },
        "hard links",
    );
}

#[test]
fn a_symlink_that_leaves_the_tree_is_refused() {
    refuses(
        |tree| symlink("../outside", tree.join("up")).unwrap(),
        "resolves above the tree root",
    );
}

#[test]
fn an_absolute_symlink_is_refused() {
    refuses(
        |tree| symlink("/etc/passwd", tree.join("abs")).unwrap(),
        "is absolute",
    );
}

#[test]
fn a_directory_that_does_not_exist_is_refused() {
    let work = work("pack-nodir");
    let out = work.join("out");
    let run = run(&pack_args(&out, &[("main", &work.join("missing"))]));
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains("missing"), "{}", run.stderr);
    assert!(!out.exists());
}

#[test]
fn an_existing_out_directory_is_not_written_to() {
    let work = work("pack-exists");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let out = work.join("out");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("keep"), "mine").unwrap();
    let run = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains(&text(&out)), "{}", run.stderr);
    assert_eq!(fs::read_to_string(out.join("keep")).unwrap(), "mine");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
}

fn pack_with(extra: &[&str]) -> common::Run {
    let work = work("pack-flags");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let mut args = pack_args(&work.join("out"), &[("main", &tree)]);
    args.extend(strings(extra));
    run(&args)
}

#[test]
fn a_flag_that_names_a_tree_twice_is_refused() {
    let run = pack_with(&["--license", "main=Apache-2.0"]);
    assert_eq!(run.code, Some(1));
    assert!(
        run.stderr.contains("--license names \"main\" twice"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_licence_or_source_url_for_no_tree_is_refused() {
    let licence = pack_with(&["--license", "ghost=MIT"]);
    assert_eq!(licence.code, Some(1));
    assert!(
        licence
            .stderr
            .contains("--license names \"ghost\", which is no --tree"),
        "{}",
        licence.stderr
    );
    let source = pack_with(&["--source-url", "ghost=https://example.invalid/g"]);
    assert_eq!(source.code, Some(1));
    assert!(
        source
            .stderr
            .contains("--source-url names \"ghost\", which is no --tree"),
        "{}",
        source.stderr
    );
}

#[test]
fn a_failure_while_writing_leaves_no_out_directory_and_no_staging_directory() {
    // The out name fits a file name; the staging name beside it does not.
    let work = work("pack-partial");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let out = work.join("o".repeat(250));
    let run = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(!out.exists(), "a failed pack left its out directory");
    assert_eq!(
        fs::read_dir(&work).unwrap().count(),
        1,
        "only the tree is left"
    );
}

#[test]
fn a_tree_without_a_licence_is_refused() {
    let work = work("pack-nolicense");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let run = run(&strings(&[
        "pack",
        "--out",
        &text(&work.join("out")),
        "--name",
        "sample",
        "--bundle-version",
        "1.0.0",
        "--os-version",
        "14.0",
        "--tree",
        &format!("main={}", text(&tree)),
        "--source-url",
        "main=https://example.invalid/main",
    ]));
    assert_eq!(run.code, Some(1));
    assert!(
        run.stderr.contains("tree \"main\" has no --license"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_tree_without_a_source_url_is_refused() {
    let work = work("pack-nosource");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let run = run(&strings(&[
        "pack",
        "--out",
        &text(&work.join("out")),
        "--name",
        "sample",
        "--bundle-version",
        "1.0.0",
        "--os-version",
        "14.0",
        "--tree",
        &format!("main={}", text(&tree)),
        "--license",
        "main=MIT",
    ]));
    assert_eq!(run.code, Some(1));
    assert!(
        run.stderr.contains("tree \"main\" has no --source-url"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_bundle_with_no_tree_is_refused() {
    let work = work("pack-notree");
    let run = run(&strings(&[
        "pack",
        "--out",
        &text(&work.join("out")),
        "--name",
        "sample",
        "--bundle-version",
        "1.0.0",
        "--os-version",
        "14.0",
    ]));
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains("layers"), "{}", run.stderr);
}

#[test]
fn a_malformed_field_is_refused_with_its_name() {
    let work = work("pack-badtree");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    let run = run(&pack_args(&work.join("out"), &[("Main", &tree)]));
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains("invalid tree name"), "{}", run.stderr);
}

#[test]
fn a_config_the_format_refuses_is_refused() {
    let loader = pack_with(&["--env", "set:DYLD_LIBRARY_PATH=/x"]);
    assert_eq!(loader.code, Some(1));
    assert!(loader.stderr.contains("env[0].name"), "{}", loader.stderr);
    let undeclared = pack_with(&["--writable-tree", "nowhere"]);
    assert_eq!(undeclared.code, Some(1));
    assert!(
        undeclared.stderr.contains("nowhere"),
        "{}",
        undeclared.stderr
    );
    let entry = pack_with(&["--entrypoint", "tool=${tree:other}/bin/tool"]);
    assert_eq!(entry.code, Some(1));
    assert!(entry.stderr.contains("other"), "{}", entry.stderr);
}

#[test]
fn a_malformed_placeholder_is_refused() {
    let run = pack_with(&["--entrypoint", "tool=${nothing}/bin/tool"]);
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.starts_with("js-bundle: "), "{}", run.stderr);
}

#[test]
fn locking_a_variable_no_rule_sets_is_refused() {
    let run = pack_with(&["--lock-env", "HOME"]);
    assert_eq!(run.code, Some(1));
    assert!(
        run.stderr.contains("--lock-env \"HOME\" names no variable"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_flag_with_a_value_outside_its_vocabulary_is_a_usage_error() {
    for (flag, reason) in [
        (&["--env", "frob:X=1"][..], "unknown variant"),
        (&["--env", "nothing"][..], "expected OP:NAME=VALUE"),
        (&["--env", "set"][..], "expected OP:NAME=VALUE"),
        (&["--read", "disk"][..], "unknown variant"),
        (&["--network", "wide"][..], "unknown variant"),
        (&["--entrypoint", "tool"][..], "expected NAME=VALUE"),
    ] {
        let run = pack_with(flag);
        assert_eq!(run.code, Some(2), "{flag:?}: {}", run.stderr);
        assert!(run.stderr.contains(reason), "{flag:?}: {}", run.stderr);
    }
}

#[test]
fn a_staging_directory_that_already_exists_is_left_alone() {
    let work = work("pack-staging");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let staging = work.join(".out.partial");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("keep"), "mine").unwrap();
    let out = work.join("out");
    let run = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains(".out.partial"), "{}", run.stderr);
    assert_eq!(fs::read_to_string(staging.join("keep")).unwrap(), "mine");
    assert!(!out.exists(), "a refused pack left its out directory");
}

/// The longest path the system takes.
const PATH_MAX: usize = if cfg!(target_os = "macos") {
    1024
} else {
    4096
};

#[test]
fn a_failure_after_the_staging_directory_was_made_removes_both_directories() {
    // The parent is so deep that the staging directory fits and the blobs below it do not.
    let work = work("pack-deep");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let target = PATH_MAX - 85;
    let mut parent = work.path().to_path_buf();
    while parent.as_os_str().len() < target {
        let room = target - parent.as_os_str().len() - 1;
        parent.push("d".repeat(room.min(200)));
        fs::create_dir(&parent).unwrap();
    }
    let out = parent.join("out");
    let run = run(&pack_args(&out, &[("main", &tree)]));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(!out.exists(), "a failed pack left its out directory");
    assert!(
        !parent.join(".out.partial").exists(),
        "a failed pack left its staging directory"
    );
}

#[test]
fn a_symlinked_tree_directory_is_refused_with_a_trailing_slash_too() {
    let work = work("pack-rootlink-slash");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let link = work.join("link");
    symlink(&tree, &link).unwrap();
    let out = work.join("out");
    let run = run(&pack_args(
        &out,
        &[("main", &Path::new(&format!("{}/", text(&link))))],
    ));
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains("is a symlink"), "{}", run.stderr);
    assert!(!out.exists());
}
