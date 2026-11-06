//! `js-mac-session`: the root helper that lends every Mac lease a user of its pool: the
//! verbs `user-create` (a free pool user for the lease), `run` (in the background, or in
//! the lease user's GUI session), `kill-uid` and `user-delete` (the lease ends: its user
//! is reset for the next lease, or replaced), `pool-create` and `pool-status`, the GUI
//! session switch (`session-login`, `session-ready`, `session-idle`), the boot reset
//! (which also makes the pool), and the leak scan's `scan`, `baseline` and
//! `reboot-dirty` over a minimal item set.
//!
//! - [`lease`]: lease ids, the pool's slots, user names (`kbf-lease-pool-<slot>`) and the
//!   uid range.
//! - [`grant`]: the MDM gate's signed admin grant.
//! - [`ledger`]: lease ids used and the state of each pool slot (free, leased,
//!   resetting, broken), kept across reboots, so grants are used once and a user is lent
//!   to one lease at a time.
//! - [`limits`]: the file-size and process limits a `run` asks the helper to set.
//! - [`helper`]: the verbs' rules, over a [`helper::Host`] that does the OS's part.
//! - [`autologin`]: the session switch's intent record, `/etc/kcpassword`, the login
//!   window's preference and the restart.
//! - [`sweep`]: what `user-delete` and a pool user's reset remove, never following a
//!   link.
//! - [`scan`]: the leak scan's items, its baseline and the baseline window.
//! - [`spawn`]: starting a process as the lease user.
//! - [`disclaim`]: the one way this crate and `js-agent` start a program; on macOS with
//!   its privacy (TCC) responsibility disclaimed, so it never holds the helper's or the
//!   agent's grants.
//! - [`proto`], [`server`], [`client`]: the socket, its wire format, the caller check.
//! - [`start`]: flags and start-up.
//! - [`node_admin`]: the `node-admin` mode, the verbs a node's administrator runs
//!   through `sudo` (`phase0` and `role-user` so far), behind its own guards.
//! - `macos`: the host (OpenDirectory, launchd, libproc), the caller check by code
//!   signature, and the `as-user` mode a GUI run executes (`macos::gui`), on macOS
//!   only.
//!
//! Everything but `macos` builds and is tested on every platform; the binary serves
//! only on macOS.
//!
//! The crate was moved here from the kbf repository (`kbf-mac-session`) without a change
//! to what goes over its socket or onto its disk: the protocol, the ledger, the account
//! names (`kbf-lease-<term>-<seq>` for a lease from before the pool, the role account
//! `_kbf`), the default paths and the
//! grant and scan-baseline headers keep the names they had, because the daemons and
//! installers that use them name them. The program name in the diagnostics (`kbf-mac-session`)
//! is kept too, although the binary file, and so the usage text, is now named
//! `js-mac-session`. The `S<n>` and `fleet-updates` references in comments are sections
//! of design documents of the kbf repository that are not part of this one.

pub mod autologin;
pub mod client;
pub mod disclaim;
pub mod grant;
pub mod helper;
pub mod lease;
pub mod ledger;
pub mod limits;
pub mod node_admin;
pub mod proto;
pub mod scan;
pub mod server;
pub mod spawn;
pub mod start;
pub mod sweep;
mod xattr;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(test)]
mod testing;
