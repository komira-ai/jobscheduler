//! Shared helpers: the golden configs and a JSON editor for hostile variants.
#![allow(dead_code)]

use js_bundle::{BundleConfig, ConfigError};
use serde_json::Value;

pub const TWO_TREE: &str = include_str!("../fixtures/two_tree.canonical.json");
pub const TWO_TREE_REORDERED: &str = include_str!("../fixtures/two_tree.reordered.json");
pub const MINIMAL: &str = include_str!("../fixtures/minimal.canonical.json");

pub fn golden() -> BundleConfig {
    BundleConfig::parse(TWO_TREE.as_bytes()).unwrap()
}

pub fn minimal() -> BundleConfig {
    BundleConfig::parse(MINIMAL.as_bytes()).unwrap()
}

pub fn doc() -> Value {
    serde_json::from_str(TWO_TREE).unwrap()
}

/// Parses the golden document after an edit.
pub fn edited(edit: impl FnOnce(&mut Value)) -> Result<BundleConfig, ConfigError> {
    let mut value = doc();
    edit(&mut value);
    BundleConfig::parse(&serde_json::to_vec(&value).unwrap())
}

/// Replaces the value at a JSON pointer.
pub fn set(value: &mut Value, pointer: &str, new: Value) {
    *value.pointer_mut(pointer).unwrap() = new;
}

pub fn hex(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
