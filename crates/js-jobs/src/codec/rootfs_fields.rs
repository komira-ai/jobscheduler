//! A job's file system beyond its image (`docs/design/job-rootfs.md`): the job row's layers
//! (tag 54), mounts (55) and host ports (56), the agent's code of a refused start (57), the
//! code of an input that failed while the job ran (58, a result field), and the job row's
//! format when the job has no image.
//!
//! Each of tags 54 to 56 is a list, and is written only when it holds an item, so the row of a
//! job that names none is the bytes a build before these fields wrote. Tags 57 and 58 are text,
//! written only when there is a code; 58 is read only beside a result (`result_fields`).
//!
//! - A layer is its kind (one byte: 0 a host path, 1 an image) and its text.
//! - A mount is a list of its parts: its source (a kind byte, 0 a host path, and its text), its
//!   target, its mode (one byte: 0 read-only, 1 read-write, 2 copy-on-write), its `sub_path`
//!   (empty when absent, else 1 and the text), `no_exec` (0 or 1) and its propagation (one
//!   byte: 0 private).
//! - A host port is five bytes: the host port and the job port (`u16` each, the job port 0
//!   when absent) and the protocol (one byte: 0 TCP).
//!
//! **Additive (B8).** A kind or enum value this build does not know is [`DecodeError::Newer`],
//! as is a mount with more parts or a host port with more bytes than this build reads: a later
//! build that adds an option to a mount or a host port writes it there, and a build that
//! cannot keep the option leaves the row out rather than rewrite it without the option.
//!
//! **A job without an image.** Every row before these fields holds an image (tag 2), which a
//! build before them reads as a field every row holds: such a build fails a row without one as
//! corrupt, and a listing with it. So the row of a job without an image is written with the
//! format byte [`FORMAT_NO_IMAGE`], which such a build reads as a later build's row and leaves
//! out. This build reads both format bytes for every row; only a job row without an image is
//! written with the second.

use js_agent_wire::{HostPort, JobMount, Layer, MountMode, MountSource, PortProtocol, Propagation};
use js_types::ImageDigestRef;

use super::{
    DecodeError, FORMAT, Fields, Writer, byte_of, image_of, items_of, job_tag, list, text, text_of,
};

/// The format byte of the row of a job without an image: format 1 otherwise, under a byte a
/// build before it reads as a later build's.
pub(crate) const FORMAT_NO_IMAGE: u8 = 2;

/// The tags these fields add to a job row, after the queue fields' 48 to 53.
pub(crate) mod job_rootfs_tag {
    /// The layers (a list of layers).
    pub const LAYERS: u16 = 54;
    /// The mounts (a list of mounts).
    pub const MOUNTS: u16 = 55;
    /// The host ports (a list of five-byte host ports).
    pub const HOST_PORTS: u16 = 56;
    /// The agent's code of a start it refused (text).
    pub const START_FAILURE_DETAIL: u16 = 57;
    /// With a result whose reason is an input failure, the agent's code of the input (text).
    pub const RESULT_INPUT_FAILURE: u16 = 58;
}

use job_rootfs_tag as t;

/// The kind byte of a host path, as a layer's or a mount's source.
const HOST_PATH: u8 = 0;
/// The kind byte of an image layer.
const IMAGE: u8 = 1;
/// The parts of a mount this build reads.
const MOUNT_PARTS: usize = 6;
/// The bytes of a host port this build reads.
const HOST_PORT_LEN: usize = 5;

/// The format byte of the row of a job whose image is `image`.
pub(super) fn format_of(image: Option<&ImageDigestRef>) -> u8 {
    image.map_or(FORMAT_NO_IMAGE, |_| FORMAT)
}

/// Puts the image, when the job has one, in `row`.
pub(super) fn put_image(row: &mut Writer, image: Option<&ImageDigestRef>) {
    row.put_opt(
        job_tag::IMAGE,
        image.map(|image| text(&format!("{}@{}", image.name(), image.digest()))),
    );
}

/// The image of a row: a row of format 1 holds one, a row of [`FORMAT_NO_IMAGE`] may not.
pub(super) fn take_image(fields: &mut Fields) -> Result<Option<ImageDigestRef>, DecodeError> {
    let image = fields.opt(job_tag::IMAGE, image_of)?;
    match (image, fields.1) {
        (None, FORMAT) => Err(DecodeError::Missing {
            tag: job_tag::IMAGE,
        }),
        (image, _) => Ok(image),
    }
}

/// Puts the layers, mounts and host ports in `row`, each only when it holds an item.
pub(super) fn put_spec(
    row: &mut Writer,
    layers: &[Layer],
    mounts: &[JobMount],
    host_ports: &[HostPort],
) {
    let layers: Vec<Vec<u8>> = layers.iter().map(layer).collect();
    let mounts: Vec<Vec<u8>> = mounts.iter().map(mount).collect();
    let ports: Vec<Vec<u8>> = host_ports.iter().map(host_port).collect();
    for (tag, items) in [
        (t::LAYERS, layers),
        (t::MOUNTS, mounts),
        (t::HOST_PORTS, ports),
    ] {
        row.put_opt(
            tag,
            (!items.is_empty()).then(|| list(items.iter().map(Vec::as_slice))),
        );
    }
}

/// A job's layers, mounts and host ports.
type Parts = (Vec<Layer>, Vec<JobMount>, Vec<HostPort>);

/// The layers, mounts and host ports of a row, each empty when the row lacks it.
pub(super) fn take_spec(fields: &mut Fields) -> Result<Parts, DecodeError> {
    let layers = fields.opt(t::LAYERS, |tag, value| each(tag, &value, layer_of))?;
    let mounts = fields.opt(t::MOUNTS, |tag, value| each(tag, &value, mount_of))?;
    let ports = fields.opt(t::HOST_PORTS, |tag, value| each(tag, &value, host_port_of))?;
    Ok((
        layers.unwrap_or_default(),
        mounts.unwrap_or_default(),
        ports.unwrap_or_default(),
    ))
}

/// Puts the agent's code of a refused start, when there is one, in `row`.
pub(super) fn put_detail(row: &mut Writer, detail: Option<&str>) {
    row.put_opt(t::START_FAILURE_DETAIL, detail.map(text));
}

/// The agent's code of a refused start, when the row holds one.
pub(super) fn take_detail(fields: &mut Fields) -> Result<Option<String>, DecodeError> {
    fields.opt(t::START_FAILURE_DETAIL, text_of)
}

/// Each item of the list value `value` of `tag`, read by `read`.
fn each<T>(
    tag: u16,
    value: &[u8],
    read: fn(u16, &[u8]) -> Result<T, DecodeError>,
) -> Result<Vec<T>, DecodeError> {
    items_of(tag, value)?
        .into_iter()
        .map(|item| read(tag, item))
        .collect()
}

/// A kind byte and its text.
fn kinded(kind: u8, value: &str) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(value.as_bytes());
    out
}

/// The kind byte and the text of `item`.
fn kind_of(tag: u16, item: &[u8]) -> Result<(u8, String), DecodeError> {
    let (&kind, rest) = item.split_first().ok_or(DecodeError::BadValue { tag })?;
    Ok((kind, text_of(tag, rest.to_vec())?))
}

fn layer(layer: &Layer) -> Vec<u8> {
    match layer {
        Layer::HostPath { path } => kinded(HOST_PATH, path),
        Layer::Image { image } => kinded(IMAGE, image),
    }
}

fn layer_of(tag: u16, item: &[u8]) -> Result<Layer, DecodeError> {
    match kind_of(tag, item)? {
        (HOST_PATH, path) => Ok(Layer::HostPath { path }),
        (IMAGE, image) => Ok(Layer::Image { image }),
        _ => Err(DecodeError::Newer),
    }
}

fn mount(mount: &JobMount) -> Vec<u8> {
    let MountSource::HostPath { path } = &mount.source;
    let sub_path = mount
        .sub_path
        .as_deref()
        .map_or_else(Vec::new, |sub| kinded(1, sub));
    let mode = match mount.mode {
        MountMode::ReadOnly => 0,
        MountMode::ReadWrite => 1,
        MountMode::CopyOnWrite => 2,
    };
    let Propagation::Private = mount.propagation;
    let parts = [
        kinded(HOST_PATH, path),
        text(&mount.target),
        vec![mode],
        sub_path,
        vec![u8::from(mount.no_exec)],
        vec![0],
    ];
    list(parts.iter().map(Vec::as_slice))
}

fn mount_of(tag: u16, item: &[u8]) -> Result<JobMount, DecodeError> {
    let parts = items_of(tag, item)?;
    let [source, target, mode, sub_path, no_exec, propagation] = parts.as_slice() else {
        return Err(match parts.len() > MOUNT_PARTS {
            true => DecodeError::Newer,
            false => DecodeError::BadValue { tag },
        });
    };
    let source = match kind_of(tag, source)? {
        (HOST_PATH, path) => MountSource::HostPath { path },
        _ => return Err(DecodeError::Newer),
    };
    let mode = match byte_of(tag, mode)? {
        0 => MountMode::ReadOnly,
        1 => MountMode::ReadWrite,
        2 => MountMode::CopyOnWrite,
        _ => return Err(DecodeError::Newer),
    };
    let sub_path = match sub_path {
        [] => None,
        _ => match kind_of(tag, sub_path)? {
            (1, sub) => Some(sub),
            _ => return Err(DecodeError::BadValue { tag }),
        },
    };
    let no_exec = match byte_of(tag, no_exec)? {
        0 => false,
        1 => true,
        _ => return Err(DecodeError::BadValue { tag }),
    };
    let propagation = match byte_of(tag, propagation)? {
        0 => Propagation::Private,
        _ => return Err(DecodeError::Newer),
    };
    Ok(JobMount {
        source,
        target: text_of(tag, target.to_vec())?,
        mode,
        sub_path,
        no_exec,
        propagation,
    })
}

fn host_port(port: &HostPort) -> Vec<u8> {
    let PortProtocol::Tcp = port.protocol;
    let mut out = port.host_port.to_be_bytes().to_vec();
    out.extend_from_slice(&port.container_port.unwrap_or(0).to_be_bytes());
    out.push(0);
    out
}

fn host_port_of(tag: u16, item: &[u8]) -> Result<HostPort, DecodeError> {
    if item.len() > HOST_PORT_LEN {
        return Err(DecodeError::Newer);
    }
    let Ok([h0, h1, c0, c1, protocol]) = <[u8; HOST_PORT_LEN]>::try_from(item) else {
        return Err(DecodeError::BadLength { tag });
    };
    let protocol = match protocol {
        0 => PortProtocol::Tcp,
        _ => return Err(DecodeError::Newer),
    };
    let container = u16::from_be_bytes([c0, c1]);
    Ok(HostPort {
        host_port: u16::from_be_bytes([h0, h1]),
        container_port: (container != 0).then_some(container),
        protocol,
    })
}

#[cfg(test)]
#[path = "rootfs_fields_tests.rs"]
mod tests;
