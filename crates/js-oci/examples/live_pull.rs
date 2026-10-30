//! A read-only pull probe against a live registry, with js-oci alone.
//!
//! ```text
//! live_pull <registry>/<repository>@sha256:<manifest digest> [--allow-host HOST]...
//!           [--max-layer-bytes N] [--arch x86_64|aarch64]
//! ```
//!
//! It fetches the manifest by digest, the config and the smallest layer, anonymously,
//! checks each against its digest, and prints one `key=value` line per step. The layer
//! goes into a sink that hashes and counts; nothing is written to disk. The digest must
//! name one platform's image manifest: js-oci refuses an image index.
//!
//! `--allow-host HOST` also accepts HOST as a token endpoint (the registry's own host
//! always is). It is not a gate on blob hosts: a redirect target needs no entry, js-oci
//! follows redirects to https hosts on port 443 whose addresses are public. The probe
//! reports neither the token realm nor the redirect host (js-oci exposes neither); a
//! run that succeeds is the evidence that both were accepted. Exit status 1 and the js-oci error on
//! stderr when anything fails. The steps are in `live_pull/probe.rs`.

#[path = "live_pull/probe.rs"]
mod probe;

use std::process::ExitCode;

use js_oci::{Allow, Client, Digest, Kind, Location, Platform, Policy};

fn run(args: &[String]) -> Result<Vec<String>, String> {
    let mut reference = None;
    let (mut hosts, mut max, mut arch) =
        (Vec::new(), 64u64 << 20, std::env::consts::ARCH.to_owned());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--allow-host" => hosts.push(value()?),
            "--max-layer-bytes" => max = value()?.parse().map_err(|e| format!("{arg}: {e}"))?,
            "--arch" => arch = value()?,
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => reference = Some(other.to_owned()),
        }
    }
    let reference =
        reference.ok_or("usage: live_pull <registry>/<repository>@sha256:<digest> [flags]")?;
    let (name, digest) = reference
        .split_once('@')
        .ok_or("the reference needs @sha256:<digest>")?;
    let location = Location::parse(name).map_err(|e| e.to_string())?;
    let pin = Digest::parse(digest).map_err(|e| e.to_string())?;
    let kind = Kind::image(Platform::linux(&arch).map_err(|e| e.to_string())?);
    let entry = Allow::parse(name).map_err(|e| e.to_string())?;
    let mut policy = Policy::new(vec![entry]);
    for host in &hosts {
        policy = policy.allow_realm(host).map_err(|e| e.to_string())?;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(probe::probe(
            &Client::new(policy),
            location,
            &pin,
            &kind,
            max,
        ))
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(lines) => {
            lines.iter().for_each(|l| println!("{l}"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("live_pull: {e}");
            ExitCode::FAILURE
        }
    }
}
