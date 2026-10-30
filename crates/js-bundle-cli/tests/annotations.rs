//! `pack --annotation`: manifest annotations, sorted, in the manifest only.

mod common;

use std::fs;
use std::path::Path;

use common::{pack_args, run, sample_tree, strings, text, verify_args, work};

const SOURCE: &str = "org.opencontainers.image.source=https://example.invalid/repo";

fn pack_into(work: &Path, name: &str, annotations: &[&str]) -> (common::Run, std::path::PathBuf) {
    let tree = work.join("tree");
    if !tree.exists() {
        fs::create_dir(&tree).unwrap();
        sample_tree(&tree, "a");
    }
    let out = work.join(name);
    let mut args = pack_args(&out, &[("main", &tree)]);
    for annotation in annotations {
        args.extend(strings(&["--annotation", annotation]));
    }
    (run(&args), out)
}

#[test]
fn annotations_are_in_the_manifest_sorted_and_inspect_prints_them() {
    let work = work("annotate");
    let (packed, out) = pack_into(
        &work,
        "out",
        &["org.example.zeta=last", SOURCE, "org.example.alpha=first"],
    );
    assert_eq!(packed.code, Some(0), "{}", packed.stderr);
    let manifest = common::manifest(&out);
    assert_eq!(manifest["annotations"]["org.example.alpha"], "first");
    assert_eq!(
        manifest["annotations"]["org.opencontainers.image.source"],
        "https://example.invalid/repo"
    );
    let raw = String::from_utf8(
        fs::read(common::blob_path(&out, &common::manifest_digest(&out))).unwrap(),
    )
    .unwrap();
    let positions = [
        "org.example.alpha",
        "org.example.zeta",
        "org.opencontainers.image.source",
    ]
    .map(|key| raw.find(key).unwrap());
    assert!(positions.is_sorted(), "{raw}");
    let shown = run(&strings(&["inspect", &text(&out)]));
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    let lines: Vec<&str> = shown.stdout.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with("annotation "))
        .unwrap();
    assert_eq!(
        lines[at..at + 3],
        [
            "annotation org.example.alpha first",
            "annotation org.example.zeta last",
            "annotation org.opencontainers.image.source https://example.invalid/repo",
        ]
    );
    let verified = run(&verify_args(&out, &work.join("scratch")));
    assert_eq!(verified.code, Some(0), "{}", verified.stderr);
}

#[test]
fn the_same_annotations_give_the_same_digest_in_any_flag_order() {
    let work = work("annotate-same");
    let (one, _) = pack_into(&work, "one", &[SOURCE, "org.example.k=v"]);
    let (two, _) = pack_into(&work, "two", &["org.example.k=v", SOURCE]);
    assert_eq!(one.digest(), two.digest());
}

#[test]
fn a_different_annotation_gives_a_different_digest_and_the_same_config_and_layers() {
    let work = work("annotate-differ");
    let (none, none_out) = pack_into(&work, "none", &[]);
    let (one, one_out) = pack_into(&work, "one", &["org.example.k=v"]);
    let (other, _) = pack_into(&work, "other", &["org.example.k=w"]);
    assert_ne!(none.digest(), one.digest());
    assert_ne!(one.digest(), other.digest());
    assert!(common::manifest(&none_out).get("annotations").is_none());
    assert_eq!(
        common::manifest(&none_out)["config"],
        common::manifest(&one_out)["config"]
    );
    assert_eq!(
        common::manifest(&none_out)["layers"],
        common::manifest(&one_out)["layers"]
    );
}

#[test]
fn a_duplicate_or_malformed_annotation_is_refused_before_anything_is_written() {
    for (annotations, reason) in [
        (
            &["org.example.k=v", "org.example.k=w"][..],
            "names \"org.example.k\" twice",
        ),
        (&["source=https://example.invalid"][..], "reverse-domain"),
        (&["Org.Example.k=v"][..], "reverse-domain"),
        (&["org..k=v"][..], "reverse-domain"),
        (&["org.example.k="][..], "non-empty value"),
    ] {
        let work = work("annotate-bad");
        let (packed, out) = pack_into(&work, "out", annotations);
        assert_eq!(packed.code, Some(1), "{annotations:?}: {}", packed.stderr);
        assert!(
            packed.stderr.contains(reason),
            "{annotations:?}: {}",
            packed.stderr
        );
        assert!(!out.exists(), "{annotations:?}");
    }
}
