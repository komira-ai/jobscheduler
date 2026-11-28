//! `js-mac-session`: the root helper that lends every Mac lease a user of its pool: the
//! verbs `user-create` (a free pool user for the lease), `run` (outside any login
//! session), `watch` and `stop`, `kill-uid` and `user-delete` (the lease ends: its user
//! is reset in place for the next lease), `pool-create` and `pool-status`, the boot reset
//! (which also clears auto-login and makes the pool), the leak scan's `scan` over a
//! minimal item set, and `full-disk-access`.
//!
//! - [`lease`]: lease ids, the pool's slots, user names (`kbf-lease-pool-<slot>`) and the
//!   uid range.
//! - [`ledger`]: lease ids used and the state of each pool slot (free, leased,
//!   resetting, broken), kept across reboots, so a lease id is used once and a user is
//!   lent to one lease at a time.
//! - [`groups`]: the group list a process started as a lease user holds.
//! - [`limits`]: the file-size and process limits a `run` asks the helper to set.
//! - [`helper`]: the verbs' rules, over a [`helper::Host`] that does the OS's part.
//! - [`autologin`]: the login window's auto-login, which the boot reset clears, and the
//!   command runner and whole-file writes the rest share.
//! - [`sweep`]: what `user-delete` and a pool user's reset remove, never following a
//!   link.
//! - [`scan`]: the leak scan's items and its recorded baseline.
//! - [`spawn`]: starting a process as the lease user.
//! - [`disclaim`]: the one way this crate and `js-agent` start a program; on macOS with
//!   its privacy (TCC) responsibility disclaimed by default, so it does not hold the
//!   starter's grants; only a call of `Program::keep_responsibility` keeps it.
//! - [`full_disk_access`]: whether a process holds Full Disk Access, which `js-agent`
//!   judges of itself at start and the `full-disk-access` verb answers for the helper.
//! - [`proto`], [`server`], [`client`]: the socket, its wire format, the caller check.
//! - [`start`]: flags and start-up.
//! - `macos`: the host (OpenDirectory, launchd, libproc) and the caller check by code
//!   signature, on macOS only.
//!
//! Everything but `macos` builds and is tested on every platform; the binary serves
//! only on macOS.
//!
//! The crate was moved here from the kbf repository (`kbf-mac-session`) without a change
//! to what goes over its socket or onto its disk: the protocol, the ledger, the account
//! names (`kbf-lease-<term>-<seq>` for a lease from before the pool, the role account
//! `_kbf`), the default paths and the
//! scan-baseline header keep the names they had, because the daemons and
//! installers that use them name them. The program name in the diagnostics (`kbf-mac-session`)
//! is kept too, although the binary file, and so the usage text, is now named
//! `js-mac-session`. The `S<n>` and `fleet-updates` references in comments are sections
//! of design documents of the kbf repository that are not part of this one.
//!
//! Since the move, the verbs no code of this repository called are gone: the GUI session
//! switch (`session-login`, `session-ready`, `session-idle`, a `run` in the GUI session and
//! the `as-user` mode it executed), the standing VM user's `role-*` verbs, the image verbs
//! (`image-setup`, `image-harvest`), and the leak scan's `baseline` and `reboot-dirty`.
//! The helper refuses each as an unknown verb, and a `run` that names a `session` as an
//! unknown field.

pub mod autologin;
pub mod client;
pub mod disclaim;
pub mod full_disk_access;
pub mod groups;
pub mod helper;
pub mod lease;
pub mod ledger;
pub mod limits;
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
