//! `pack`: directories into a bundle in a new layout directory.
//!
//! Everything is packed and checked in memory first; the layout directory is made only
//! when the whole bundle is good, so a refusal leaves nothing behind.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use js_bundle::{
    Built, BundleConfig, DottedVersion, Entrypoint, EnvRule, Layer, Seatbelt, Template, TreeName,
    pack_layer,
};
use js_oci::Digest;

use crate::cli::{PackArgs, Pair};
use crate::error::Error;
use crate::layout::{self, Blob};

/// `pairs` as a map; a name given twice is refused.
fn unique(flag: &'static str, pairs: &[Pair]) -> Result<BTreeMap<String, String>, Error> {
    let mut map = BTreeMap::new();
    for pair in pairs {
        if map.insert(pair.key.clone(), pair.value.clone()).is_some() {
            return Err(Error::Duplicate {
                flag,
                key: pair.key.clone(),
            });
        }
    }
    Ok(map)
}

/// Refuses a flag that still has a value after every tree took its own: it names no tree.
fn none_left(flag: &'static str, left: BTreeMap<String, String>) -> Result<(), Error> {
    match left.into_keys().next() {
        Some(key) => Err(Error::NoSuchTree { flag, key }),
        None => Ok(()),
    }
}

/// The environment rules the `--env` and `--lock-env` flags describe: the values of one
/// variable and operation are one rule, in the order of the flags.
fn rules(args: &PackArgs) -> Result<Vec<EnvRule>, Error> {
    let mut rules: Vec<EnvRule> = Vec::new();
    for flag in &args.env {
        let value = Template::parse(&flag.value)?;
        let same =
            |rule: &&mut EnvRule| (rule.name.as_str(), rule.op) == (flag.name.as_str(), flag.op);
        match rules.iter_mut().find(same) {
            Some(rule) => rule.values.push(value),
            None => rules.push(EnvRule {
                name: flag.name.clone(),
                op: flag.op,
                values: vec![value],
                locked: false,
            }),
        }
    }
    for name in &args.locked_env {
        let rule = rules.iter_mut().find(|rule| rule.name == *name);
        rule.ok_or(Error::NoSuchEnv(name.clone()))?.locked = true;
    }
    Ok(rules)
}

pub(crate) fn pack(args: &PackArgs, out: &mut dyn Write) -> Result<(), Error> {
    let mut licenses = unique("--license", &args.licenses)?;
    let mut sources = unique("--source-url", &args.source_urls)?;
    let mut layers = Vec::new();
    let mut blobs = Vec::new();
    for tree in &args.trees {
        let name = TreeName::parse(&tree.key)?;
        let packed = pack_layer(Path::new(&tree.value)).map_err(|source| Error::Tree {
            tree: tree.key.clone(),
            source,
        })?;
        let missing = |flag| Error::Missing {
            flag,
            tree: tree.key.clone(),
        };
        layers.push(Layer {
            tree: name,
            diff_id: packed.diff_id,
            uncompressed_bytes: packed.uncompressed_bytes,
            entries: packed.entries,
            license: licenses.remove(&tree.key).ok_or(missing("--license"))?,
            source_url: sources.remove(&tree.key).ok_or(missing("--source-url"))?,
        });
        blobs.push(Blob {
            digest: Digest::parse(&packed.blob_digest)?,
            bytes: packed.blob,
        });
    }
    none_left("--license", licenses)?;
    none_left("--source-url", sources)?;
    let mut entrypoints = BTreeMap::new();
    for (name, path) in unique("--entrypoint", &args.entrypoints)? {
        let path = Template::parse(&path)?;
        entrypoints.insert(name, Entrypoint { path });
    }
    let os_version = DottedVersion::parse(&args.os_version)?;
    let config = BundleConfig {
        schema: 1,
        os: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        os_version,
        name: args.name.clone(),
        version: args.bundle_version.clone(),
        layers,
        env: rules(args)?,
        system_path: args.system_path,
        entrypoints,
        writable_trees: args
            .writable_trees
            .iter()
            .map(String::as_str)
            .map(TreeName::parse)
            .collect::<Result<_, _>>()?,
        seatbelt: Seatbelt {
            reads: args.reads.clone(),
            network_max: args.network,
        },
        xcode: None,
        built: Built {
            deployment_target: os_version,
            tool: format!("js-bundle {}", env!("CARGO_PKG_VERSION")),
        },
    };
    config.validate()?;
    let config = Blob::new(config.to_canonical_json());
    let manifest = layout::write(&args.out, &config, &blobs)?;
    writeln!(out, "{manifest}")?;
    Ok(())
}
