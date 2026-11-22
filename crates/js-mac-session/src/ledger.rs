//! The ledger: every lease id the helper ever made an account for, kept across reboots.
//!
//! One append-only text file in the helper's state directory, one record per line, each
//! written with a single `write` and then `fsync`ed before the helper acts on it. An id is
//! used once: a second `user-create` for it is refused. The ledger holds the uid each
//! lease got, so `run`, `kill-uid` and `user-delete` act on the uid it names rather than on
//! whatever a directory lookup says:
//!
//! ```text
//! created <term>.<seq> <uid>
//! deleted <term>.<seq>
//! ```
//!
//! `created` is written before the lease's account is made, so a crash after it leaves a
//! lease the next boot reset deletes (a lease with no account is deleted as one with
//! none left to delete). `deleted` is written once the account was deleted and shown gone
//! (`user-delete`): the lease's uid is free from then on, and not before. A lease that is
//! created and not deleted holds its uid, which is how a deletion that did not complete
//! keeps its uid out of use. This is the format every release since the first wrote and
//! read, so each of them still reads this file.
//!
//! The uid of the last `created` record is the one the next lease's uid is chosen after
//! ([`Ledger::last_uid`]), so a uid comes back as late as the range allows and no state
//! beyond this file is kept for it.
//!
//! A crash can leave a last line without its newline. That record's action never
//! started (it follows the `fsync`), so on opening the fragment is cut off. Any other
//! line that does not parse stops the helper from starting: the ledger is root's own
//! file, and guessing past corruption could reuse a lease id or hand out a uid twice.
//!
//! The file grows by about 30 bytes per lease and is not compacted (a later change can
//! drop records of ended leases once lease ids are known to grow).
//!
//! Beside it a release that kept a pool of lease users wrote a second file, [`POOL_FILE`].
//! This release neither reads nor writes it; the boot reset removes it once, before it
//! deletes the pool's accounts ([`Ledger::remove_pool_file`]), so a rollback to that release
//! finds no pool and makes its own afresh.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Mode, OFlags};

use js_types::LeaseId;

use crate::lease::{Dotted, parse_lease};

/// The name of the file a release with a pool kept beside the ledger.
pub const POOL_FILE: &str = "pool";

/// One lease's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The uid the lease's user had.
    pub uid: u32,
    /// Whether the lease is over (its account was deleted and shown gone); a lease id
    /// that is over is never used again.
    pub deleted: bool,
}

/// The ledger file and what it says.
#[derive(Debug)]
pub struct Ledger {
    file: File,
    dir: PathBuf,
    leases: BTreeMap<LeaseId, Entry>,
    /// The uid of the last `created` record: the next uid is chosen after it.
    last_uid: Option<u32>,
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
    /// Opens (creating, mode 0600) and reads the ledger at `path`.
    ///
    /// # Errors
    /// The file cannot be opened, read or trimmed, or a complete line does not parse or
    /// does not follow from the lines before it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let (file, text) = open_records(path)?;
        let mut ledger = Self {
            file,
            dir: path.parent().unwrap_or(Path::new(".")).to_owned(),
            leases: BTreeMap::new(),
            last_uid: None,
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
                self.last_uid = Some(uid);
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

    /// The uid of the last `created` record read or written: the next lease's uid is chosen
    /// after it. `None` for a ledger with no record.
    #[must_use]
    pub fn last_uid(&self) -> Option<u32> {
        self.last_uid
    }

    /// Makes every later write fail, as a full or failing disk would.
    #[cfg(test)]
    pub(crate) fn fail_writes(&mut self, path: &Path) {
        self.file = File::open(path).unwrap();
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

    /// Removes the file ([`POOL_FILE`]) a release that kept a pool of lease users wrote
    /// beside the ledger, if it is there, and makes the removal durable (an `fsync` of the
    /// state directory, whether or not a file was there). Returns whether a file was removed.
    ///
    /// # Errors
    /// The removal or the `fsync` failed.
    pub fn remove_pool_file(&self) -> io::Result<bool> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let dir = rustix::fs::openat(CWD, &self.dir, flags, Mode::empty())?;
        let removed = match rustix::fs::unlinkat(&dir, POOL_FILE, AtFlags::empty()) {
            Ok(()) => true,
            Err(rustix::io::Errno::NOENT) => false,
            Err(why) => return Err(why.into()),
        };
        rustix::fs::fsync(&dir)?;
        Ok(removed)
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
    /// forgets used lease ids and a name can be used again.
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
        assert_eq!(ledger.last_uid(), Some(601));
        assert_eq!(ledger.held_uids(), BTreeSet::from([601]));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 1.1 600\ncreated 1.2 601\ndeleted 1.1\n");
    }

    /// Catches: a second `created` for one lease accepted (a reused name),
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
        assert_eq!(ledger.last_uid(), Some(600));
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
        ledger.fail_writes(&path);
        assert!(ledger.record_created(LeaseId::new(5, 2), 601).is_err());
        assert_eq!(ledger.get(LeaseId::new(5, 2)), None);
        assert_eq!(ledger.last_uid(), Some(600));
        assert_eq!(ledger.held_uids(), BTreeSet::from([600]));
    }

    /// Catches: the next uid chosen after the wrong record: the last `created` record
    /// is the one, in the order written (not the highest uid, not the highest lease id),
    /// and a `deleted` record moves nothing; none in a ledger with no record.
    #[test]
    fn the_last_uid_is_that_of_the_last_created_record() {
        let _path_own = dir("last");
        let path = _path_own.join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.last_uid(), None);
        ledger.record_created(LeaseId::new(9, 5), 650).unwrap();
        ledger.record_created(LeaseId::new(1, 1), 610).unwrap();
        ledger.record_deleted(LeaseId::new(9, 5)).unwrap();
        assert_eq!(ledger.last_uid(), Some(610));
        drop(ledger);
        assert_eq!(Ledger::open(&path).unwrap().last_uid(), Some(610));
    }

    /// Catches: a removal of the pool file that is not durable or that fails for a file
    /// that is not there (the boot reset would then stop), or one that leaves the file.
    #[test]
    fn the_pool_file_is_removed_once() {
        let state = dir("pool-file");
        let path = state.join("ledger");
        let ledger = Ledger::open(&path).unwrap();
        assert!(!ledger.remove_pool_file().unwrap());
        std::fs::write(state.join(POOL_FILE), "made 0 600\nfree 0\n").unwrap();
        assert!(ledger.remove_pool_file().unwrap());
        assert!(!state.join(POOL_FILE).exists());
        assert!(path.exists(), "the ledger stays");
        assert!(!ledger.remove_pool_file().unwrap());
        // Something that cannot be unlinked as a file is an error, and stays.
        std::fs::create_dir(state.join(POOL_FILE)).unwrap();
        assert!(ledger.remove_pool_file().is_err());
        assert!(state.join(POOL_FILE).is_dir());
    }

    /// Catches: a removal that reports success when the state directory is not there to
    /// be opened.
    #[test]
    fn the_pool_file_of_a_missing_state_directory_is_an_error() {
        let state = dir("pool-file-missing");
        let inner = state.join("state");
        std::fs::create_dir(&inner).unwrap();
        let ledger = Ledger::open(&inner.join("ledger")).unwrap();
        std::fs::remove_file(inner.join("ledger")).unwrap();
        std::fs::remove_dir(&inner).unwrap();
        assert!(ledger.remove_pool_file().is_err());
    }
}
