//! The checks of a submitted job's file system beyond its image (`docs/design/job-rootfs.md`,
//! section 2.5), for `validate`: its layers, mounts and host ports become the forms the job's
//! record keeps, each default in its explicit form (a mode, a propagation and a protocol left
//! unspecified are read-only, private and TCP; an absent job port is the host port; an empty
//! `subPath` is none), so a resubmit that spells a default either way is the same job. The
//! shape only is checked here: which host paths and ports a host allows is its agent's to
//! say, at the start.
//!
//! | field | rule | refused as |
//! |---|---|---|
//! | `spec.layers` | at most 16 | `field: spec.layers` |
//! | `spec.layers[i]` | exactly one source this server knows | `field: spec.layers[i]: no source this server knows` |
//! | `spec.layers[i].hostPath` | absolute, in normal form, at most 4096 bytes, no control character ([`path::absolute`]) | `field: spec.layers[i].hostPath` |
//! | `spec.layers[i].image` | `NAME[:TAG]@sha256:<64 hex>`, as `spec.image` | `field: spec.layers[i].image` |
//! | `spec.mounts` | at most 64 | `field: spec.mounts` |
//! | `spec.mounts[i]` | exactly one source this server knows | `field: spec.mounts[i]: no source this server knows` |
//! | `spec.mounts[i].hostPath` | as a layer's | `field: spec.mounts[i].hostPath` |
//! | `spec.mounts[i].target` | absolute, in normal form, not `/`, ASCII, no `:`, `,` or `\`, at most 4096 bytes, outside the runtime's places ([`path::target`]); no two equal in lower case | `field: spec.mounts[i].target` |
//! | `spec.mounts[i].mode` | a known value | `field: spec.mounts[i].mode` |
//! | `spec.mounts[i].subPath` | empty, or relative in normal form, at most 4096 bytes, no control character ([`path::relative`]) | `field: spec.mounts[i].subPath` |
//! | `spec.mounts[i].propagation` | a known value | `field: spec.mounts[i].propagation` |
//! | `spec.hostPorts` | at most 16 | `field: spec.hostPorts` |
//! | `spec.hostPorts[i].hostPort` | 1 to 65535 | `field: spec.hostPorts[i].hostPort` |
//! | `spec.hostPorts[i].containerPort` | absent or 1 to 65535; no job port twice for one protocol, and none the `containerPort` of one of `spec.ports` | `field: spec.hostPorts[i].containerPort` |
//! | `spec.hostPorts[i].protocol` | a known value | `field: spec.hostPorts[i].protocol` |
//!
//! The path rules are `js-types`' ([`js_types::path`]), the ones the agent holds a start to,
//! and the bounds are the agent wire's ([`limits`]), so a job the server takes is a start the
//! wire carries.

use std::collections::BTreeSet;

use js_agent_wire::{
    HostPort, JobMount, Layer, MountMode, MountSource, PortProtocol, Propagation, limits,
};
use js_jobs::DeclaredPort;
use js_proto::v1;
use js_types::{ImageDigestRef, path};

use super::validate::Fault;

/// The layers of `layers`: at most [`limits::MAX_LAYERS`], each with a source.
pub(super) fn layers(layers: &[v1::Layer]) -> Result<Vec<Layer>, Fault> {
    count("spec.layers", layers.len(), limits::MAX_LAYERS)?;
    layers
        .iter()
        .enumerate()
        .map(|(at, layer)| {
            let entry = |field| Fault::Entry("layers", at, field);
            match &layer.source {
                Some(v1::layer::Source::HostPath(text)) => path::absolute(text)
                    .map(|()| Layer::HostPath { path: text.clone() })
                    .map_err(|_| entry("hostPath")),
                Some(v1::layer::Source::Image(text)) => ImageDigestRef::parse(text)
                    .map(|_| Layer::Image {
                        image: text.clone(),
                    })
                    .map_err(|_| entry("image")),
                None => Err(Fault::NoSource("layers", at)),
            }
        })
        .collect()
}

/// The mounts of `mounts`: at most [`limits::MAX_MOUNTS`], each with a source, a target, a
/// known mode and propagation and a `subPath` in its form, and no two targets equal in lower
/// case.
pub(super) fn mounts(mounts: &[v1::Mount]) -> Result<Vec<JobMount>, Fault> {
    count("spec.mounts", mounts.len(), limits::MAX_MOUNTS)?;
    let mut targets = BTreeSet::new();
    mounts
        .iter()
        .enumerate()
        .map(|(at, mount)| {
            let entry = |field| Fault::Entry("mounts", at, field);
            let source = match &mount.source {
                Some(v1::mount::Source::HostPath(text)) => path::absolute(text)
                    .map(|()| MountSource::HostPath { path: text.clone() })
                    .map_err(|_| entry("hostPath"))?,
                None => return Err(Fault::NoSource("mounts", at)),
            };
            let unique = targets.insert(mount.target.to_ascii_lowercase());
            path::target(&mount.target)
                .ok()
                .filter(|()| unique)
                .ok_or(entry("target"))?;
            let mode = match v1::MountMode::try_from(mount.mode) {
                Ok(v1::MountMode::Unspecified | v1::MountMode::ReadOnly) => MountMode::ReadOnly,
                Ok(v1::MountMode::ReadWrite) => MountMode::ReadWrite,
                Ok(v1::MountMode::CopyOnWrite) => MountMode::CopyOnWrite,
                Err(_) => return Err(entry("mode")),
            };
            path::relative(&mount.sub_path, true).map_err(|_| entry("subPath"))?;
            let propagation = match v1::MountPropagation::try_from(mount.propagation) {
                Ok(v1::MountPropagation::Unspecified | v1::MountPropagation::Private) => {
                    Propagation::Private
                }
                Err(_) => return Err(entry("propagation")),
            };
            Ok(JobMount {
                source,
                target: mount.target.clone(),
                mode,
                sub_path: Some(mount.sub_path.clone()).filter(|sub| !sub.is_empty()),
                no_exec: mount.no_exec,
                propagation,
            })
        })
        .collect()
}

/// The host ports of `ports`, beside the job's declared `forwards`: at most
/// [`limits::MAX_PORTS`], each port 1 to 65535 and a known protocol, the job port given (the
/// host port when absent), no job port twice for one protocol and none a declared forward's
/// container port.
pub(super) fn host_ports(
    ports: &[v1::HostPort],
    forwards: &[DeclaredPort],
) -> Result<Vec<HostPort>, Fault> {
    count("spec.hostPorts", ports.len(), limits::MAX_PORTS)?;
    let mut seen = BTreeSet::new();
    ports
        .iter()
        .enumerate()
        .map(|(at, port)| {
            let entry = |field| Fault::Entry("hostPorts", at, field);
            let host_port = number(port.host_port).ok_or(entry("hostPort"))?;
            let job_port = port
                .container_port
                .map_or(Some(host_port), number)
                .ok_or(entry("containerPort"))?;
            let protocol = match v1::PortProtocol::try_from(port.protocol) {
                Ok(v1::PortProtocol::Unspecified | v1::PortProtocol::Tcp) => PortProtocol::Tcp,
                Err(_) => return Err(entry("protocol")),
            };
            let job_port = Some(job_port)
                .filter(|job| !forwards.iter().any(|forward| forward.container == *job))
                .filter(|job| seen.insert((protocol, *job)))
                .ok_or(entry("containerPort"))?;
            Ok(HostPort {
                host_port,
                container_port: Some(job_port),
                protocol,
            })
        })
        .collect()
}

/// Whether a job of `layers` and `host_ports` must run in a container: it has layers, which
/// only a container's root can stack, or a host port at a job port of its own, which needs a
/// network of its own.
pub(super) fn needs_container(layers: &[Layer], host_ports: &[HostPort]) -> bool {
    let remapped = host_ports
        .iter()
        .any(|port| port.job_port() != port.host_port);
    !layers.is_empty() | remapped
}

/// A port of 1 to 65535.
fn number(port: u32) -> Option<u16> {
    u16::try_from(port).ok().filter(|port| *port > 0)
}

/// `Ok` when `len` is at most `most`, else a fault naming `field`.
fn count(field: &'static str, len: usize, most: usize) -> Result<(), Fault> {
    match len <= most {
        true => Ok(()),
        false => Err(Fault::Field(field)),
    }
}

#[cfg(test)]
#[path = "rootfs_tests.rs"]
mod tests;
