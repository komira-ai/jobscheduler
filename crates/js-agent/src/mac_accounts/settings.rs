//! The pool's fixed configuration and the places it sweeps.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::str::FromStr;

use super::AccountError;

/// The uids accounts get: a range the agent is given, with no default. The pool never
/// touches a uid outside it, so the range must be one nothing else on the Mac uses (an earlier
/// release's accounts, for one).
///
/// macOS gives the uids up to 500 to the system and to the first user, so a range starts at
/// [`Slots::LOWEST`] or above; it ends at [`Slots::HIGHEST`] or below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slots {
    first: u32,
    last: u32,
}

impl Slots {
    /// The lowest uid a range may start at: macOS gives the uids up to 500 to the system and
    /// to the first user.
    pub const LOWEST: u32 = 501;
    /// The highest uid a range may end at.
    pub const HIGHEST: u32 = 60_000;

    /// The `count` uids from `first` on.
    ///
    /// # Errors
    /// [`AccountError::BadSlots`]: `count` is 0, `first` is below [`Slots::LOWEST`], the last
    /// uid is above [`Slots::HIGHEST`] or does not fit a uid at all.
    pub fn new(first: u32, count: u32) -> Result<Self, AccountError> {
        let Some(span) = count.checked_sub(1) else {
            return Err(bad("the count is 0, so there is no slot".to_owned()));
        };
        let Some(last) = first.checked_add(span) else {
            return Err(bad(format!(
                "{count} slots from uid {first} go past the largest uid"
            )));
        };
        Self::between(first, last)
    }

    /// The uids `first` to `last`, both included.
    fn between(first: u32, last: u32) -> Result<Self, AccountError> {
        if first > last {
            return Err(bad(format!(
                "the first uid, {first}, is above the last, {last}"
            )));
        }
        if first < Self::LOWEST {
            return Err(bad(format!(
                "the first uid, {first}, is below {}: macOS keeps the uids up to 500 for the \
                 system and the first user",
                Self::LOWEST
            )));
        }
        if last > Self::HIGHEST {
            return Err(bad(format!(
                "the last uid, {last}, is above {}",
                Self::HIGHEST
            )));
        }
        Ok(Self { first, last })
    }

    /// The uids, first to last.
    #[must_use]
    pub const fn range(self) -> RangeInclusive<u32> {
        self.first..=self.last
    }

    /// How many uids there are, at least 1.
    #[must_use]
    pub const fn count(self) -> u32 {
        self.last - self.first + 1
    }
}

impl FromStr for Slots {
    type Err = AccountError;

    /// Reads `<first>-<last>`, both uids and both included: `700-799` is 100 slots.
    ///
    /// # Errors
    /// [`AccountError::BadSlots`]: not two uids joined by `-`, or a range [`Slots::new`]
    /// refuses.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let uids = text
            .split_once('-')
            .and_then(|(first, last)| Some((first.parse().ok()?, last.parse().ok()?)));
        match uids {
            Some((first, last)) => Self::between(first, last),
            None => Err(bad(format!("{text:?} is not <first>-<last>, two uids"))),
        }
    }
}

/// The refusal of a range, saying `why`.
fn bad(why: String) -> AccountError {
    AccountError::BadSlots(why)
}

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

/// What the pool is configured with. [`Settings::macos`] is what a Mac runs with, given its
/// slots; the tests give their own paths and uids.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The uids accounts get ([`Slots::range`] on a Mac).
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
    /// A Mac's settings with `ready` accounts kept ready: the uids of `slots`, group staff,
    /// homes in `/Users`, a state directory root owns.
    #[must_use]
    pub fn macos(slots: Slots, ready: usize) -> Self {
        Self {
            range: slots.range(),
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

#[cfg(test)]
mod tests;
