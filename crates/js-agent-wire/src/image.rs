//! The image reference grammar: `name[:tag][@digest]`, as the OCI distribution
//! specification and the docker reference grammar define it.
//!
//! A bare character set is not enough: `oci:/var/lib/x` and `dir:/etc` use only allowed
//! characters, yet a tool such as podman or skopeo reads them as a local transport and a
//! host path, not as a registry reference. The grammar below has no place for either.

use crate::check::{At, lower_alnum};
use crate::error::{Rule, ValidationError};
use crate::limits;

/// Names a container tool reads as a transport when one is followed by `:`. A reference
/// that starts `<transport>:` is refused even where the rest would parse as a tag
/// (`containers-storage:foo`), because the tool would not read it as one.
const TRANSPORTS: [&str; 10] = [
    "containers-storage",
    "dir",
    "docker",
    "docker-archive",
    "docker-daemon",
    "oci",
    "oci-archive",
    "ostree",
    "sif",
    "tarball",
];

/// The longest tag, in bytes.
const MAX_TAG_LEN: usize = 128;

/// The fewest hex digits of a digest.
const MIN_DIGEST_HEX: usize = 32;

/// A non-empty reference of at most [`limits::MAX_IMAGE_LEN`] bytes that follows the
/// grammar and names no transport.
pub(crate) fn image(at: At, value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if value.len() > limits::MAX_IMAGE_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    if !is_reference(value) {
        return Err(at.fail(Rule::BadChars));
    }
    Ok(())
}

fn is_reference(value: &str) -> bool {
    let transport = value
        .split_once(':')
        .is_some_and(|(head, _)| TRANSPORTS.contains(&head));
    let (rest, digest) = value
        .split_once('@')
        .map_or((value, None), |(rest, digest)| (rest, Some(digest)));
    let (name, tag) = split_tag(rest);
    [
        !transport,
        digest.is_none_or(is_digest),
        tag.is_none_or(is_tag),
        is_name(name),
    ]
    .into_iter()
    .all(|ok| ok)
}

/// Splits `name:tag`: the tag follows a `:` after the last `/`, so the `:` of a registry
/// port (which sits in the first component, before a `/`) is not a tag separator.
fn split_tag(rest: &str) -> (&str, Option<&str>) {
    let start = rest.rfind('/').map_or(0, |slash| slash + 1);
    match rest[start..].rfind(':') {
        Some(colon) => (&rest[..start + colon], Some(&rest[start + colon + 1..])),
        None => (rest, None),
    }
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
fn is_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    let leads = bytes.first().is_some_and(|byte| word(*byte));
    let inside = bytes
        .iter()
        .all(|byte| word(*byte) || matches!(byte, b'.' | b'-'));
    [(1..=MAX_TAG_LEN).contains(&bytes.len()), leads, inside]
        .into_iter()
        .all(|ok| ok)
}

/// `algorithm:hex`: the algorithm is `[a-z0-9]+` parts joined by one of `+._-`, the hex
/// is at least [`MIN_DIGEST_HEX`] lower-case hex digits.
fn is_digest(digest: &str) -> bool {
    let Some((algorithm, hex)) = digest.split_once(':') else {
        return false;
    };
    let parts = algorithm
        .split(['+', '.', '_', '-'])
        .all(|part| !part.is_empty() && part.bytes().all(lower_alnum));
    let digits = hex
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    [parts, digits, hex.len() >= MIN_DIGEST_HEX]
        .into_iter()
        .all(|ok| ok)
}

/// `[domain/]path`: a first component of several is a domain (with an optional port)
/// or a path component; every other component is a path component.
fn is_name(name: &str) -> bool {
    match name.split_once('/') {
        None => is_path_component(name),
        Some((first, rest)) => {
            (is_domain(first) || is_path_component(first)) && rest.split('/').all(is_path_component)
        }
    }
}

/// `[a-z0-9]+` runs joined by one separator: `.`, `_`, `__` or any number of `-`.
fn is_path_component(component: &str) -> bool {
    let runs: Vec<&[u8]> = component
        .as_bytes()
        .chunk_by(|a, b| lower_alnum(*a) == lower_alnum(*b))
        .collect();
    let ends = |run: Option<&&[u8]>| run.is_some_and(|run| lower_alnum(run[0]));
    let separators = runs
        .iter()
        .filter(|run| !lower_alnum(run[0]))
        .all(|run| matches!(*run, b"." | b"_" | b"__") || run.iter().all(|byte| *byte == b'-'));
    [ends(runs.first()), ends(runs.last()), separators]
        .into_iter()
        .all(|ok| ok)
}

/// Dot-separated host labels (`[A-Za-z0-9]`, `-` inside) and an optional `:port` of
/// digits.
fn is_domain(component: &str) -> bool {
    let (host, port) = component
        .split_once(':')
        .map_or((component, None), |(host, port)| (host, Some(port)));
    let port_ok = port.is_none_or(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    [port_ok, host.split('.').all(is_host_label)]
        .into_iter()
        .all(|ok| ok)
}

fn is_host_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    let ends = |byte: Option<&u8>| byte.is_some_and(|b| b.is_ascii_alphanumeric());
    let inside = bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-');
    [ends(bytes.first()), ends(bytes.last()), inside]
        .into_iter()
        .all(|ok| ok)
}
