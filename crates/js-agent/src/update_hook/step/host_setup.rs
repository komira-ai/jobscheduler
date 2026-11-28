//! The host setup of a Mac, part of `prepare`: what the package's daemons need of the host and no
//! file of a package can carry, made by the hook, which hostkeeper runs as root, for each part
//! the params name. hostkeeper is the only way onto the Macs, so nothing here is left to an
//! operator.
//!
//! - `scratch_root`: the agent's `--scratch-root`, which must be on another file system than its
//!   `--state-root` (the agent refuses a shared `st_dev`). The scratch volume is not mounted on
//!   the scratch root but on its parent, the scratch volume's mount point, so that the bundle
//!   store can lie beside the scratch root on the same volume (the agent's start sweeps every
//!   entry of the scratch root that is not a run's directory, and a lease user must be able to
//!   walk down to the store, which a private mount point would not let it). The state root, the
//!   mount point and the scratch root are made when they are not there, with each missing
//!   directory above them (mode 0755), and on every run are made the hook's user's: the state
//!   root and the scratch root mode 0700, as the agent requires of them, and the mount point
//!   mode [`MOUNT_POINT_MODE`] (0711, search only for others). The hook makes no store
//!   directory: the agent makes it at its start. While the mount point shares a device with the
//!   state root it is given an APFS volume of its own, [`SCRATCH_VOLUME`], in the container of
//!   the Data volume ([`DATA_VOLUME`]), with a quota of [`QUOTA_TENTHS`] tenths of the
//!   container's free space when it is made; a Mac whose volume is mounted on the mount point
//!   already needs no mount. The quota is a ceiling, not a reservation: jobs that fill the
//!   volume leave the rest of that free space to the system and the state root, as long as they
//!   do not grow into it themselves. A volume that is there already is used, not made again.
//!   The volume is made unmounted (`diskutil apfs addVolume ... -nomount`) and then mounted on
//!   the mount point (`diskutil mount nobrowse -mountPoint`); one that is mounted elsewhere is
//!   unmounted first (see [`scratch_volume`](crate::lifecycle::scratch_volume)). The scratch
//!   root is made once the volume is mounted, so that it lies on the volume. Nothing names the
//!   volume in the file system table, which macOS does not let the hook write: the agent mounts
//!   it again at each of its starts, which is what makes a boot find it on the mount point. A
//!   mount point its user does not own (a volume that ignores ownership) is given ownership
//!   (`diskutil enableOwnership`) before it is made the hook's user's again. Last, the copy of
//!   the file system table that an earlier release's setup wrote beside it and could not rename
//!   over it ([`FSTAB_LEFTOVER`]) is removed: nothing reads it.
//!
//! Each part looks before it changes anything and makes only what is not as required (the
//! owner and mode of the two roots are set on every run), so a run killed at any point and run
//! again ends as an uninterrupted one; every change passes a kill point first. A part that
//! cannot be read or made ends the step deferred (75, `host_setup_missing`) with the part and
//! the cause named: no record is written for it and the host is not cordoned, and hostkeeper
//! orders the host again later. A system call that the system refuses (`EPERM`, which no retry
//! changes) ends the step failed instead (1, `host_setup_refused`, class environment), naming
//! the path and the error, so that the order fails and someone looks. A command (`diskutil`) is
//! not told apart in that way: the runner reports how it exited, not what it printed. The setup
//! belongs to the host, not to a release: no step undoes it.
//!
//! The setup is a launchd matter: the params of a systemd host that name the scratch root are
//! refused by every step (`bad_params`) before anything is read or done
//! ([`refuse_without_launchd`]).

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, chown};
use std::path::Path;

use rustix::io::Errno;

use crate::lifecycle::scratch_volume::{self, VolumeError};
use crate::update_hook::{Class, Done, Manager, Params, Reason, SCRATCH_ROOT_KEY};

pub use crate::lifecycle::scratch_volume::{DATA_VOLUME, DISKUTIL, SCRATCH_VOLUME};

use super::prepare::deferred;
use super::{Env, PathFacts, failure};

/// The mode of the scratch volume's mount point: search only for others, so that a lease user
/// can walk down through it to the bundle store beside the scratch root and cannot list it.
pub const MOUNT_POINT_MODE: u32 = 0o711;

/// The scratch volume's quota, in tenths of its container's free space when it is made: a
/// ceiling on what jobs write there, so that the rest of that space stays for the system and the
/// state root unless they fill it themselves.
pub const QUOTA_TENTHS: u64 = 9;

/// The copy of the file system table that an earlier release's host setup wrote beside
/// `/etc/fstab` and then could not rename over it: the setup removes it.
pub const FSTAB_LEFTOVER: &str = "/etc/fstab.js-agent";

/// Refuses a params table that names a part of a Mac's host setup (the scratch root) on a host
/// that is not under launchd.
pub(super) fn refuse_without_launchd(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    match (&params.scratch_root, env.manager) {
        (Some(root), Manager::Systemd) => Err(failure(
            Reason::BadParams,
            format!(
                "{SCRATCH_ROOT_KEY} {:?} is set, and only a launchd host's setup gives it a volume",
                root.display().to_string()
            ),
        )),
        _ => Ok(()),
    }
}

/// Makes each part of the host setup the params name: the roots.
pub(super) fn set_up(env: &Env<'_>, params: &Params) -> Result<(), Done> {
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

/// The state root, the scratch volume's mount point (the scratch root's parent) and the scratch
/// root are there: the state root and the scratch root are the hook's user's, mode 0700, the
/// mount point is the hook's user's, mode [`MOUNT_POINT_MODE`], and the scratch root is on the
/// scratch volume, a file system of its own; the leftover copy of the file system table is gone.
/// The scratch root is made after the mount, so that it lies on the volume and not below it.
fn roots(env: &Env<'_>, params: &Params, scratch: &Path) -> Result<(), Done> {
    // The params refuse a scratch root that is `/` (the state root is inside it), so it has a
    // parent.
    let mount = scratch.parent().unwrap_or(scratch);
    make_dir(env, params, &params.state_root)?;
    own(env, params, &params.state_root, 0o700)?;
    make_dir(env, params, mount)?;
    if shared(env, params, mount)? {
        volume(env, params, mount)?;
        if shared(env, params, mount)? {
            return Err(missing(
                params,
                format!(
                    "the scratch root {} is on the file system of the state root {} after {SCRATCH_VOLUME} was mounted on {}",
                    scratch.display(),
                    params.state_root.display(),
                    mount.display()
                ),
            ));
        }
    }
    let mount_arg = mount.to_string_lossy();
    if look(env, params, mount)?.uid != env.euid {
        env.ports.point();
        command(env, params, DISKUTIL, &["enableOwnership", &mount_arg])?;
    }
    own(env, params, mount, MOUNT_POINT_MODE)?;
    make_dir(env, params, scratch)?;
    own(env, params, scratch, 0o700)?;
    clear_leftover(env, params)
}

/// Makes `dir` the hook's user's, mode `mode` (a link followed).
fn own(env: &Env<'_>, params: &Params, dir: &Path, mode: u32) -> Result<(), Done> {
    env.ports.point();
    chown(dir, Some(env.euid), None)
        .and_then(|()| fs::set_permissions(dir, Permissions::from_mode(mode)))
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

/// Whether `dir` is on the state root's file system.
fn shared(env: &Env<'_>, params: &Params, dir: &Path) -> Result<bool, Done> {
    Ok(look(env, params, dir)?.device == look(env, params, &params.state_root)?.device)
}

/// Gives the scratch root its volume: found in the Data volume's container or made there, and
/// mounted on `mount`, the scratch root's parent.
fn volume(env: &Env<'_>, params: &Params, mount: &Path) -> Result<(), Done> {
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
    scratch_volume::mount_on(env.runner, &|| env.ports.point(), &volume, mount)
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
