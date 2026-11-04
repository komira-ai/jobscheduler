//! The `in-process` feature stays out of every shipped binary: no default list names it and
//! no shipped crate's dependencies turn it on. Read from the manifests as they are in the
//! repository, with the shipped crates taken from the release plan's `--ship` list.

use std::fs;
use std::path::{Path, PathBuf};

/// The feature that must not ship.
const FEATURE: &str = "in-process";

/// The root of the workspace.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The crates whose binaries are shipped: the default values of `--ship` in the release
/// plan's arguments (`crates/xtask/src/release/plan.rs`).
fn shipped(plan: &str) -> Vec<String> {
    let start = plan
        .find("default_values_t = [")
        .expect("the plan lists the shipped crates");
    let list = &plan[start..];
    let list = &list[..list.find(']').expect("the list ends")];
    list.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// Every place in `manifest` (the manifest of the crate `name`) that turns the feature on in
/// a build that does not ask for it: a `default` list naming it (as itself in this crate, or
/// through `js-lease-server/`), and an entry for `js-lease-server` among the
/// `[dependencies]` or `[build-dependencies]`, for every target, that names it among its
/// features or is this crate itself. Dev-dependencies are not built into a binary.
fn turns_on(name: &str, manifest: &str) -> Vec<String> {
    let manifest: toml::Table = toml::from_str(manifest).unwrap();
    let mut found = Vec::new();
    let defaults = manifest
        .get("features")
        .and_then(|features| features.get("default"))
        .and_then(toml::Value::as_array);
    for value in defaults.into_iter().flatten() {
        let named = value.as_str().unwrap_or_default();
        let ours = name == "js-lease-server" && named == FEATURE;
        if ours || named == format!("js-lease-server/{FEATURE}") {
            found.push(format!("{name}: [features] default names {named}"));
        }
    }
    let targets = manifest.get("target").and_then(toml::Value::as_table);
    let tables = std::iter::once(&manifest).chain(
        targets
            .into_iter()
            .flat_map(|targets| targets.values().filter_map(toml::Value::as_table)),
    );
    for table in tables {
        for kind in ["dependencies", "build-dependencies"] {
            let dependency = table.get(kind).and_then(|deps| deps.get("js-lease-server"));
            let Some(dependency) = dependency else {
                continue;
            };
            let features = dependency.get("features").and_then(toml::Value::as_array);
            let names = features.into_iter().flatten();
            let named = names.filter_map(toml::Value::as_str).any(|f| f == FEATURE);
            if named || name == "js-lease-server" {
                found.push(format!(
                    "{name}: [{kind}] js-lease-server turns on {FEATURE}"
                ));
            }
        }
    }
    found
}

/// Asserts that no shipped crate's manifest (js-lease-server's own among them) turns the
/// feature on: no `default` list names it and no `[dependencies]` or `[build-dependencies]`
/// entry asks for it; and that the check finds each of those in a manifest that has one,
/// and passes this crate's own dev-dependency, which the tests need. It catches the seam
/// reaching a shipped binary through a default feature or a dependency of a shipped crate,
/// and a shipped list read as empty.
#[test]
fn the_in_process_feature_is_not_a_default_feature() {
    let root = root();
    let plan = fs::read_to_string(root.join("crates/xtask/src/release/plan.rs")).unwrap();
    let crates = shipped(&plan);
    assert!(
        crates.iter().any(|name| name == "js-lease-server"),
        "{crates:?}"
    );
    assert!(crates.iter().any(|name| name == "js-agent"), "{crates:?}");
    for name in &crates {
        let path = root.join("crates").join(name).join("Cargo.toml");
        let manifest = fs::read_to_string(&path).unwrap();
        assert_eq!(turns_on(name, &manifest), Vec::<String>::new(), "{name}");
    }

    let ours = "[features]\ndefault = [\"in-process\"]\nin-process = []\n";
    assert_eq!(turns_on("js-lease-server", ours).len(), 1);
    let through = "[features]\ndefault = [\"js-lease-server/in-process\"]\n";
    assert_eq!(turns_on("js-cli", through).len(), 1);
    let depends = "[dependencies]\njs-lease-server = { path = \"../js-lease-server\", features = [\"in-process\"] }\n";
    assert_eq!(turns_on("js-cli", depends).len(), 1);
    let on_linux = "[target.'cfg(target_os = \"linux\")'.build-dependencies]\n\
                    js-lease-server = { path = \"../js-lease-server\", features = [\"in-process\"] }\n";
    assert_eq!(turns_on("js-cli", on_linux).len(), 1);
    let itself = "[dependencies]\njs-lease-server = { path = \".\" }\n";
    assert_eq!(turns_on("js-lease-server", itself).len(), 1);
    let plain = "[dependencies]\njs-lease-server = { path = \"../js-lease-server\" }\n\
                 [dev-dependencies]\njs-lease-server = { path = \"../js-lease-server\", features = [\"in-process\"] }\n";
    assert_eq!(turns_on("js-cli", plain), Vec::<String>::new());
    let other = "[features]\ndefault = [\"in-process\"]\n";
    assert_eq!(turns_on("js-cli", other), Vec::<String>::new());
}
