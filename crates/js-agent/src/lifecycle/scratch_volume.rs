//! The scratch root's own APFS volume on a Mac, [`SCRATCH_VOLUME`], in the container of the
//! Data volume ([`DATA_VOLUME`]): how it is found and how it is mounted on the scratch root.
//! The update hook's host setup (`prepare` on a Mac) makes the volume and mounts it; the agent's
//! own start mounts it again ([`mount_at_start`]), because nothing else does after a boot.
//!
//! Nothing names the volume in the file system table: since macOS 26.6 the system's sandbox lets
//! only a process with Full Disk Access (a privacy grant of TCC, which root does not imply)
//! write `/etc/fstab` and answers any other with `EPERM`, and a launchd daemon on a Mac with no
//! device management has no way to be given that grant. So the volume is mounted by its device
//! with `diskutil mount nobrowse -mountPoint <scratch root> <device>`, which asks DiskArbitration
//! to mount it: that takes root and no grant.
//!
//! Every command is `diskutil` ([`DISKUTIL`]) with a `-plist` form where one is read, run
//! through a [`CommandRunner`], so a test answers as a Mac does.

use std::path::Path;

use plist::{Dictionary, Value};

use crate::facts::{CommandRunner, FileReader};

/// The tool that reads, makes and mounts volumes.
pub const DISKUTIL: &str = "/usr/sbin/diskutil";

/// The mount point of the Data volume, which holds `/var/db` and so the state root: the scratch
/// volume is in its container.
pub const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The name of the scratch root's own APFS volume.
pub const SCRATCH_VOLUME: &str = "js-agent-scratch";

/// The scratch volume as `diskutil apfs list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// `DeviceIdentifier` (`disk3s7`): the name `diskutil` is given to mount it.
    pub device: String,
}

/// The APFS container of the Data volume as `diskutil apfs list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// `ContainerReference` (`disk3`): the name `diskutil apfs addVolume` is given.
    pub reference: String,
    /// `CapacityFree`: its free space, in bytes.
    pub free: u64,
    /// The scratch volume, when the container holds one.
    pub volume: Option<Volume>,
}

/// Why the scratch volume could not be found or mounted. Each names the command or the answer
/// at fault.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VolumeError {
    /// `diskutil` failed: the command line and how it ended.
    #[error("{line}: {why}")]
    Command {
        /// The program and its arguments.
        line: String,
        /// How it ended, as the runner says.
        why: String,
    },
    /// A `-plist` command printed something that is not one dictionary.
    #[error("{line} printed no property list dictionary")]
    NoDictionary {
        /// The program and its arguments.
        line: String,
    },
    /// An answer lacks a key the setup reads.
    #[error("{DISKUTIL} shows {what} with no {key}")]
    NoKey {
        /// What the answer describes.
        what: String,
        /// The key it lacks.
        key: &'static str,
    },
    /// `diskutil apfs list` does not list the container it was asked for.
    #[error("{DISKUTIL} apfs list does not list the container {0}")]
    NoContainer(String),
    /// The container is listed without its free space.
    #[error("{DISKUTIL} shows the container {0} with no CapacityFree")]
    NoFree(String),
}

/// Runs `diskutil` with `args`: what it printed.
///
/// # Errors
///
/// [`VolumeError::Command`] naming the command line.
pub fn diskutil(runner: &dyn CommandRunner, args: &[&str]) -> Result<String, VolumeError> {
    runner
        .run(DISKUTIL, args)
        .map_err(|why| VolumeError::Command {
            line: format!("{DISKUTIL} {}", args.join(" ")),
            why,
        })
}

/// Runs `diskutil` with `args` (a `-plist` form): the dictionary it printed.
fn dictionary(runner: &dyn CommandRunner, args: &[&str]) -> Result<Dictionary, VolumeError> {
    let text = diskutil(runner, args)?;
    Value::from_reader_xml(text.as_bytes())
        .ok()
        .and_then(Value::into_dictionary)
        .ok_or_else(|| VolumeError::NoDictionary {
            line: format!("{DISKUTIL} {}", args.join(" ")),
        })
}

/// The text `key` of `dict`, which describes `what`.
fn text<'d>(dict: &'d Dictionary, key: &'static str, what: &str) -> Result<&'d str, VolumeError> {
    dict.get(key)
        .and_then(Value::as_string)
        .ok_or_else(|| VolumeError::NoKey {
            what: what.to_owned(),
            key,
        })
}

/// The container of the Data volume, with the scratch volume in it if it has one.
///
/// # Errors
///
/// A [`VolumeError`] naming the command or the answer that cannot be used.
pub fn find(runner: &dyn CommandRunner) -> Result<Container, VolumeError> {
    let data = dictionary(runner, &["info", "-plist", DATA_VOLUME])?;
    let reference = text(&data, "APFSContainerReference", DATA_VOLUME)?;
    listed(runner, reference)
}

/// The APFS container `reference` as `diskutil apfs list` shows it: its free space and the
/// scratch volume in it, if it has one.
///
/// # Errors
///
/// A [`VolumeError`] naming the command or the answer that cannot be used.
pub fn listed(runner: &dyn CommandRunner, reference: &str) -> Result<Container, VolumeError> {
    let list = dictionary(runner, &["apfs", "list", "-plist", reference])?;
    let entry = list
        .get("Containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
        .find(|entry| entry.get("ContainerReference").and_then(Value::as_string) == Some(reference))
        .ok_or_else(|| VolumeError::NoContainer(reference.to_owned()))?;
    let free = entry
        .get("CapacityFree")
        .and_then(Value::as_unsigned_integer)
        .ok_or_else(|| VolumeError::NoFree(reference.to_owned()))?;
    let volume = entry
        .get("Volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
        .find(|volume| volume.get("Name").and_then(Value::as_string) == Some(SCRATCH_VOLUME))
        .map(|volume| -> Result<Volume, VolumeError> {
            Ok(Volume {
                device: text(volume, "DeviceIdentifier", SCRATCH_VOLUME)?.to_owned(),
            })
        })
        .transpose()?;
    Ok(Container {
        reference: reference.to_owned(),
        free,
        volume,
    })
}

/// Mounts `volume` on `scratch`, unmounting it first when it is mounted anywhere (macOS may have
/// mounted it in `/Volumes` by itself, at a boot or when it was made without `-nomount`). It is
/// mounted `nobrowse`, as the table line of an earlier release named it, so that the Finder does
/// not show it as a disk. `point` is called before each change: the hook's kill points.
///
/// # Errors
///
/// A [`VolumeError`] naming the command or the answer at fault.
pub fn mount_on(
    runner: &dyn CommandRunner,
    point: &dyn Fn(),
    volume: &Volume,
    scratch: &Path,
) -> Result<(), VolumeError> {
    let info = dictionary(runner, &["info", "-plist", &volume.device])?;
    if info
        .get("MountPoint")
        .and_then(Value::as_string)
        .is_some_and(|at| !at.is_empty())
    {
        point();
        diskutil(runner, &["unmount", &volume.device])?;
    }
    point();
    let at = scratch.to_string_lossy();
    diskutil(
        runner,
        &["mount", "nobrowse", "-mountPoint", &at, &volume.device],
    )?;
    Ok(())
}

/// The scratch volume mounted on `scratch` at the agent's start, while the scratch root is on
/// the file system of the state root `state`: what a boot leaves undone, since nothing names
/// the volume in the file system table. A root on a file system of its own already, or one
/// that cannot be looked at, is left to the agent's check of its file systems, which follows.
/// The volume must be there: the update hook's `prepare` makes it, and this makes nothing.
///
/// # Errors
///
/// The text, which starts with `--scratch-root:`, names the command or the answer at fault, or
/// says that the Data volume's container holds no scratch volume.
pub fn mount_at_start(
    runner: &dyn CommandRunner,
    files: &dyn FileReader,
    state: &Path,
    scratch: &Path,
) -> Result<(), String> {
    let shared = matches!(
        (files.device(state), files.device(scratch)),
        (Ok(own), Ok(found)) if own == found
    );
    if !shared {
        return Ok(());
    }
    let flagged = |why: VolumeError| format!("--scratch-root: {why}");
    let container = find(runner).map_err(flagged)?;
    let Some(volume) = container.volume else {
        return Err(format!(
            "--scratch-root: {} is on the file system of the --state-root and {DISKUTIL} lists \
             no {SCRATCH_VOLUME} in {}, the Data volume's container: the update hook's prepare \
             makes it",
            scratch.display(),
            container.reference
        ));
    };
    mount_on(runner, &|| {}, &volume, scratch).map_err(flagged)
}
