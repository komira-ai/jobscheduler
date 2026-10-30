//! `verify`: a packed bundle passes; a bundle altered in any way the format or the digests
//! can see is refused with the reason.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{
    blob_path, config, layer_digest, manifest_digest, pack_args, packed, rewrite, run, sample_tree,
    strings, text, verify_args, work,
};

fn verify(layout: &Path, work: &Path) -> (common::Run, PathBuf) {
    let scratch = work.join("scratch");
    (run(&verify_args(layout, &scratch)), scratch)
}

fn refused(run: &common::Run, scratch: &Path, reason: &str) {
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains(reason), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(
        !scratch.exists(),
        "a refused verify left its scratch directory"
    );
}

#[test]
fn a_packed_bundle_of_two_trees_verifies() {
    let work = work("verify-ok");
    let (one, two) = (work.join("t1"), work.join("t2"));
    fs::create_dir(&one).unwrap();
    fs::create_dir(&two).unwrap();
    sample_tree(&one, "one");
    sample_tree(&two, "two");
    let layout = work.join("layout");
    let packed = run(&pack_args(&layout, &[("alpha", &one), ("beta", &two)]));
    assert_eq!(packed.code, Some(0), "{}", packed.stderr);
    let (verified, scratch) = verify(&layout, &work);
    assert_eq!(verified.code, Some(0), "{}", verified.stderr);
    assert_eq!(
        verified.stdout,
        format!("verified {} (2 layers)\n", packed.digest())
    );
    assert!(!scratch.exists(), "verify left its scratch directory");
}

#[test]
fn verify_takes_the_digest_pack_printed() {
    let work = work("verify-digest");
    let (layout, digest) = packed(&work, "out", "a");
    let mut args = verify_args(&layout, &work.join("scratch"));
    args.extend(strings(&["--digest", &digest]));
    let verified = run(&args);
    assert_eq!(verified.code, Some(0), "{}", verified.stderr);
}

#[test]
fn a_digest_the_layout_does_not_list_is_refused() {
    let work = work("verify-unlisted");
    let (layout, _) = packed(&work, "out", "a");
    let mut args = verify_args(&layout, &work.join("scratch"));
    args.extend(strings(&[
        "--digest",
        &format!("sha256:{}", "0".repeat(64)),
    ]));
    let verified = run(&args);
    assert_eq!(verified.code, Some(1));
    assert!(
        verified.stderr.contains("not listed in index.json"),
        "{}",
        verified.stderr
    );
}

#[test]
fn a_malformed_digest_is_refused() {
    let work = work("verify-baddigest");
    let (layout, _) = packed(&work, "out", "a");
    let mut args = verify_args(&layout, &work.join("scratch"));
    args.extend(strings(&["--digest", "nonsense"]));
    let verified = run(&args);
    assert_eq!(verified.code, Some(1));
    assert!(verified.stderr.contains("nonsense"), "{}", verified.stderr);
}

#[test]
fn a_layout_that_lists_two_manifests_needs_a_digest() {
    let work = work("verify-two");
    let (layout, digest) = packed(&work, "out", "a");
    let mut index = common::read_json(&layout.join("index.json"));
    let entry = index["manifests"][0].clone();
    index["manifests"].as_array_mut().unwrap().push(entry);
    fs::write(
        layout.join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    let (without, scratch) = verify(&layout, &work);
    refused(&without, &scratch, "lists 2 manifests");
    let mut args = verify_args(&layout, &scratch);
    args.extend(strings(&["--digest", &digest]));
    assert_eq!(run(&args).code, Some(0));
}

#[test]
fn a_directory_that_is_not_a_layout_is_refused() {
    let work = work("verify-notlayout");
    let empty = work.join("empty");
    fs::create_dir(&empty).unwrap();
    let (verified, scratch) = verify(&empty, &work);
    refused(&verified, &scratch, "oci-layout");
}

#[test]
fn a_layout_without_an_index_is_refused() {
    let work = work("verify-noindex");
    let (layout, _) = packed(&work, "out", "a");
    fs::remove_file(layout.join("index.json")).unwrap();
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "index.json");
}

#[test]
fn an_index_that_is_not_json_is_refused() {
    let work = work("verify-badindex");
    let (layout, _) = packed(&work, "out", "a");
    fs::write(layout.join("index.json"), "not json").unwrap();
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "JSON");
}

#[test]
fn an_existing_scratch_directory_is_refused_and_kept() {
    let work = work("verify-scratch");
    let (layout, _) = packed(&work, "out", "a");
    let scratch = work.join("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::write(scratch.join("keep"), "mine").unwrap();
    let verified = run(&verify_args(&layout, &scratch));
    assert_eq!(verified.code, Some(1));
    assert!(
        verified.stderr.contains(&text(&scratch)),
        "{}",
        verified.stderr
    );
    assert_eq!(fs::read_to_string(scratch.join("keep")).unwrap(), "mine");
}

fn damage_blob(layout: &Path, digest: &str, change: impl FnOnce(&mut Vec<u8>)) {
    let path = blob_path(layout, digest);
    let mut bytes = fs::read(&path).unwrap();
    change(&mut bytes);
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_layer_blob_with_a_changed_byte_is_refused() {
    let work = work("verify-flip-layer");
    let (layout, _) = packed(&work, "out", "a");
    damage_blob(&layout, &layer_digest(&layout, 0), |bytes| bytes[10] ^= 1);
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "hashes to");
}

#[test]
fn a_truncated_layer_blob_is_refused() {
    let work = work("verify-short-layer");
    let (layout, _) = packed(&work, "out", "a");
    damage_blob(&layout, &layer_digest(&layout, 0), |bytes| {
        bytes.pop();
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "the manifest says");
}

#[test]
fn a_missing_layer_blob_is_refused() {
    let work = work("verify-missing-layer");
    let (layout, _) = packed(&work, "out", "a");
    fs::remove_file(blob_path(&layout, &layer_digest(&layout, 0))).unwrap();
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "cannot be read");
}

#[test]
fn a_config_blob_with_a_changed_byte_is_refused() {
    let work = work("verify-flip-config");
    let (layout, _) = packed(&work, "out", "a");
    let digest = common::manifest(&layout)["config"]["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    damage_blob(&layout, &digest, |bytes| bytes[3] ^= 1);
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "hashes to");
}

#[test]
fn a_manifest_with_a_changed_byte_is_refused() {
    let work = work("verify-flip-manifest");
    let (layout, digest) = packed(&work, "out", "a");
    damage_blob(&layout, &digest, |bytes| bytes.push(b' '));
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "not the pinned");
}

#[test]
fn a_layer_of_a_generic_image_media_type_is_refused() {
    let work = work("verify-layer-type");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |manifest, _| {
        manifest["layers"][0]["mediaType"] = "application/vnd.oci.image.layer.v1.tar+gzip".into();
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "media type");
}

#[test]
fn a_config_of_a_generic_image_media_type_is_refused() {
    let work = work("verify-config-type");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |manifest, _| {
        manifest["config"]["mediaType"] = "application/vnd.oci.image.config.v1+json".into();
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "the config has media type");
}

#[test]
fn a_manifest_of_another_artifact_type_is_refused() {
    let work = work("verify-artifact-type");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |manifest, _| {
        manifest["artifactType"] = "application/vnd.example.other.v1".into();
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "artifact type");
}

#[test]
fn a_config_of_another_platform_is_refused() {
    let work = work("verify-platform");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |_, config| config["os"] = "linux".into());
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "linux");
}

#[test]
fn a_manifest_and_a_config_that_disagree_on_the_layers_are_refused() {
    let work = work("verify-count");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |manifest, _| {
        let layer = manifest["layers"][0].clone();
        manifest["layers"].as_array_mut().unwrap().push(layer);
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(
        &verified,
        &scratch,
        "the manifest has 2 layers and the config describes 1",
    );
}

#[test]
fn a_config_with_a_field_the_format_does_not_have_is_refused() {
    let work = work("verify-unknown-field");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |_, config| config["extra"] = 1.into());
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "unknown field");
}

#[test]
fn a_config_that_breaks_a_format_rule_is_refused() {
    let work = work("verify-rule");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |_, config| config["name"] = "Upper".into());
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "name");
}

#[test]
fn a_layer_that_is_not_the_tree_the_config_names_is_refused() {
    let work = work("verify-diff-id");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |_, config| {
        config["layers"][0]["diff_id"] = format!("sha256:{}", "0".repeat(64)).into();
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "layer 0 (tree \"main\")");
}

#[test]
fn a_layer_with_more_entries_than_the_config_allows_is_refused() {
    let work = work("verify-entries");
    let (layout, _) = packed(&work, "out", "a");
    rewrite(&layout, |_, config| {
        config["layers"][0]["entries"] = 1.into()
    });
    let (verified, scratch) = verify(&layout, &work);
    refused(&verified, &scratch, "layer 0 (tree \"main\")");
}

#[test]
fn the_manifest_digest_is_unchanged_by_verifying() {
    let work = work("verify-readonly");
    let (layout, digest) = packed(&work, "out", "a");
    let before = fs::read(layout.join("index.json")).unwrap();
    let (verified, _) = verify(&layout, &work);
    assert_eq!(verified.code, Some(0), "{}", verified.stderr);
    assert_eq!(fs::read(layout.join("index.json")).unwrap(), before);
    assert_eq!(manifest_digest(&layout), digest);
    assert!(config(&layout)["layers"].is_array());
}
