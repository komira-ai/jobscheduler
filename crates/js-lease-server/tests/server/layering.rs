//! Which crates may know of which: the usage crate sits below the server and beside the
//! registry, and the scheduler's core knows nothing of it.

use std::fs;
use std::path::{Path, PathBuf};

fn crate_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(name)
}

/// The names a crate's manifest depends on, in every kind of dependency table.
fn depends_on(name: &str) -> Vec<String> {
    let text = fs::read_to_string(crate_dir(name).join("Cargo.toml")).unwrap();
    let manifest: toml::Table = toml::from_str(&text).unwrap();
    let tables = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut names = Vec::new();
    for table in tables {
        if let Some(deps) = manifest.get(table).and_then(toml::Value::as_table) {
            names.extend(deps.keys().cloned());
        }
    }
    names
}

/// The text of every Rust source file under a crate's `src`.
fn sources(name: &str) -> String {
    let mut text = String::new();
    let mut pending = vec![crate_dir(name).join("src")];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            match path.is_dir() {
                true => pending.push(path),
                false => text.push_str(&fs::read_to_string(&path).unwrap()),
            }
        }
    }
    text
}

/// Whether the sources of a crate name `module` as a path or import it.
fn names(name: &str, module: &str) -> bool {
    let text = sources(name);
    text.contains(&format!("{module}::")) | text.contains(&format!("use {module}"))
}

/// Asserts that the scheduler's core neither depends on the usage crate nor names it in
/// its sources, so admission can be built and simulated without a store. It catches a
/// convenience import that would pull the store (and its database) under the scheduler.
#[test]
fn the_scheduler_does_not_know_the_usage_crate() {
    assert!(!depends_on("js-sched").contains(&"js-usage".to_string()));
    assert!(!names("js-sched", "js_usage"));
}

/// Asserts that the usage crate depends on none of the wire, the registry, the scheduler
/// or the server: it takes plain values, and the server converts the wire into them. It
/// catches a conversion that moves into the usage crate by importing the wire types,
/// which would tie the store to the protocol's versions.
#[test]
fn the_usage_crate_takes_plain_values_and_knows_no_wire_or_server() {
    let deps = depends_on("js-usage");
    for (package, module) in [
        ("js-agent-wire", "js_agent_wire"),
        ("js-registry", "js_registry"),
        ("js-sched", "js_sched"),
        ("js-lease-server", "js_lease_server"),
    ] {
        assert!(!deps.contains(&package.to_string()), "{package}");
        assert!(!names("js-usage", module), "{module}");
    }
}
