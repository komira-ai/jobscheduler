//! The ledger: every lease id the helper ever gave a user to, and the state of each
//! user of the pool, kept across reboots.
//!
//! Two append-only text files in the helper's state directory, one record per line, each
//! written with a single `write` and then `fsync`ed before the helper acts on it.
//!
//! `ledger`, the lease ids. An id is used once: a second `user-create` for it is
//! refused, so a gate grant (which names one lease) is single-use. It holds the uid each
//! lease got, so `run`, `kill-uid` and `user-delete` act on the uid the ledger names
//! rather than on whatever a directory lookup says:
//!
//! ```text
//! created <term>.<seq> <uid>
//! deleted <term>.<seq>
//! ```
//!
//! `deleted` records that the lease is over: its pool user went back to the pool (or was
//! replaced), or, for a lease a release before the pool created, its own user was
//! deleted. This file's format is the one releases before the pool wrote and read, so
//! they can still read it.
//!
//! `pool` (the [`pool`] module), the state of each slot of the pool: free, leased (to
//! which lease), resetting or broken.
//!
//! A crash can leave a last line without its newline. That record's action never
//! started (it follows the `fsync`), so on opening the fragment is cut off. Any other
//! line that does not parse stops the helper from starting: the ledger is root's own
//! file, and guessing past corruption could reuse a lease id or hand out a user twice.
//!
//! When a lease takes a slot, the slot's `leased` record is written before the lease's
//! `created`; when it ends, `deleted` is written before the slot's `resetting`. So a crash
//! between the two never leaves a live lease without its slot: it leaves a slot leased
//! to a lease that is not live, which the boot reset resets.
//!
//! `ledger` grows by about 30 bytes per lease and `pool` by about 40, and neither is
//! compacted (a later change can drop records of ended leases older than any grant).

pub mod pool;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use js_types::LeaseId;

use crate::lease::{Dotted, Slot, parse_lease};
use pool::{Pool, SlotEntry, SlotState};

/// The pool's file name, beside the ledger's.
pub const POOL_FILE: &str = "pool";

/// One lease's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The uid the lease's user had.
    pub uid: u32,
    /// Whether the lease is over (its user returned to the pool, replaced, or deleted);
    /// a lease id that is over is never used again.
    pub deleted: bool,
}

/// The ledger's two files and what they say.
#[derive(Debug)]
pub struct Ledger {
    file: File,
    leases: BTreeMap<LeaseId, Entry>,
    pool: Pool,
}

/// Opens (creating, mode 0600, never through a link) the record file at `path` and
/// reads it whole, cutting off a last line a crash left without its newline. Returns
/// the file and its complete lines.
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
            "dropping a ledger record cut short by a crash"
        );
        file.set_len(whole as u64)?;
        file.sync_all()?;
    }
    text.truncate(whole);
    Ok((file, text))
}

/// The error for line `number` (from 0) of `path`, which `why` refuses.
fn bad_line(path: &Path, number: usize, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: line {}: {why}", path.display(), number + 1),
    )
}

/// Writes `line` and its newline to `file` in one `write`, then `fsync`s.
fn write_record(file: &mut File, line: &str) -> io::Result<()> {
    file.write_all(format!("{line}\n").as_bytes())
        .and_then(|()| file.sync_all())
}

/// A record refused before it was written.
fn refused(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

impl Ledger {
    /// Opens (creating, mode 0600) and reads the ledger at `path`, and the pool's file
    /// ([`POOL_FILE`]) beside it.
    ///
    /// # Errors
    /// A file cannot be opened, read or trimmed, or a complete line does not parse or
    /// does not follow from the lines before it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let (file, text) = open_records(path)?;
        let mut ledger = Self {
            file,
            leases: BTreeMap::new(),
            pool: Pool::open(&path.with_file_name(POOL_FILE))?,
        };
        for (number, line) in text.lines().enumerate() {
            let record = Record::parse(line)
                .and_then(|record| ledger.admit(record).map(|()| record))
                .map_err(|why| bad_line(path, number, &why))?;
            ledger.commit(record);
        }
        Ok(ledger)
    }

    /// Whether `record` may follow what the ledger holds.
    fn admit(&self, record: Record) -> Result<(), String> {
        match record {
            Record::Created(lease, _) if self.leases.contains_key(&lease) => {
                Err(format!("lease {} created twice", Dotted(lease)))
            }
            Record::Deleted(lease) if self.get(lease).is_none_or(|entry| entry.deleted) => {
                Err(format!("lease {} deleted but not live", Dotted(lease)))
            }
            _ => Ok(()),
        }
    }

    fn commit(&mut self, record: Record) {
        match record {
            Record::Created(lease, uid) => {
                let entry = Entry {
                    uid,
                    deleted: false,
                };
                self.leases.insert(lease, entry);
            }
            Record::Deleted(lease) => {
                // Admitted, so the lease is live.
                self.leases
                    .entry(lease)
                    .and_modify(|entry| entry.deleted = true);
            }
        }
    }

    /// Writes `record` and its newline in one `write`, `fsync`s, then applies it.
    fn append(&mut self, record: Record) -> io::Result<()> {
        self.admit(record).map_err(refused)?;
        write_record(&mut self.file, &record.to_string())?;
        self.commit(record);
        Ok(())
    }

    /// The lease's record, if it was ever created.
    #[must_use]
    pub fn get(&self, lease: LeaseId) -> Option<Entry> {
        self.leases.get(&lease).copied()
    }

    /// The leases created and not yet deleted, in order.
    #[must_use]
    pub fn live_leases(&self) -> Vec<LeaseId> {
        self.leases
            .iter()
            .filter(|(_, entry)| !entry.deleted)
            .map(|(lease, _)| *lease)
            .collect()
    }

    /// The uids of leases created and not yet deleted.
    #[must_use]
    pub fn held_uids(&self) -> BTreeSet<u32> {
        self.leases
            .values()
            .filter(|entry| !entry.deleted)
            .map(|entry| entry.uid)
            .collect()
    }

    /// Makes every later write fail, as a full or failing disk would: of the ledger's
    /// file, and of the pool's when `pool`.
    #[cfg(test)]
    pub(crate) fn fail_writes(&mut self, path: &Path, pool: bool) {
        self.file = File::open(path).unwrap();
        if pool {
            self.pool.fail_writes(&path.with_file_name(POOL_FILE));
        }
    }

    /// Makes every write to the pool's file after the next `writes` fail.
    #[cfg(test)]
    pub(crate) fn fail_pool_after(&mut self, writes: usize) {
        self.pool.fail_after(writes);
    }

    /// Records that `lease` got `uid`, durably.
    ///
    /// # Errors
    /// The lease was created before, or the write or `fsync` failed.
    pub fn record_created(&mut self, lease: LeaseId, uid: u32) -> io::Result<()> {
        self.append(Record::Created(lease, uid))
    }

    /// Records that `lease` is over, durably.
    ///
    /// # Errors
    /// The lease is not live, or the write or `fsync` failed.
    pub fn record_deleted(&mut self, lease: LeaseId) -> io::Result<()> {
        self.append(Record::Deleted(lease))
    }

    /// The slot's record, if the slot was ever made.
    #[must_use]
    pub fn slot(&self, slot: Slot) -> Option<SlotEntry> {
        self.pool.get(slot)
    }

    /// Every slot that was made, in order, with its record.
    #[must_use]
    pub fn slots(&self) -> Vec<(Slot, SlotEntry)> {
        self.pool.all()
    }

    /// The slot `lease` holds: the one leased to it.
    #[must_use]
    pub fn slot_of(&self, lease: LeaseId) -> Option<Slot> {
        self.pool.leased_to(lease)
    }

    /// Records, durably, that the user of `slot`, with `uid`, is being made: the slot is
    /// resetting until [`Self::record_free`] (or broken, [`Self::record_broken`]).
    ///
    /// # Errors
    /// The slot was made before, another slot has `uid`, or the write failed.
    pub fn record_slot(&mut self, slot: Slot, uid: u32) -> io::Result<()> {
        self.pool.append(pool::Record::Made(slot, uid))
    }

    /// Records, durably, that `lease` takes the free `slot` (as an administrator when
    /// `admin`): first the slot's `leased`, then the lease's `created` with the slot's
    /// uid.
    ///
    /// # Errors
    /// The lease was created before, the slot is not free, or a write failed (a failed
    /// `created` leaves the slot leased to a lease that is not live, which the boot reset
    /// resets).
    pub fn record_leased(&mut self, slot: Slot, lease: LeaseId, admin: bool) -> io::Result<()> {
        let Some(SlotEntry {
            uid,
            state: SlotState::Free,
        }) = self.slot(slot)
        else {
            return Err(refused(format!("slot {slot} is not free")));
        };
        self.admit(Record::Created(lease, uid)).map_err(refused)?;
        self.pool.append(pool::Record::Leased(slot, lease, admin))?;
        self.append(Record::Created(lease, uid))
    }

    /// Records, durably, that `lease`, which holds `slot`, is over: first the lease's
    /// `deleted`, then the slot's `resetting`.
    ///
    /// # Errors
    /// The lease is not live, `slot` is not leased to it, or a write failed (a failed
    /// `resetting` leaves the slot leased to a lease that is not live, which the boot
    /// reset resets).
    pub fn record_released(&mut self, slot: Slot, lease: LeaseId) -> io::Result<()> {
        self.admit(Record::Deleted(lease)).map_err(refused)?;
        self.pool.admit_reset(slot, Some(lease)).map_err(refused)?;
        self.append(Record::Deleted(lease))?;
        self.pool.append(pool::Record::Resetting(slot))
    }

    /// Records, durably, that `slot`, leased to a lease that is not live, is resetting.
    ///
    /// # Errors
    /// The slot is not leased, or is leased to a live lease, or the write failed.
    pub fn record_orphan_reset(&mut self, slot: Slot) -> io::Result<()> {
        if let Some(SlotEntry {
            state: SlotState::Leased { lease, .. },
            ..
        }) = self.slot(slot)
            && self.get(lease).is_some_and(|entry| !entry.deleted)
        {
            return Err(refused(format!(
                "slot {slot} is leased to lease {}, which is live",
                Dotted(lease)
            )));
        }
        self.pool.admit_reset(slot, None).map_err(refused)?;
        self.pool.append(pool::Record::Resetting(slot))
    }

    /// Records, durably, that `slot`'s user is clean (or new): the slot is free.
    ///
    /// # Errors
    /// The slot is neither resetting nor broken, or the write failed.
    pub fn record_free(&mut self, slot: Slot) -> io::Result<()> {
        self.pool.append(pool::Record::Free(slot))
    }

    /// Records, durably, that `slot`'s user could be neither reset nor replaced: the slot
    /// is broken, and leased to no one.
    ///
    /// # Errors
    /// The slot is not resetting, or the write failed.
    pub fn record_broken(&mut self, slot: Slot) -> io::Result<()> {
        self.pool.append(pool::Record::Broken(slot))
    }
}

/// One line of the ledger.
#[derive(Clone, Copy, Debug)]
enum Record {
    Created(LeaseId, u32),
    Deleted(LeaseId),
}

impl Record {
    fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split(' ').collect();
        match words.as_slice() {
            ["created", lease, uid] => Ok(Self::Created(
                parse_lease(lease)?,
                uid.parse().map_err(|_| format!("bad uid {uid:?}"))?,
            )),
            ["deleted", lease] => Ok(Self::Deleted(parse_lease(lease)?)),
            _ => Err(format!("unknown record {line:?}")),
        }
    }
}

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created(lease, uid) => write!(f, "created {} {uid}", Dotted(*lease)),
            Self::Deleted(lease) => write!(f, "deleted {}", Dotted(*lease)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log held in memory: what a test's subscriber writes to.
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Log {
        fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
            let log = self.clone();
            tracing_subscriber::fmt()
                .with_writer(move || {
                    let mut writer = log.clone();
                    std::io::Write::flush(&mut writer).unwrap();
                    writer
                })
                .with_ansi(false)
                .finish()
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn dir(name: &str) -> js_testkit::dir::TestDir {
        js_testkit::dir::TestDir::under(
            std::env::temp_dir(),
            &format!("kbf-mac-session-ledger-{name}"),
        )
    }

    /// Catches: a ledger kept only in memory, under which a restart (or reboot)
    /// forgets used lease ids and a grant or a name can be used again.
    #[test]
    fn records_survive_reopening() {
        let _path_own = dir("reopen");
        let path = _path_own.join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record_created(LeaseId::new(1, 1), 600).unwrap();
        ledger.record_created(LeaseId::new(1, 2), 601).unwrap();
        ledger.record_deleted(LeaseId::new(1, 1)).unwrap();
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(
            ledger.get(LeaseId::new(1, 1)),
            Some(Entry {
                uid: 600,
                deleted: true
            })
        );
        assert_eq!(
            ledger.get(LeaseId::new(1, 2)),
            Some(Entry {
                uid: 601,
                deleted: false
            })
        );
        assert_eq!(ledger.get(LeaseId::new(1, 3)), None);
        assert_eq!(ledger.held_uids(), BTreeSet::from([601]));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 1.1 600\ncreated 1.2 601\ndeleted 1.1\n");
    }

    /// Catches: a second `created` for one lease accepted (a reused name or grant),
    /// or a deletion of a lease that is not live.
    #[test]
    fn a_lease_is_created_once_and_deleted_once() {
        let _path_own = dir("once");
        let path = _path_own.join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record_created(LeaseId::new(2, 1), 600).unwrap();
        let again = ledger.record_created(LeaseId::new(2, 1), 601).unwrap_err();
        assert!(again.to_string().contains("created twice"), "{again}");
        ledger.record_deleted(LeaseId::new(2, 1)).unwrap();
        let twice = ledger.record_deleted(LeaseId::new(2, 1)).unwrap_err();
        assert!(twice.to_string().contains("not live"), "{twice}");
        let never = ledger.record_deleted(LeaseId::new(2, 9)).unwrap_err();
        assert!(never.to_string().contains("not live"), "{never}");
        // The refused records were not written, and nothing changed in memory.
        assert!(ledger.live_leases().is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 2.1 600\ndeleted 2.1\n");
    }

    /// Catches: a torn last record kept (it would glue onto the next record and make
    /// the ledger unreadable), or a corrupt complete line skipped.
    #[test]
    fn a_torn_last_line_is_cut_and_a_corrupt_line_refused() {
        let _path_own = dir("torn");
        let path = _path_own.join("ledger");
        std::fs::write(&path, "created 3.1 600\ncreated 3.").unwrap();
        // Own subscriber: the warning is seen (and its field lines count as run)
        // whatever else ran in the process.
        let log = Log::default();
        let subscriber = log.subscriber();
        let mut ledger =
            tracing::subscriber::with_default(subscriber, || Ledger::open(&path).unwrap());
        let text = log.text();
        assert!(text.contains("WARN"), "{text:?}");
        assert!(
            text.contains("dropping a ledger record cut short by a crash"),
            "{text:?}"
        );
        assert!(text.contains("fragment=\"created 3.\""), "{text:?}");
        assert_eq!(ledger.get(LeaseId::new(3, 1)).map(|e| e.uid), Some(600));
        ledger.record_created(LeaseId::new(3, 2), 601).unwrap();
        drop(ledger);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 3.1 600\ncreated 3.2 601\n");

        for bad in [
            "created 3.1\n",
            "created 3.1 six\n",
            "created 03.1 600\n",
            "removed 3.1\n",
            "deleted 3.1\n",
            "created 3.1 600\ncreated 3.1 601\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            let error = Ledger::open(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bad:?}");
            assert!(error.to_string().contains("line "), "{bad:?}: {error}");
        }
    }

    /// Catches: the ledger opened through a symbolic link planted at its path.
    #[test]
    fn a_symlink_at_the_ledger_path_is_refused() {
        let dir = dir("link");
        std::fs::write(dir.join("elsewhere"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("ledger")).unwrap();
        assert!(Ledger::open(&dir.join("ledger")).is_err());
    }

    /// Catches: memory updated although the record never reached the disk.
    #[test]
    fn a_failed_write_changes_nothing() {
        let _path_own = dir("fail");
        let path = _path_own.join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record_created(LeaseId::new(5, 1), 600).unwrap();
        ledger.fail_writes(&path, false);
        assert!(ledger.record_created(LeaseId::new(5, 2), 601).is_err());
        assert_eq!(ledger.get(LeaseId::new(5, 2)), None);
        assert_eq!(ledger.held_uids(), BTreeSet::from([600]));
    }
}
