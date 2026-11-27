//! The checks of the macOS agent's start that read what the host's neighbours provide, for the
//! update hook (see [`neighbours`]). Kept beside `macos.rs`, which holds the start itself.

use crate::cli::Cli;
use crate::native::{self, PutFlags};

use super::macos::{jobs, node_of_name};

/// The checks of the agent's start that read what the host's neighbours provide, on the flags
/// `cli`: the mount roots (`--mount-root`, `--mount-root-ro`), the directory and the user of each
/// `--mount-provider`, the trees root (`--native-trees-root`), and the put root and its user
/// (`--native-put-root`, `--native-put-user`): tierstore's mount, socket, put directory and
/// account on a Mac of the farm. They are the start's own checks ([`jobs`](super::macos::jobs),
/// [`crate::lifecycle::MountRoots::with_read_only`], [`native::put_of`]), which read and change
/// nothing; the update hook's `prepare` runs them on the host's flags file, so that an update
/// whose agent would refuse to start is refused before anything is stopped.
///
/// # Errors
///
/// The first refusal, as the start words it (the text names the flag).
pub(crate) fn neighbours(cli: &Cli) -> Result<(), String> {
    let mounts =
        crate::lifecycle::MountRoots::with_read_only(&cli.mount_roots, &cli.mount_roots_ro)?;
    jobs(cli, None, &mounts)?;
    let put = PutFlags {
        native_put_root: cli.os.native_put_root.clone(),
        native_put_user: cli.os.native_put_user.clone(),
    };
    native::put_of(&put, &node_of_name)?;
    Ok(())
}
