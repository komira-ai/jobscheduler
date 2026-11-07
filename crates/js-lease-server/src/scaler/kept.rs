//! Where the scaler's records and the hosts the registry refused to remove are kept: the two
//! files beside the lease file ([`RecordsFile`], [`UnremovedFile`]), or, with a store setting
//! (`--store`, `server.store`), two keys of the state store.
//!
//! | key | value |
//! |---|---|
//! | `acquires/scaler/records` | all of the scaler's [`Records`], in their own format ([`Records::encode`]: JSON, versioned), written whole |
//! | `acquires/scaler/unremoved` | the hosts to remove, the text of the unremoved file: `{"version":1,"hosts":[...]}` |
//!
//! Each save is one [`Store::txn`] that puts its key whole, made where the file would be
//! written: the records before the first call a step decided, the hosts before the registry
//! is asked to remove one. A save that cannot be committed is the shell's failed save, as a
//! file that cannot be written is: the step stops before any call and the failure is counted.
//! The keys are read once at start, after the writer lock, and are refused as the files are
//! when they cannot be read as written; a live pool that is not at its first start does not
//! start without the records key, as it does not without the file.

use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;

use js_scaler::Records;
use js_state::Store;

use super::{RecordsFile, UnremovedFile, unremoved};
use crate::error::StartupError;
use crate::host_keys;

/// The key of the scaler's records.
pub(crate) const RECORDS: &str = "acquires/scaler/records";

/// The key of the hosts the registry refused to remove.
pub(crate) const UNREMOVED: &str = "acquires/scaler/unremoved";

/// Where the shell's saves go: the files, or the state store.
pub(crate) enum ScalerKeeper {
    Files {
        records: RecordsFile,
        unremoved: UnremovedFile,
    },
    Store(Arc<dyn Store>),
}

impl ScalerKeeper {
    /// Makes the file or the key hold `records`, all of them.
    ///
    /// # Errors
    ///
    /// The file could not be written, or the store failed or refused the commit; what was
    /// kept before is still there.
    pub(crate) fn save(&self, records: &Records) -> io::Result<()> {
        match self {
            Self::Files { records: file, .. } => file.save(records),
            Self::Store(store) => {
                let text = records.encode().map_err(io::Error::other)?;
                put(&**store, RECORDS, text)
            }
        }
    }

    /// Makes the file or the key hold `hosts`, all of them.
    ///
    /// # Errors
    ///
    /// As for [`ScalerKeeper::save`].
    pub(crate) fn save_unremoved(&self, hosts: &BTreeSet<String>) -> io::Result<()> {
        match self {
            Self::Files {
                unremoved: file, ..
            } => file.save(hosts),
            Self::Store(store) => put(&**store, UNREMOVED, unremoved::encode(hosts)),
        }
    }
}

/// Puts `text` at `key` in one transaction.
fn put(store: &dyn Store, key: &str, text: String) -> io::Result<()> {
    host_keys::commit(store, vec![(key.to_owned(), Some(text.into_bytes()))])
}

/// The records and the hosts to remove that `store` holds, read at start. No records key is
/// no records, unless `existing` (a live pool not at its first start), when it is a refusal;
/// no hosts key is no hosts.
///
/// # Errors
///
/// The reason, naming the key: the store could not be read, a value is not text, or is not
/// records or a list of hosts, or the records are missing and `existing`.
pub(crate) fn read(
    store: &dyn Store,
    existing: bool,
) -> Result<(Records, BTreeSet<String>), String> {
    let records = match text(store, RECORDS)? {
        Some(text) => Records::decode(&text)
            .map_err(|err| format!("{RECORDS} does not hold valid scaler records: {err}"))?,
        None if existing => return Err(missing()),
        None => Records::default(),
    };
    let hosts = match text(store, UNREMOVED)? {
        Some(text) => unremoved::decode(&text)
            .map_err(|why| format!("{UNREMOVED} does not hold a valid list of hosts: {why}"))?,
        None => BTreeSet::new(),
    };
    Ok((records, hosts))
}

/// The refusal of a live pool's start with no records key.
fn missing() -> String {
    format!(
        "{RECORDS} is missing from the store, and a pool with launch = \"live\" does not start without its scaler records: it would launch again for machines that may already exist and forget the day's spend. If this is the first start of the pool, start once with --boot initialise (boot = \"initialise\" in the file)"
    )
}

/// The text at `key`; none when the key is absent.
fn text(store: &dyn Store, key: &str) -> Result<Option<String>, String> {
    let row = store
        .get(key)
        .map_err(|err| format!("{key} could not be read: {err}"))?;
    let text = row.map(|row| String::from_utf8(row.value));
    text.transpose().map_err(|_| format!("{key} is not text"))
}

/// The keys the first start over a store imports from the two files ([`crate::store`]): the
/// records file's text, read and checked as at a start without a store (so its refusals
/// stand), when there is a file, and the hosts of the unremoved file, when it lists any (no
/// file lists none). `existing` is as for [`read`]: a live pool not at its first start is
/// refused when there is no records file, before anything is imported.
///
/// # Errors
///
/// The readers' refusals of the files, naming the file.
pub(crate) fn import(
    (records, unremoved): (&RecordsFile, &UnremovedFile),
    existing: bool,
) -> Result<Vec<(String, Vec<u8>)>, StartupError> {
    let text = records.import_text(existing)?;
    let hosts = unremoved.load()?;
    let records = text.map(|text| (RECORDS.to_owned(), text.into_bytes()));
    let listed = !hosts.is_empty();
    let hosts = listed.then(|| (UNREMOVED.to_owned(), unremoved::encode(&hosts).into_bytes()));
    Ok(records.into_iter().chain(hosts).collect())
}

#[cfg(test)]
mod tests {
    use js_jobs::interfere::{INJECTED, Interfere};
    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Txn};

    use super::*;

    fn store() -> Arc<Interfere<Fake>> {
        let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        Arc::new(Interfere::new(fake))
    }

    fn hosts() -> BTreeSet<String> {
        BTreeSet::from(["js-a".to_owned(), "js-b".to_owned()])
    }

    /// Puts `value` at `key` of `store`, as a value another build could have left.
    fn left(store: &Interfere<Fake>, key: &str, value: &[u8]) {
        let txn = Txn::new().put(key, value.to_vec(), Cond::Any);
        store.txn(txn).unwrap().unwrap();
    }

    /// Asserts that the records and the hosts are each put whole at their key, in their own
    /// format (the records file's text and the unremoved file's), and read back as written;
    /// with no key, a store reads as no records and no hosts. It catches a save that writes
    /// another format than the files' (whose goldens describe the records), and a read that
    /// takes an empty store for a damaged one.
    #[test]
    fn the_records_and_the_hosts_round_trip_through_their_keys() {
        let store = store();
        assert_eq!(
            read(&*store, false).unwrap(),
            (Records::default(), BTreeSet::new())
        );
        let kept = ScalerKeeper::Store(store.clone());
        let records = Records::default();
        kept.save(&records).unwrap();
        kept.save_unremoved(&hosts()).unwrap();
        assert_eq!(store.commit_stats().txns, 2);
        let stored = store.get(RECORDS).unwrap().unwrap().value;
        assert_eq!(stored, records.encode().unwrap().into_bytes());
        let stored = store.get(UNREMOVED).unwrap().unwrap().value;
        assert_eq!(stored, br#"{"version":1,"hosts":["js-a","js-b"]}"#);
        assert_eq!(read(&*store, true).unwrap(), (records, hosts()));
    }

    /// Asserts that a commit that fails is the save's error, with the store's reason, for the
    /// records and for the hosts, and leaves what the keys held. It catches a save that
    /// swallows a failed commit, which would let the shell call the provider for a record
    /// that is not in the store.
    #[test]
    fn a_commit_that_fails_is_a_failed_save() {
        let store = store();
        let kept = ScalerKeeper::Store(store.clone());
        store.fail_next();
        let failed = kept.save(&Records::default()).unwrap_err();
        assert!(failed.to_string().contains(INJECTED), "{failed}");
        store.fail_next();
        let failed = kept.save_unremoved(&hosts()).unwrap_err();
        assert!(failed.to_string().contains(INJECTED), "{failed}");
        assert!(store.get(RECORDS).unwrap().is_none());
        assert!(store.get(UNREMOVED).unwrap().is_none());
    }

    /// Asserts each refusal of the read at start, by its whole text: no records key for a
    /// live pool that is not at its first start, a value that is not text, records that are
    /// not records, hosts that are not a list, and a read that fails. It catches a damaged or
    /// missing key read as an empty memory, which would launch again for machines in flight.
    #[test]
    fn a_store_that_cannot_be_read_as_written_is_refused() {
        let store = store();
        let refused = read(&*store, true).unwrap_err();
        assert_eq!(refused, missing());
        assert!(refused.starts_with("acquires/scaler/records is missing from the store"));
        assert!(refused.contains("--boot initialise"), "{refused}");

        left(&store, RECORDS, &[0xff]);
        let refused = read(&*store, false).unwrap_err();
        assert_eq!(refused, "acquires/scaler/records is not text");
        left(&store, RECORDS, b"{}");
        let refused = read(&*store, false).unwrap_err();
        assert!(
            refused.starts_with("acquires/scaler/records does not hold valid scaler records: "),
            "{refused}"
        );

        left(
            &store,
            RECORDS,
            Records::default().encode().unwrap().as_bytes(),
        );
        left(&store, UNREMOVED, br#"{"version":2,"hosts":[]}"#);
        let refused = read(&*store, false).unwrap_err();
        assert!(
            refused.starts_with("acquires/scaler/unremoved does not hold a valid list of hosts: "),
            "{refused}"
        );

        left(&store, UNREMOVED, br#"{"version":1,"hosts":[]}"#);
        store.fail_read_after(1);
        let refused = read(&*store, false).unwrap_err();
        assert!(
            refused.starts_with("acquires/scaler/unremoved could not be read: "),
            "{refused}"
        );
        store.fail_read_after(0);
        let refused = read(&*store, false).unwrap_err();
        assert!(
            refused.starts_with("acquires/scaler/records could not be read: "),
            "{refused}"
        );
        assert!(refused.contains(INJECTED), "{refused}");
    }
}
