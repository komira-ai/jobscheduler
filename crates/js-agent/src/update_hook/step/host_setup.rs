//! The host setup of a Mac, part of `prepare`: what the package's two daemons need of the host
//! and no file of a package can carry, made by the hook, which hostkeeper runs as root, for each
//! part the params name. hostkeeper is the only way onto the Macs, so nothing here is left to an
//! operator.
//!
//! - `socket_group`: the group the root helper gives its socket (`js-mac-session
//!   --socket-group`). When the local directory (`dscl . -list /Groups PrimaryGroupID`) does
//!   not list it, it is made with `dseditgroup -o create`, under the gid macOS picks. A group
//!   with staff's gid ([`STAFF_GID`], the lease users' group, which the helper's plist leaves at
//!   its default) is refused, as the helper refuses it at its start.
//! - `scratch_root`: the agent's `--scratch-root`, which must be on another file system than its
//!   `--state-root` (the agent refuses a shared `st_dev`). The state root and the scratch root
//!   are made when they are not there, with each missing directory above them (mode 0755), and
//!   on every run are made the hook's user's, mode 0700, as the agent requires of them. While the
//!   two share a device the scratch root is given an APFS volume of its own, [`SCRATCH_VOLUME`],
//!   in the container of the Data volume ([`DATA_VOLUME`]), with a quota of [`QUOTA_TENTHS`]
//!   tenths of the container's free space when it is made. The quota is a ceiling, not a
//!   reservation: jobs that fill the volume leave the rest of that free space to the system and
//!   the state root, as long as they do not grow into it themselves. The volume is made
//!   unmounted (`diskutil apfs addVolume ... -nomount`), named in the file system table by its
//!   UUID (`UUID=<uuid> <scratch root> apfs rw,nobrowse`, so that every boot mounts it there; a
//!   line that names the volume with anything else is replaced), and only then mounted on the
//!   scratch root; one that is mounted elsewhere is unmounted first. A scratch root its user does
//!   not own (a volume that ignores ownership) is given ownership (`diskutil enableOwnership`)
//!   before it is made the hook's user's again.
//!
//! Each part looks before it changes anything and makes only what is not as required (the
//! owner and mode of the two roots are set on every run), so a run killed at any point and run
//! again ends as an uninterrupted one; every change passes a kill point first. A part that
//! cannot be read or made ends the step deferred (75,
//! `host_setup_missing`) with the part and the cause named: no record is written for it and the
//! host is not cordoned, and hostkeeper orders the host again later. The setup belongs to the
//! host, not to a release: no step undoes it.

use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, chown};
use std::path::Path;

use js_mac_session::start::STAFF_GID;
use plist::{Dictionary, Value};

use crate::update_hook::{Done, Params, Reason};

use super::prepare::deferred;
use super::{Env, PathFacts};

/// The directory service's command line tool.
pub const DSCL: &str = "/usr/bin/dscl";

/// The tool that makes a group.
pub const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";

/// The tool that reads, makes and mounts volumes.
pub const DISKUTIL: &str = "/usr/sbin/diskutil";

/// The mount point of the Data volume, which holds `/var/db` and so the state root: the scratch
/// volume is made in its container.
pub const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The name of the scratch root's own APFS volume.
pub const SCRATCH_VOLUME: &str = "js-agent-scratch";

/// The scratch volume's quota, in tenths of its container's free space when it is made: a
/// ceiling on what jobs write there, so that the rest of that space stays for the system and the
/// state root unless they fill it themselves.
pub const QUOTA_TENTHS: u64 = 9;

/// The file system table of a Mac, where the scratch volume is named so that every boot mounts
/// it on the scratch root.
pub const FSTAB: &str = "/etc/fstab";

/// The scratch volume as `diskutil apfs list` shows it.
struct Volume {
    /// `APFSVolumeUUID`: the name the file system table gives it.
    uuid: String,
    /// `DeviceIdentifier` (`disk3s7`): the name `diskutil` is given to mount it.
    device: String,
}

/// Makes each part of the host setup the params name: the socket group, then the roots.
pub(super) fn set_up(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    if let Some(group) = &params.settings.socket_group {
        socket_group(env, params, group)?;
    }
    if let Some(scratch) = &params.scratch_root {
        roots(env, params, scratch)?;
    }
    Ok(())
}

/// The deferral of a host setup that is not complete, with `detail` naming the part and the
/// cause.
fn missing(params: &Params, detail: String) -> Done {
    deferred(params, Reason::HostSetupMissing, detail)
}

/// The deferral for a path that cannot be read or made.
fn io_missing(params: &Params, path: &Path, why: &io::Error) -> Done {
    missing(params, format!("{}: {why}", path.display()))
}

/// Runs `program` with `args`: what it printed, or the deferral naming the command.
fn command(env: &Env<'_>, params: &Params, program: &str, args: &[&str]) -> Result<String, Done> {
    env.runner
        .run(program, args)
        .map_err(|why| missing(params, format!("{program} {}: {why}", args.join(" "))))
}

/// Runs `diskutil` with `args` (a `-plist` form): the dictionary it printed.
fn plist_of(env: &Env<'_>, params: &Params, args: &[&str]) -> Result<Dictionary, Done> {
    let text = command(env, params, DISKUTIL, args)?;
    Value::from_reader_xml(text.as_bytes())
        .ok()
        .and_then(Value::into_dictionary)
        .ok_or_else(|| {
            missing(
                params,
                format!(
                    "{DISKUTIL} {} printed no property list dictionary",
                    args.join(" ")
                ),
            )
        })
}

/// The text `key` of `dict`, which describes `what`.
fn text<'d>(params: &Params, dict: &'d Dictionary, key: &str, what: &str) -> Result<&'d str, Done> {
    dict.get(key)
        .and_then(Value::as_string)
        .ok_or_else(|| missing(params, format!("{DISKUTIL} shows {what} with no {key}")))
}

/// The socket group is there, made if it was not, and not staff's.
fn socket_group(env: &Env<'_>, params: &Params, group: &str) -> Result<(), Done> {
    let gid = match gid_of(env, params, group)? {
        Some(gid) => gid,
        None => {
            env.ports.point();
            command(env, params, DSEDITGROUP, &["-o", "create", group])?;
            gid_of(env, params, group)?.ok_or_else(|| {
                missing(
                    params,
                    format!("the group {group} is not listed after {DSEDITGROUP} made it"),
                )
            })?
        }
    };
    if gid == STAFF_GID {
        return Err(missing(
            params,
            format!(
                "the group {group} has gid {gid}, staff's: the helper refuses a socket group its lease users are in"
            ),
        ));
    }
    Ok(())
}

/// The gid of `group` in the local directory, `None` when it lists no such group.
fn gid_of(env: &Env<'_>, params: &Params, group: &str) -> Result<Option<u32>, Done> {
    let listed = command(
        env,
        params,
        DSCL,
        &[".", "-list", "/Groups", "PrimaryGroupID"],
    )?;
    let Some(line) = listed
        .lines()
        .find(|line| line.split_whitespace().next() == Some(group))
    else {
        return Ok(None);
    };
    match line
        .split_whitespace()
        .nth(1)
        .and_then(|gid| gid.parse().ok())
    {
        Some(gid) => Ok(Some(gid)),
        None => Err(missing(
            params,
            format!("{DSCL} lists the group {group} with no gid: {line:?}"),
        )),
    }
}

/// The state root and the scratch root are there, on file systems of their own, and both are the
/// hook's user's, mode 0700.
fn roots(env: &Env<'_>, params: &Params, scratch: &Path) -> Result<(), Done> {
    make_dir(env, params, &params.state_root)?;
    own(env, params, &params.state_root)?;
    make_dir(env, params, scratch)?;
    if shared(env, params, scratch)? {
        volume(env, params, scratch)?;
        if shared(env, params, scratch)? {
            return Err(missing(
                params,
                format!(
                    "the scratch root {} is on the file system of the state root {} after {SCRATCH_VOLUME} was mounted on it",
                    scratch.display(),
                    params.state_root.display()
                ),
            ));
        }
    }
    let scratch_arg = scratch.to_string_lossy();
    if look(env, params, scratch)?.uid != env.euid {
        env.ports.point();
        command(env, params, DISKUTIL, &["enableOwnership", &scratch_arg])?;
    }
    own(env, params, scratch)
}

/// Makes `dir` the hook's user's, mode 0700 (a link followed).
fn own(env: &Env<'_>, params: &Params, dir: &Path) -> Result<(), Done> {
    env.ports.point();
    chown(dir, Some(env.euid), None)
        .and_then(|()| fs::set_permissions(dir, Permissions::from_mode(0o700)))
        .map_err(|why| io_missing(params, dir, &why))
}

/// Makes `dir`, mode 0700, and each missing directory above it, mode 0755; a `dir` that is there
/// is left for [`own`].
fn make_dir(env: &Env<'_>, params: &Params, dir: &Path) -> Result<(), Done> {
    env.ports.point();
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir.parent().unwrap_or(dir))
        .and_then(|()| match DirBuilder::new().mode(0o700).create(dir) {
            Err(why) if why.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            made => made,
        })
        .map_err(|why| io_missing(params, dir, &why))
}

/// What the look at `path` shows.
fn look(env: &Env<'_>, params: &Params, path: &Path) -> Result<PathFacts, Done> {
    (env.look)(path).map_err(|why| io_missing(params, path, &why))
}

/// Whether the scratch root is on the state root's file system.
fn shared(env: &Env<'_>, params: &Params, scratch: &Path) -> Result<bool, Done> {
    Ok(look(env, params, scratch)?.device == look(env, params, &params.state_root)?.device)
}

/// Gives the scratch root its volume: found in the Data volume's container or made there, named
/// in the file system table, and mounted on the scratch root.
fn volume(env: &Env<'_>, params: &Params, scratch: &Path) -> Result<(), Done> {
    let data = plist_of(env, params, &["info", "-plist", DATA_VOLUME])?;
    let container = text(params, &data, "APFSContainerReference", DATA_VOLUME)?.to_owned();
    let found = match listed(env, params, &container)? {
        (_, Some(found)) => found,
        (free, None) => {
            let quota = format!("{}G", free / 10 * QUOTA_TENTHS / 1_000_000_000);
            let add: [&str; 8] = [
                "apfs",
                "addVolume",
                &container,
                "APFS",
                SCRATCH_VOLUME,
                "-quota",
                &quota,
                "-nomount",
            ];
            env.ports.point();
            command(env, params, DISKUTIL, &add)?;
            listed(env, params, &container)?.1.ok_or_else(|| {
                missing(
                    params,
                    format!(
                        "{DISKUTIL} does not list {SCRATCH_VOLUME} in {container} after it made it"
                    ),
                )
            })?
        }
    };
    persist(env, params, &found.uuid, scratch)?;
    let info = plist_of(env, params, &["info", "-plist", &found.device])?;
    if info
        .get("MountPoint")
        .and_then(Value::as_string)
        .is_some_and(|point| !point.is_empty())
    {
        env.ports.point();
        command(env, params, DISKUTIL, &["unmount", &found.device])?;
    }
    env.ports.point();
    let point = scratch.to_string_lossy();
    command(
        env,
        params,
        DISKUTIL,
        &["mount", "-mountPoint", &point, &found.device],
    )?;
    Ok(())
}

/// The free space of the APFS container `container` and the scratch volume in it, if it has one.
fn listed(env: &Env<'_>, params: &Params, container: &str) -> Result<(u64, Option<Volume>), Done> {
    let list = plist_of(env, params, &["apfs", "list", "-plist", container])?;
    let entry = list
        .get("Containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
        .find(|entry| entry.get("ContainerReference").and_then(Value::as_string) == Some(container))
        .ok_or_else(|| {
            missing(
                params,
                format!("{DISKUTIL} apfs list does not list the container {container}"),
            )
        })?;
    let free = entry
        .get("CapacityFree")
        .and_then(Value::as_unsigned_integer)
        .ok_or_else(|| {
            missing(
                params,
                format!("{DISKUTIL} shows the container {container} with no CapacityFree"),
            )
        })?;
    let volume = entry
        .get("Volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
        .find(|volume| volume.get("Name").and_then(Value::as_string) == Some(SCRATCH_VOLUME))
        .map(|volume| -> Result<Volume, Done> {
            Ok(Volume {
                uuid: text(params, volume, "APFSVolumeUUID", SCRATCH_VOLUME)?.to_owned(),
                device: text(params, volume, "DeviceIdentifier", SCRATCH_VOLUME)?.to_owned(),
            })
        })
        .transpose()?;
    Ok((free, volume))
}

/// Names the volume `uuid` in the file system table, on the scratch root, unless the table holds
/// that line already. Any other line that names the volume (another mount point, other options)
/// is dropped, and the other lines are kept. The table is written whole, to a file beside it that
/// is then renamed over it.
fn persist(env: &Env<'_>, params: &Params, uuid: &str, scratch: &Path) -> Result<(), Done> {
    let table = match fs::read_to_string(env.fstab) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => String::new(),
        read => read.map_err(|why| io_missing(params, env.fstab, &why))?,
    };
    let name = format!("UUID={uuid}");
    let entry = format!("{name} {} apfs rw,nobrowse", scratch.display());
    if table.lines().any(|line| line == entry) {
        return Ok(());
    }
    let mut lines: Vec<&str> = table
        .lines()
        .filter(|line| line.split_whitespace().next() != Some(name.as_str()))
        .collect();
    lines.push(&entry);
    let text = format!("{}\n", lines.join("\n"));
    let temp = env.fstab.with_extension("js-agent");
    env.ports.point();
    write_file(&temp, &text)
        .and_then(|()| fs::rename(&temp, env.fstab))
        .map_err(|why| io_missing(params, env.fstab, &why))
}

/// Writes `text` to `path`, mode 0644, and syncs it.
fn write_file(path: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
}
