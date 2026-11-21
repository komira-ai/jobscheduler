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
//!   the state root, as long as they do not grow into it themselves. A volume that is there
//!   already is used, not made again. The volume is made unmounted (`diskutil apfs addVolume
//!   ... -nomount`) and then mounted on the scratch root (`diskutil mount nobrowse -mountPoint`);
//!   one that is mounted elsewhere is unmounted first (see
//!   [`scratch_volume`](crate::lifecycle::scratch_volume)). Nothing names it in the file system
//!   table, which macOS does not let the hook write: the agent mounts it again at each of its
//!   starts, which is what makes a boot find it on the scratch root. A scratch root its user
//!   does not own (a volume that ignores ownership) is given ownership (`diskutil
//!   enableOwnership`) before it is made the hook's user's again. Last, the
//!   copy of the file system table that an earlier release's setup wrote beside it and could
//!   not rename over it ([`FSTAB_LEFTOVER`]) is removed: nothing reads it.
//!
//! Each part looks before it changes anything and makes only what is not as required (the
//! owner and mode of the two roots are set on every run), so a run killed at any point and run
//! again ends as an uninterrupted one; every change passes a kill point first. A part that
//! cannot be read or made ends the step deferred (75, `host_setup_missing`) with the part and
//! the cause named: no record is written for it and the host is not cordoned, and hostkeeper
//! orders the host again later. A system call that the system refuses (`EPERM`, which no retry
//! changes) ends the step failed instead (1, `host_setup_refused`, class environment), naming
//! the path and the error, so that the order fails and someone looks. A command (`dscl`,
//! `dseditgroup`, `diskutil`) is not told apart in that way: the runner reports how it exited,
//! not what it printed. The setup belongs to the host, not to a release: no step undoes it.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, chown};
use std::path::Path;

use js_mac_session::start::STAFF_GID;
use rustix::io::Errno;

use crate::lifecycle::scratch_volume::{self, VolumeError};
use crate::update_hook::{Class, Done, Params, Reason};

pub use crate::lifecycle::scratch_volume::{DATA_VOLUME, DISKUTIL, SCRATCH_VOLUME};

use super::prepare::deferred;
use super::{Env, PathFacts, failure};

/// The directory service's command line tool.
pub const DSCL: &str = "/usr/bin/dscl";

/// The tool that makes a group.
pub const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";

/// The scratch volume's quota, in tenths of its container's free space when it is made: a
/// ceiling on what jobs write there, so that the rest of that space stays for the system and the
/// state root unless they fill it themselves.
pub const QUOTA_TENTHS: u64 = 9;

/// The copy of the file system table that an earlier release's host setup wrote beside
/// `/etc/fstab` and then could not rename over it: the setup removes it.
pub const FSTAB_LEFTOVER: &str = "/etc/fstab.js-agent";

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

/// The ending for a path that cannot be read or made: failed when the system refuses the call
/// (`EPERM`), deferred otherwise.
fn io_ending(params: &Params, path: &Path, why: &io::Error) -> Done {
    let detail = format!("{}: {why}", path.display());
    if why.raw_os_error() == Some(Errno::PERM.raw_os_error()) {
        let mut done = failure(Reason::HostSetupRefused, detail);
        done.report.class = Class::Environment;
        return done;
    }
    missing(params, detail)
}

/// The deferral for an answer of `diskutil` the setup cannot use.
fn volume_missing(params: &Params) -> impl Fn(VolumeError) -> Done + '_ {
    move |why| missing(params, why.to_string())
}

/// Runs `program` with `args`: what it printed, or the deferral naming the command.
fn command(env: &Env<'_>, params: &Params, program: &str, args: &[&str]) -> Result<String, Done> {
    env.runner
        .run(program, args)
        .map_err(|why| missing(params, format!("{program} {}: {why}", args.join(" "))))
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
/// hook's user's, mode 0700; the leftover copy of the file system table is gone.
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
    own(env, params, scratch)?;
    clear_leftover(env, params)
}

/// Makes `dir` the hook's user's, mode 0700 (a link followed).
fn own(env: &Env<'_>, params: &Params, dir: &Path) -> Result<(), Done> {
    env.ports.point();
    chown(dir, Some(env.euid), None)
        .and_then(|()| fs::set_permissions(dir, Permissions::from_mode(0o700)))
        .map_err(|why| io_ending(params, dir, &why))
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
        .map_err(|why| io_ending(params, dir, &why))
}

/// What the look at `path` shows.
fn look(env: &Env<'_>, params: &Params, path: &Path) -> Result<PathFacts, Done> {
    (env.look)(path).map_err(|why| io_ending(params, path, &why))
}

/// Whether the scratch root is on the state root's file system.
fn shared(env: &Env<'_>, params: &Params, scratch: &Path) -> Result<bool, Done> {
    Ok(look(env, params, scratch)?.device == look(env, params, &params.state_root)?.device)
}

/// Gives the scratch root its volume: found in the Data volume's container or made there, and
/// mounted on the scratch root.
fn volume(env: &Env<'_>, params: &Params, scratch: &Path) -> Result<(), Done> {
    let found = scratch_volume::find(env.runner).map_err(volume_missing(params))?;
    let volume = match found.volume {
        Some(volume) => volume,
        None => {
            let quota = format!("{}G", found.free / 10 * QUOTA_TENTHS / 1_000_000_000);
            let container = found.reference.as_str();
            let add: [&str; 8] = [
                "apfs",
                "addVolume",
                container,
                "APFS",
                SCRATCH_VOLUME,
                "-quota",
                &quota,
                "-nomount",
            ];
            env.ports.point();
            command(env, params, DISKUTIL, &add)?;
            let listed = scratch_volume::listed(env.runner, container);
            listed.map_err(volume_missing(params))?.volume.ok_or_else(|| {
                missing(
                    params,
                    format!(
                        "{DISKUTIL} does not list {SCRATCH_VOLUME} in {container} after it made it"
                    ),
                )
            })?
        }
    };
    scratch_volume::mount_on(env.runner, &|| env.ports.point(), &volume, scratch)
        .map_err(volume_missing(params))
}

/// Removes [`FSTAB_LEFTOVER`] (the path the machine names) when it is there.
fn clear_leftover(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    let leftover = env.fstab_leftover;
    match fs::symlink_metadata(leftover) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(()),
        looked => looked.map_err(|why| io_ending(params, leftover, &why))?,
    };
    env.ports.point();
    fs::remove_file(leftover).map_err(|why| io_ending(params, leftover, &why))
}
