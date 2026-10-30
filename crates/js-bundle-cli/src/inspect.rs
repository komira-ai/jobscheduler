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
    let blob = manifest.config();
    let reads: Vec<&str> = config.seatbelt.reads.iter().map(|r| r.as_str()).collect();
    let writable: Vec<&str> = config.writable_trees.iter().map(|t| t.as_str()).collect();
    let mut lines = vec![
        format!("manifest {}", manifest.digest()),
        format!(
            "artifact-type {}",
            manifest.artifact_type().unwrap_or_default()
        ),
        format!(
            "config {} {} {} bytes",
            blob.digest(),
            blob.media_type(),
            blob.size()
        ),
        format!(
            "bundle {} {} {}/{} macos>={}",
            config.name,
            config.version,
            config.os,
            config.architecture,
            String::from(config.os_version)
        ),
        format!("system-path {}", config.system_path),
        format!("network {}", word(config.seatbelt.network_max)),
        format!("reads {}", reads.join(" ")),
        format!("writable-trees {}", writable.join(" ")),
    ];
    for (index, (blob, layer)) in manifest.layers().iter().zip(&config.layers).enumerate() {
        lines.push(format!(
            "layer {index} {} {} {} bytes {}",
            layer.tree.as_str(),
            blob.digest(),
            blob.size(),
            blob.media_type()
        ));
        lines.push(format!("  diff-id {}", layer.diff_id.as_str()));
        lines.push(format!("  entries {}", layer.entries));
        lines.push(format!("  uncompressed-bytes {}", layer.uncompressed_bytes));
        lines.push(format!("  license {}", layer.license));
        lines.push(format!("  source-url {}", layer.source_url));
    }
    for (name, entry) in &config.entrypoints {
        lines.push(format!("entrypoint {name} {}", entry.path.as_str()));
    }
    for rule in &config.env {
        let values: Vec<&str> = rule.values.iter().map(|v| v.as_str()).collect();
        lines.push(format!(
            "env {} {} {} locked={}",
            word(rule.op),
            rule.name,
            values.join(":"),
            rule.locked
        ));
    }
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}
