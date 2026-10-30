//! `inspect`: prints what the manifest and the config say.

mod common;

use std::fs;

use common::{blob_path, config, manifest, pack_args, run, sample_tree, strings, text, work};

#[test]
fn inspect_prints_the_manifest_and_the_config() {
    let work = work("inspect");
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    sample_tree(&tree, "a");
    let layout = work.join("layout");
    let mut args = pack_args(&layout, &[("main", &tree)]);
    args.extend(strings(&[
        "--entrypoint",
        "tool=${tree:main}/bin/tool",
        "--env",
        "prepend:PATH=${tree:main}/bin",
        "--env",
        "prepend:PATH=${tree:main}/lib",
        "--env",
        "set:LANG=C",
        "--lock-env",
        "LANG",
        "--read",
        "system-certs",
        "--read",
        "timezone",
        "--network",
        "loopback",
        "--system-path",
        "--writable-tree",
        "main",
    ]));
    let packed = run(&args);
    assert_eq!(packed.code, Some(0), "{}", packed.stderr);

    let shown = run(&strings(&["inspect", &text(&layout)]));
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    let lines: Vec<&str> = shown.stdout.lines().collect();
    let manifest = manifest(&layout);
    let config = config(&layout);
    let config_blob = &manifest["config"];
    let layer = &manifest["layers"][0];
    let layer_digest = layer["digest"].as_str().unwrap();
    let expected = [
        format!("manifest {}", packed.digest()),
        "artifact-type application/vnd.komira.js.bundle.v1".to_owned(),
        format!(
            "config {} application/vnd.komira.js.bundle.config.v1+json {} bytes",
            config_blob["digest"].as_str().unwrap(),
            config_blob["size"]
        ),
        "bundle sample 1.0.0 darwin/arm64 macos>=14.0".to_owned(),
        "system-path true".to_owned(),
        "network loopback".to_owned(),
        "reads system-certs timezone".to_owned(),
        "writable-trees main".to_owned(),
        format!(
            "layer 0 main {layer_digest} {} bytes application/vnd.komira.js.bundle.layer.v1.tar+zstd",
            fs::metadata(blob_path(&layout, layer_digest))
                .unwrap()
                .len()
        ),
        format!(
            "  diff-id {}",
            config["layers"][0]["diff_id"].as_str().unwrap()
        ),
        "  entries 5".to_owned(),
        format!(
            "  uncompressed-bytes {}",
            config["layers"][0]["uncompressed_bytes"]
        ),
        "  license MIT".to_owned(),
        "  source-url https://example.invalid/main".to_owned(),
        "entrypoint tool ${tree:main}/bin/tool".to_owned(),
        "env prepend PATH ${tree:main}/bin:${tree:main}/lib locked=false".to_owned(),
        "env set LANG C locked=true".to_owned(),
    ];
    assert_eq!(lines, expected);
}

#[test]
fn inspect_refuses_what_verify_refuses_before_any_layer() {
    let work = work("inspect-bad");
    let (layout, digest) = common::packed(&work, "out", "a");
    let mut bytes = fs::read(blob_path(&layout, &digest)).unwrap();
    bytes.push(b' ');
    fs::write(blob_path(&layout, &digest), bytes).unwrap();
    let shown = run(&strings(&["inspect", &text(&layout)]));
    assert_eq!(shown.code, Some(1));
    assert!(shown.stderr.contains("not the pinned"), "{}", shown.stderr);
    assert_eq!(shown.stdout, "");
}

#[test]
fn inspect_names_a_manifest_by_digest() {
    let work = work("inspect-digest");
    let (layout, digest) = common::packed(&work, "out", "a");
    let shown = run(&strings(&["inspect", &text(&layout), "--digest", &digest]));
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert!(shown.stdout.starts_with(&format!("manifest {digest}\n")));
}
