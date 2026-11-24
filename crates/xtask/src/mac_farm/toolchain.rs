//! The Rust toolchain a farm Mac runs the action with, from the pins of `toolchain.toml`: each
//! component's tarball is downloaded, held to its SHA-256, and its `bin`, `lib` and `libexec`
//! become entries of the input root below one prefix.

#[cfg(test)]
#[path = "toolchain_tests.rs"]
mod tests;

use std::fmt::Display;
use std::io::Read as _;
use std::path::Path;

use serde::Deserialize;

use super::tree::Entry;
use crate::fetch::Sha256;

/// The pins.
pub const PINS: &str = include_str!("toolchain.toml");

/// The directories of a component that are sent: what `cargo test` runs.
const KEPT: [&str; 3] = ["bin", "lib", "libexec"];

/// The toolchain's pins.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    /// The Rust release.
    pub rust: String,
    /// Its components.
    pub component: Vec<Pin>,
}

/// One component tarball.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// The component: `rustc`, `cargo`, `rust-std`.
    pub name: String,
    /// Its tarball, over HTTPS.
    pub url: String,
    /// The tarball's SHA-256, 64 lowercase hex digits.
    pub sha256: String,
    /// The component's directory in the tarball, under its top directory.
    pub dir: String,
}

/// Why the toolchain could not be made.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolchainError {
    /// The pins file is not the pins' TOML.
    #[error("the toolchain pins do not parse: {0}")]
    Pins(String),
    /// A pin whose URL is not HTTPS or whose SHA-256 is not one.
    #[error("the pin of {name} has no https URL, or a SHA-256 that is not 64 hex digits")]
    Pin {
        /// The component.
        name: String,
    },
    /// The tarball could not be downloaded.
    #[error("{name}: {why}")]
    Download {
        /// The component.
        name: String,
        /// Why not.
        why: String,
    },
    /// The download is not the pinned tarball.
    #[error("{name}: the download's SHA-256 is {got}, the pin says {want}")]
    Sha256 {
        /// The component.
        name: String,
        /// The download's SHA-256.
        got: String,
        /// The pin's.
        want: String,
    },
    /// The tarball is not an xz-compressed tar.
    #[error("{name}: the tarball does not unpack: {why}")]
    Unpack {
        /// The component.
        name: String,
        /// Why not.
        why: String,
    },
    /// A member that would be sent is a hard link, a device or another kind.
    #[error("{name}: {path} is neither a file nor a symlink")]
    Kind {
        /// The component.
        name: String,
        /// The member's path in the tarball.
        path: String,
    },
    /// No member is under the pin's directory.
    #[error("{name}: nothing of the tarball is under {dir}/bin, lib or libexec")]
    Empty {
        /// The component.
        name: String,
        /// The pin's directory.
        dir: String,
    },
}

/// The pins of `text`.
///
/// # Errors
/// [`ToolchainError::Pins`] or [`ToolchainError::Pin`].
pub fn parse(text: &str) -> Result<Pins, ToolchainError> {
    let pins: Pins = toml::from_str(text).map_err(|e| ToolchainError::Pins(e.to_string()))?;
    for pin in &pins.component {
        if !pin.url.starts_with("https://") || pin.sha256.parse::<Sha256>().is_err() {
            return Err(ToolchainError::Pin {
                name: pin.name.clone(),
            });
        }
    }
    Ok(pins)
}

/// Downloads a URL: its bytes, or why not.
pub type Download<'a> = dyn FnMut(&str) -> Result<Vec<u8>, String> + 'a;

/// The entries of every component of `pins` under `prefix`, each tarball downloaded by
/// `download` and held to its pin.
///
/// # Errors
/// [`ToolchainError`] for the first component that is not its pin, or does not unpack.
pub fn entries(
    pins: &Pins,
    prefix: &str,
    download: &mut Download<'_>,
) -> Result<Vec<(String, Entry)>, ToolchainError> {
    let mut out = Vec::new();
    for pin in &pins.component {
        let bytes = download(&pin.url).map_err(|why| ToolchainError::Download {
            name: pin.name.clone(),
            why,
        })?;
        let got = Sha256::of(&bytes).to_string();
        if got != pin.sha256 {
            return Err(ToolchainError::Sha256 {
                name: pin.name.clone(),
                got,
                want: pin.sha256.clone(),
            });
        }
        let before = out.len();
        unpack(pin, &bytes, prefix, &mut out)?;
        if out.len() == before {
            return Err(ToolchainError::Empty {
                name: pin.name.clone(),
                dir: pin.dir.clone(),
            });
        }
    }
    Ok(out)
}

/// The path below `<top>/<dir>/` of a member, when it is under one of [`KEPT`].
fn kept(path: &Path, dir: &str) -> Option<String> {
    let parts: Vec<&str> = path.iter().filter_map(std::ffi::OsStr::to_str).collect();
    match parts.as_slice() {
        [_top, d, first, ..] if *d == dir && KEPT.contains(first) => Some(parts[2..].join("/")),
        _ => None,
    }
}

/// A closure that makes the [`ToolchainError::Unpack`] of the component `name`.
fn unpacking<E: Display>(name: &str) -> impl FnOnce(E) -> ToolchainError + '_ {
    move |e| ToolchainError::Unpack {
        name: name.to_owned(),
        why: e.to_string(),
    }
}

/// The members of the `.tar.xz` `xz` under the component's [`KEPT`] directories, into `out`
/// below `prefix`.
fn unpack(
    pin: &Pin,
    xz: &[u8],
    prefix: &str,
    out: &mut Vec<(String, Entry)>,
) -> Result<(), ToolchainError> {
    let mut tar = Vec::new();
    lzma_rs::xz_decompress(&mut &xz[..], &mut tar).map_err(unpacking(&pin.name))?;
    let mut archive = tar::Archive::new(tar.as_slice());
    for member in archive.entries().map_err(unpacking(&pin.name))? {
        let mut member = member.map_err(unpacking(&pin.name))?;
        let path = member.path().map_err(unpacking(&pin.name))?.into_owned();
        let Some(rest) = kept(&path, &pin.dir) else {
            continue;
        };
        let header = member.header();
        let kind = header.entry_type();
        let entry = if kind.is_dir() {
            continue;
        } else if kind.is_file() {
            let executable = header.mode().map_err(unpacking(&pin.name))? & 0o111 != 0;
            let mut bytes = Vec::new();
            member
                .read_to_end(&mut bytes)
                .map_err(unpacking(&pin.name))?;
            Entry::File { bytes, executable }
        } else if kind.is_symlink() {
            let target = member
                .link_name()
                .map_err(unpacking(&pin.name))?
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            Entry::Symlink(target)
        } else {
            return Err(ToolchainError::Kind {
                name: pin.name.clone(),
                path: path.display().to_string(),
            });
        };
        out.push((format!("{prefix}/{rest}"), entry));
    }
    Ok(())
}
