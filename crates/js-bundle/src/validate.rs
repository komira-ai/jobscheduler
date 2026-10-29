//! `BundleConfig::validate`: every rule of format section 1.3 that the types leave.

use std::collections::BTreeSet;

use crate::config::{BundleConfig, EnvOp, Layer};
use crate::env::check_env_name;
use crate::error::{ConfigError, Defect};
use crate::names::TreeName;
use crate::template::{Segment, Template};

/// The longest config document read, in bytes (format section 1.1).
pub const MAX_CONFIG_BYTES: usize = 1 << 20;
/// The most layers (and so trees) a bundle has.
pub const MAX_LAYERS: usize = 32;
/// The most environment rules a config has.
pub const MAX_ENV_RULES: usize = 64;
/// The most values one environment rule has.
pub const MAX_VALUES_PER_RULE: usize = 64;
/// The longest placeholder string (an environment value, an argument), in bytes.
pub const MAX_VALUE_BYTES: usize = 4096;
/// The most bytes one layer may declare, and the most the layers declare together:
/// 16 GiB, the same ceiling the registry client puts on a bundle.
pub const MAX_TOTAL_BYTES: u64 = 16 << 30;
/// The most entries one layer may declare.
pub const MAX_ENTRIES: u64 = 1 << 24;
/// The most entrypoints a config has.
const MAX_ENTRYPOINTS: usize = 64;

const SCHEMA: u32 = 1;
const MAX_NAME: usize = 64;
const MAX_LICENSE: usize = 256;
const MAX_URL: usize = 2048;
const MAX_TOOL: usize = 128;

fn bad(field: impl Into<String>, defect: impl Into<Defect>) -> ConfigError {
    ConfigError::Invalid {
        field: field.into(),
        defect: defect.into(),
    }
}

fn non_empty(field: &str, text: &str, max: usize) -> Result<(), ConfigError> {
    if text.is_empty() {
        return Err(bad(field, Defect::Empty));
    }
    if text.len() > max {
        return Err(bad(field, Defect::TooLong { max }));
    }
    Ok(())
}

fn exactly(field: &str, text: &str, want: &'static str) -> Result<(), ConfigError> {
    if text == want {
        Ok(())
    } else {
        Err(bad(field, Defect::Unsupported { want }))
    }
}

/// `[a-z0-9][a-z0-9._-]*`.
fn lower_ident_ok(text: &str) -> bool {
    let mut bytes = text.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'));
    first_ok && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// `[A-Za-z0-9][A-Za-z0-9._+-]*`.
fn mixed_ident_ok(text: &str) -> bool {
    let mut bytes = text.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'));
    first_ok
        && bytes.all(
            |b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'+' | b'-'),
        )
}

fn ident(
    field: &str,
    text: &str,
    ok: fn(&str) -> bool,
    want: &'static str,
) -> Result<(), ConfigError> {
    non_empty(field, text, MAX_NAME)?;
    if ok(text) {
        Ok(())
    } else {
        Err(bad(field, Defect::Malformed { want }))
    }
}

fn check_layer(index: usize, layer: &Layer) -> Result<(), ConfigError> {
    let at = |name: &str| format!("layers[{index}].{name}");
    if layer.uncompressed_bytes == 0 {
        return Err(bad(at("uncompressed_bytes"), Defect::Zero));
    }
    if layer.uncompressed_bytes > MAX_TOTAL_BYTES {
        let max = MAX_TOTAL_BYTES;
        return Err(bad(at("uncompressed_bytes"), Defect::OverLimit { max }));
    }
    if layer.entries == 0 {
        return Err(bad(at("entries"), Defect::Zero));
    }
    if layer.entries > MAX_ENTRIES {
        let max = MAX_ENTRIES;
        return Err(bad(at("entries"), Defect::OverLimit { max }));
    }
    non_empty(&at("license"), &layer.license, MAX_LICENSE)?;
    let spdx = layer.license.chars().all(
        |c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | ' ' | '.' | '-' | '+' | '(' | ')'),
    );
    if !spdx {
        return Err(bad(
            at("license"),
            Defect::Malformed {
                want: "an SPDX expression",
            },
        ));
    }
    non_empty(&at("source_url"), &layer.source_url, MAX_URL)?;
    let url = layer.source_url.starts_with("https://")
        && layer.source_url.chars().all(|c| c.is_ascii_graphic());
    if !url {
        return Err(bad(
            at("source_url"),
            Defect::Malformed {
                want: "an https URL without spaces or control characters",
            },
        ));
    }
    Ok(())
}

fn check_layers(config: &BundleConfig) -> Result<BTreeSet<&TreeName>, ConfigError> {
    non_empty_list("layers", config.layers.len(), MAX_LAYERS)?;
    let mut trees = BTreeSet::new();
    let mut diffs = BTreeSet::new();
    let mut total: u64 = 0;
    for (index, layer) in config.layers.iter().enumerate() {
        if !trees.insert(&layer.tree) {
            return Err(bad(format!("layers[{index}].tree"), Defect::Duplicate));
        }
        if !diffs.insert(&layer.diff_id) {
            return Err(bad(format!("layers[{index}].diff_id"), Defect::Duplicate));
        }
        check_layer(index, layer)?;
        total = total.saturating_add(layer.uncompressed_bytes);
        if total > MAX_TOTAL_BYTES {
            let max = MAX_TOTAL_BYTES;
            return Err(bad("layers", Defect::OverLimit { max }));
        }
    }
    Ok(trees)
}

fn non_empty_list(field: &str, len: usize, max: usize) -> Result<(), ConfigError> {
    if len == 0 {
        return Err(bad(field, Defect::Empty));
    }
    if len > max {
        return Err(bad(field, Defect::TooMany { max }));
    }
    Ok(())
}

fn declared(
    field: &str,
    template: &Template,
    trees: &BTreeSet<&TreeName>,
) -> Result<(), ConfigError> {
    match template.trees().find(|name| !trees.contains(name)) {
        Some(name) => Err(bad(field, Defect::Undeclared(name.as_str().to_string()))),
        None => Ok(()),
    }
}

fn check_env(config: &BundleConfig, trees: &BTreeSet<&TreeName>) -> Result<(), ConfigError> {
    if config.env.len() > MAX_ENV_RULES {
        return Err(bad("env", Defect::TooMany { max: MAX_ENV_RULES }));
    }
    let mut names = BTreeSet::new();
    for (index, rule) in config.env.iter().enumerate() {
        let at = format!("env[{index}]");
        check_env_name(&rule.name).map_err(|why| bad(format!("{at}.name"), why))?;
        if !names.insert(rule.name.as_str()) {
            return Err(bad(format!("{at}.name"), Defect::Duplicate));
        }
        let joined = matches!(rule.op, EnvOp::Prepend | EnvOp::Append);
        let arity_ok = if joined {
            (1..=MAX_VALUES_PER_RULE).contains(&rule.values.len())
        } else {
            rule.values.len() == 1
        };
        if !arity_ok {
            return Err(bad(format!("{at}.values"), Defect::Arity));
        }
        for (position, value) in rule.values.iter().enumerate() {
            let field = format!("{at}.values[{position}]");
            declared(&field, value, trees)?;
            let colon = value
                .segments()
                .iter()
                .any(|segment| matches!(segment, Segment::Lit(lit) if lit.contains(':')));
            if joined && colon {
                return Err(bad(
                    field,
                    Defect::Malformed {
                        want: "free of ':' (the values are joined with it)",
                    },
                ));
            }
        }
    }
    Ok(())
}

fn check_entrypoints(
    config: &BundleConfig,
    trees: &BTreeSet<&TreeName>,
) -> Result<(), ConfigError> {
    if config.entrypoints.len() > MAX_ENTRYPOINTS {
        return Err(bad(
            "entrypoints",
            Defect::TooMany {
                max: MAX_ENTRYPOINTS,
            },
        ));
    }
    for (name, entry) in &config.entrypoints {
        let field = format!("entrypoints.{name}");
        ident(
            &field,
            name,
            mixed_ident_ok,
            "a name of letters, digits and . _ - +",
        )?;
        let field = format!("{field}.path");
        match entry.path.segments() {
            [Segment::Tree(tree), Segment::Lit(_)] => {
                if !trees.contains(tree) {
                    return Err(bad(field, Defect::Undeclared(tree.as_str().to_string())));
                }
            }
            _ => {
                return Err(bad(
                    field,
                    Defect::Malformed {
                        want: "${tree:NAME}/path below a tree",
                    },
                ));
            }
        }
    }
    Ok(())
}

fn check_unique<T: Ord>(field: &str, items: impl Iterator<Item = T>) -> Result<(), ConfigError> {
    let mut seen = BTreeSet::new();
    for item in items {
        if !seen.insert(item) {
            return Err(bad(field, Defect::Duplicate));
        }
    }
    Ok(())
}

pub(crate) fn validate(config: &BundleConfig) -> Result<(), ConfigError> {
    if config.schema != SCHEMA {
        return Err(bad("schema", Defect::Unsupported { want: "1" }));
    }
    exactly("os", &config.os, "darwin")?;
    exactly("architecture", &config.architecture, "arm64")?;
    ident(
        "name",
        &config.name,
        lower_ident_ok,
        "lowercase letters, digits and . _ -",
    )?;
    ident(
        "version",
        &config.version,
        mixed_ident_ok,
        "letters, digits and . _ - +",
    )?;
    let trees = check_layers(config)?;
    check_env(config, &trees)?;
    check_entrypoints(config, &trees)?;
    for (index, tree) in config.writable_trees.iter().enumerate() {
        if !trees.contains(tree) {
            return Err(bad(
                format!("writable_trees[{index}]"),
                Defect::Undeclared(tree.as_str().to_string()),
            ));
        }
    }
    check_unique("writable_trees", config.writable_trees.iter())?;
    check_unique("seatbelt.reads", config.seatbelt.reads.iter())?;
    if let Some(need) = &config.xcode {
        check_unique("xcode.components", need.components.iter())?;
    }
    non_empty("built.tool", &config.built.tool, MAX_TOOL)?;
    if !config.built.tool.chars().all(|c| matches!(c, ' '..='~')) {
        return Err(bad(
            "built.tool",
            Defect::Malformed {
                want: "printable ASCII",
            },
        ));
    }
    Ok(())
}
