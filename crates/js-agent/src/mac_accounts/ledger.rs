//! The ledger: every account the pool ever made and what became of it, kept across restarts
//! and reboots.
//!
//! One append-only text file, `<state>/accounts`, one record per line, each written with a
//! single `write` and `fsync`ed before the pool acts on it:
//!
//! ```text
//! made kbf-acct-<n> <uid>
//! ready kbf-acct-<n>
//! lent kbf-acct-<n> <job>
//! held kbf-acct-<n> <step>
//! gone kbf-acct-<n>
//! ```
//!
//! `made` is written before anything of the account exists, so a crash after it leaves an
//! account the next open deletes (with none of it made, there is nothing to delete and the
//! proof holds). `ready` follows the record and the home. `lent` is written before the
//! account is returned to a job: an account is lent once, ever. An account made for a lend
//! goes from `made` straight to `lent`, so it is never `ready` for another lend to take. `held` says a deletion did
//! not complete, with the step it stopped at; the uid stays out of use. `gone` is written
//! only once the account was deleted and shown gone: the uid is free from then on, not
//! before.
//!
//! The next account's number is the highest ever made plus one, so a name is never used
//! again; the uid of the last `made` record is the one the next uid is chosen after, so a
//! uid comes back as late as the range allows. No state beyond this file is kept.
//!
//! A crash can leave a last line without its newline. Its action never started (the write
//! comes first), so on opening the fragment is cut off. Any other line that does not parse,
//! or does not follow from the lines before it, refuses the open: the ledger is root's own
//! file, and guessing past corruption could hand out a uid twice.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use super::{MAX_NAME, PREFIX};

/// The ledger's file name in the state directory.
pub(super) const FILE: &str = "accounts";

/// The most digits a `u64` has: the longest number a name holds.
const MAX_DIGITS: usize = 20;
const _: () = assert!(PREFIX.len() + MAX_DIGITS <= MAX_NAME);

/// The longest step name a `held` record keeps.
const MAX_STEP: usize = 40;

/// The name of account number `n`: `kbf-acct-<n>`, decimal, at most 29 bytes.
pub(super) fn name(n: u64) -> String {
    format!("{PREFIX}{n}")
}

/// The number of the account named `text`, `None` for any other name: one spelling per
/// number (no sign, no leading zero).
pub(super) fn parse_name(text: &str) -> Option<u64> {
    let digits = text.strip_prefix(PREFIX)?;
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether `job` is a job id: one to 63 of `a-z`, `0-9` and `-`.
pub(super) fn valid_job(job: &str) -> bool {
    (1..=63).contains(&job.len())
        && job
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `step` is a step name as [`super::Step`] spells it: kebab-case.
pub(super) fn valid_step(step: &str) -> bool {
    (1..=MAX_STEP).contains(&step.len())
        && step
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Where an account is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    /// Recorded; its home and record may be half made.
    Made,
    /// Made, and not lent.
    Ready,
    /// Lent to a job, which is running or ended.
    Lent,
    /// A deletion did not complete.
    Held,
    /// Deleted and shown gone.
    Gone,
}

impl State {
    fn word(self) -> String {
        format!("{self:?}").to_lowercase()
    }
}

/// One account's entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    /// Its uid.
    pub(super) uid: u32,
    /// Where it is in its life.
    pub(super) state: State,
}

/// One line of the ledger, by the number of the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Record {
    Made(u64, u32),
    Ready(u64),
    Lent(u64, String),
    Held(u64, String),
    Gone(u64),
}

impl Record {
    fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split(' ').collect();
        let account = |text: &str| parse_name(text).ok_or_else(|| format!("bad account {text:?}"));
        match words.as_slice() {
            ["made", n, uid] => Ok(Self::Made(
                account(n)?,
                uid.parse().map_err(|_| format!("bad uid {uid:?}"))?,
            )),
            ["ready", n] => Ok(Self::Ready(account(n)?)),
            ["lent", n, job] => Ok(Self::Lent(account(n)?, (*job).to_owned())),
            ["held", n, step] => Ok(Self::Held(account(n)?, (*step).to_owned())),
            ["gone", n] => Ok(Self::Gone(account(n)?)),
            _ => Err(format!("unknown record {line:?}")),
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Made(n, uid) => write!(f, "made {} {uid}", name(*n)),
            Self::Ready(n) => write!(f, "ready {}", name(*n)),
            Self::Lent(n, job) => write!(f, "lent {} {job}", name(*n)),
            Self::Held(n, step) => write!(f, "held {} {step}", name(*n)),
            Self::Gone(n) => write!(f, "gone {}", name(*n)),
        }
    }
}

/// Failures a test injects into the next append, once each.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct Inject {
    /// Writes this many bytes of the record, then fails: a fragment is left.
    pub(super) write: Option<usize>,
    /// Fails the sync that follows a complete write.
    pub(super) sync: bool,
    /// Fails the cut back to the committed length.
    pub(super) roll_back: bool,
}

/// The ledger file and what it says.
#[derive(Debug)]
pub(super) struct Ledger {
    file: File,
    /// The length of the file as far as it is committed: every byte before it is a record
    /// that was written and synced. A failed append cuts the file back to it.
    committed: u64,
    /// Set when a failed append could not be cut back: the file may hold a fragment, so
    /// every later append is refused until the ledger is opened again.
    poisoned: bool,
    /// Failures to inject into the next append.
    #[cfg(test)]
    pub(super) inject: Inject,
    entries: BTreeMap<u64, Entry>,
    /// The uid of the last `made` record: the next uid is chosen after it.
    last_uid: Option<u32>,
}

/// Opens (creating, mode 0600, never through a link) the file at `path` and reads it whole,
/// cutting off a last line a crash left without its newline. Returns the file and its
/// complete lines.
fn open_records(path: &Path) -> io::Result<(File, String)> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let whole = text.rfind('\n').map_or(0, |i| i + 1);
    if whole < text.len() {
        tracing::warn!(
            fragment = &text[whole..],
            "dropping an account ledger record cut short by a crash"
        );
        file.set_len(whole as u64)?;
        file.sync_all()?;
    }
    text.truncate(whole);
    Ok((file, text))
}

/// `need`: `found` is one of `allowed`.
fn need(n: u64, found: Option<State>, allowed: &[State], what: &str) -> Result<(), String> {
    match found {
        Some(state) if allowed.contains(&state) => Ok(()),
        Some(state) => Err(format!(
            "{} is {}, so it cannot be {what}",
            name(n),
            state.word()
        )),
        None => Err(format!(
            "{} was never made, so it cannot be {what}",
            name(n)
        )),
    }
}

impl Ledger {
    /// Opens (creating, mode 0600) and reads the ledger at `path`.
    ///
    /// # Errors
    /// The file cannot be opened, read or trimmed, or a complete line does not parse or
    /// does not follow from the lines before it (the error names the line).
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let (file, text) = open_records(path)?;
        let mut ledger = Self {
            file,
            committed: text.len() as u64,
            poisoned: false,
            #[cfg(test)]
            inject: Inject::default(),
            entries: BTreeMap::new(),
            last_uid: None,
        };
        for (number, line) in text.lines().enumerate() {
            let record = Record::parse(line)
                .and_then(|record| ledger.admit(&record).map(|()| record))
                .map_err(|why| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("line {}: {why}", number + 1),
                    )
                })?;
            ledger.commit(&record);
        }
        Ok(ledger)
    }

    /// Whether `record` may follow what the ledger holds.
    fn admit(&self, record: &Record) -> Result<(), String> {
        use State::{Held, Lent, Made, Ready};
        let state = |n: &u64| self.entries.get(n).map(|entry| entry.state);
        match record {
            Record::Made(n, _) if state(n).is_some() => Err(format!("{} was made twice", name(*n))),
            Record::Made(..) => Ok(()),
            Record::Ready(n) => need(*n, state(n), &[Made], "ready"),
            Record::Lent(_, job) if !valid_job(job) => Err(format!("bad job id {job:?}")),
            Record::Lent(n, _) => need(*n, state(n), &[Made, Ready], "lent"),
            Record::Held(_, step) if !valid_step(step) => Err(format!("bad step {step:?}")),
            Record::Held(n, _) => need(*n, state(n), &[Made, Ready, Lent, Held], "held"),
            Record::Gone(n) => need(*n, state(n), &[Made, Ready, Lent, Held], "gone"),
        }
    }

    /// Applies `record`, which was admitted.
    fn commit(&mut self, record: &Record) {
        let set = |ledger: &mut Self, n: &u64, state: State| {
            ledger
                .entries
                .entry(*n)
                .and_modify(|entry| entry.state = state);
        };
        match record {
            Record::Made(n, uid) => {
                let entry = Entry {
                    uid: *uid,
                    state: State::Made,
                };
                self.entries.insert(*n, entry);
                self.last_uid = Some(*uid);
            }
            Record::Ready(n) => set(self, n, State::Ready),
            Record::Lent(n, _) => set(self, n, State::Lent),
            Record::Held(n, _) => set(self, n, State::Held),
            Record::Gone(n) => set(self, n, State::Gone),
        }
    }

    /// Writes `record` and its newline in one `write`, `fsync`s, then applies it. A write or
    /// sync that fails is rolled back: the file is cut to its committed length (a fragment a
    /// later append would join into a garbage line goes, and so does a complete record the
    /// memory does not know) and synced. When that fails too the ledger is poisoned.
    ///
    /// # Errors
    /// The record does not follow from what the ledger holds (nothing is written), the write
    /// or the `fsync` failed (nothing changes in memory, and the file is as it was), or the
    /// ledger is poisoned.
    pub(super) fn append(&mut self, record: &Record) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "the ledger is poisoned: an earlier failed write could not be rolled back, \
                 so it takes no record until the pool is opened again",
            ));
        }
        self.admit(record)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
        let line = format!("{record}\n");
        if let Err(why) = self.write(line.as_bytes()).and_then(|()| self.sync()) {
            return Err(match self.roll_back() {
                Ok(()) => why,
                Err(undo) => {
                    self.poisoned = true;
                    io::Error::other(format!(
                        "{why}; cutting the file back also failed ({undo}), so the ledger is poisoned"
                    ))
                }
            });
        }
        self.committed += line.len() as u64;
        self.commit(record);
        Ok(())
    }

    /// Writes `bytes` at the end of the file.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if let Some(part) = self.inject.write.take() {
            self.file.write_all(&bytes[..part])?;
            return Err(io::Error::other("injected: the write failed"));
        }
        self.file.write_all(bytes)
    }

    /// Syncs the file.
    fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.inject.sync) {
            return Err(io::Error::other("injected: the sync failed"));
        }
        self.file.sync_all()
    }

    /// Cuts the file back to its committed length and syncs it.
    fn roll_back(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.inject.roll_back) {
            return Err(io::Error::other("injected: the cut failed"));
        }
        self.file.set_len(self.committed)?;
        self.file.sync_all()
    }

    /// Makes every later write fail, as a full or failing disk would.
    #[cfg(test)]
    pub(super) fn fail_writes(&mut self, path: &Path) {
        self.file = File::open(path).unwrap();
    }

    /// The number the next account gets: one more than any ever made.
    pub(super) fn next(&self) -> u64 {
        self.entries.keys().next_back().map_or(1, |n| n + 1)
    }

    /// The entry of account `n`.
    pub(super) fn get(&self, n: u64) -> Option<Entry> {
        self.entries.get(&n).copied()
    }

    /// The uid of the last `made` record: the next uid is chosen after it.
    pub(super) fn last_uid(&self) -> Option<u32> {
        self.last_uid
    }

    /// The number of the account that has `uid` and is not gone.
    pub(super) fn holding(&self, uid: u32) -> Option<u64> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.uid == uid && entry.state != State::Gone)
            .map(|(n, _)| *n)
    }

    /// The oldest account in `state`: the one with the lowest number.
    pub(super) fn first_in(&self, state: State) -> Option<(u64, Entry)> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.state == state)
            .map(|(n, entry)| (*n, *entry))
    }

    /// The accounts that are not gone, by number.
    pub(super) fn live(&self) -> Vec<(u64, Entry)> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.state != State::Gone)
            .map(|(n, entry)| (*n, *entry))
            .collect()
    }

    /// The accounts in `state`, by number.
    pub(super) fn in_state(&self, state: State) -> Vec<(u64, Entry)> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.state == state)
            .map(|(n, entry)| (*n, *entry))
            .collect()
    }
}

#[cfg(test)]
mod tests;
