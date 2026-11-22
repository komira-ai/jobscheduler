//! The pool's fixed configuration and the places it sweeps.

use std::ops::RangeInclusive;
use std::path::PathBuf;

/// The uids accounts get, `600..=699`. The pool never touches a uid outside the range it is
/// given. The egress rules of a Mac already cover this range.
pub const UID_RANGE: RangeInclusive<u32> = 600..=699;

/// The primary group of every account: `staff`.
pub const STAFF: u32 = 20;

/// Entries named after an account, and directories searched for what a uid owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Places {
    /// Single entries, removed whole whoever owns them: `{name}` and `{uid}` stand for
    /// the account's name and uid. The directory that holds each is a fixed, root-owned
    /// path.
    pub named: Vec<String>,
    /// Directories in which every entry the uid owns is removed, at any depth: never
    /// through a link, never past the filesystem of the directory.
    pub owned: Vec<PathBuf>,
}

impl Places {
    /// Where macOS starts a process of a user with no session of its own: the crontab and
    /// the `at` jobs (`/usr/lib/cron` is a link to `/private/var/at`).
    #[must_use]
    pub fn macos_schedules() -> Self {
        Self {
            named: vec!["/private/var/at/tabs/{name}".to_owned()],
            owned: vec![PathBuf::from("/private/var/at/jobs")],
        }
    }

    /// The places every user shares, besides the home and the per-user folders: the shared
    /// user folder, the temporary folders, and launchd's per-uid override and login-item
    /// files.
    #[must_use]
    pub fn macos_shared() -> Self {
        Self {
            named: [
                "/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist",
                "/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist",
            ]
            .map(str::to_owned)
            .to_vec(),
            owned: ["/Users/Shared", "/private/tmp", "/private/var/tmp"]
                .map(PathBuf::from)
                .to_vec(),
        }
    }

    /// These places and `other`'s.
    #[must_use]
    pub(super) fn with(&self, other: &Self) -> Self {
        Self {
            named: [self.named.as_slice(), other.named.as_slice()].concat(),
            owned: [self.owned.as_slice(), other.owned.as_slice()].concat(),
        }
    }
}

/// What the pool is configured with. [`Settings::macos`] is what a Mac runs with; the tests
/// give their own paths and uids.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The uids accounts get.
    pub range: RangeInclusive<u32>,
    /// The primary group of every account.
    pub gid: u32,
    /// The directory the homes are made in, `<homes>/<name>`.
    pub homes: PathBuf,
    /// How many accounts are kept ready.
    pub ready: usize,
    /// The per-user folders, `<folders>/<xx>/<hash>` (`/private/var/folders`).
    pub user_folders: PathBuf,
    /// The crontab and `at` jobs.
    pub schedules: Places,
    /// The places every user shares.
    pub shared: Places,
    /// The uid that must own the state directory; `None` takes any owner.
    pub state_owner: Option<u32>,
}

impl Settings {
    /// A Mac's settings with `ready` accounts kept ready: uids 600 to 699, group staff,
    /// homes in `/Users`, a state directory root owns.
    #[must_use]
    pub fn macos(ready: usize) -> Self {
        Self {
            range: UID_RANGE,
            gid: STAFF,
            homes: PathBuf::from("/Users"),
            ready,
            user_folders: PathBuf::from("/private/var/folders"),
            schedules: Places::macos_schedules(),
            shared: Places::macos_shared(),
            state_owner: Some(0),
        }
    }

    /// Every place the sweep covers but the home and the per-user folders.
    #[must_use]
    pub(super) fn swept_places(&self) -> Places {
        self.schedules.with(&self.shared)
    }
}
