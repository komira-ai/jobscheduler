//! `inspect`: the manifest and the config of a bundle, as lines.
//!
//! The bundle is read as `verify` reads it up to the layers: the manifest against its
//! digest and kind, the config against its digest and the format's rules. The layers are
//! not read; their descriptors are printed.

use std::io::Write;

use crate::bundle::load;
use crate::cli::LayoutArgs;
use crate::error::Error;

fn word(value: impl std::fmt::Debug) -> String {
    format!("{value:?}").to_lowercase()
}

pub(crate) async fn inspect(args: &LayoutArgs, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = load(args).await?;
    let (manifest, config) = (&loaded.manifest, &loaded.config);
    writeln!(out, "manifest {}", manifest.digest())?;
    writeln!(
        out,
        "artifact-type {}",
        manifest.artifact_type().unwrap_or_default()
    )?;
    let blob = manifest.config();
    writeln!(
        out,
        "config {} {} {} bytes",
        blob.digest(),
        blob.media_type(),
        blob.size()
    )?;
    writeln!(
        out,
        "bundle {} {} {}/{} macos>={}",
        config.name,
        config.version,
        config.os,
        config.architecture,
        String::from(config.os_version)
    )?;
    writeln!(out, "system-path {}", config.system_path)?;
    writeln!(out, "network {}", word(config.seatbelt.network_max))?;
    let reads: Vec<&str> = config.seatbelt.reads.iter().map(|r| r.as_str()).collect();
    writeln!(out, "reads {}", reads.join(" "))?;
    let writable: Vec<&str> = config.writable_trees.iter().map(|t| t.as_str()).collect();
    writeln!(out, "writable-trees {}", writable.join(" "))?;
    for (index, (blob, layer)) in manifest.layers().iter().zip(&config.layers).enumerate() {
        writeln!(
            out,
            "layer {index} {} {} {} bytes {}",
            layer.tree.as_str(),
            blob.digest(),
            blob.size(),
            blob.media_type()
        )?;
        writeln!(out, "  diff-id {}", layer.diff_id.as_str())?;
        writeln!(out, "  entries {}", layer.entries)?;
        writeln!(out, "  uncompressed-bytes {}", layer.uncompressed_bytes)?;
        writeln!(out, "  license {}", layer.license)?;
        writeln!(out, "  source-url {}", layer.source_url)?;
    }
    for (name, entry) in &config.entrypoints {
        writeln!(out, "entrypoint {name} {}", entry.path.as_str())?;
    }
    for rule in &config.env {
        let values: Vec<&str> = rule.values.iter().map(|v| v.as_str()).collect();
        writeln!(
            out,
            "env {} {} {} locked={}",
            word(rule.op),
            rule.name,
            values.join(":"),
            rule.locked
        )?;
    }
    Ok(())
}
