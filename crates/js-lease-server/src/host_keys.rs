//! The keys the server keeps its hosts under in the state store (`--store`, `server.store`):
//! one key per host binding, `hosts/binding/<host id>` ([`crate::agent_state`]). The scaler's
//! two keys (`crate::scaler::kept`) and the first start's import commit through here too. The
//! rows an earlier release kept under `hosts/join/` are neither read nor written here.
//!
//! A value is a JSON object with a `version` field. A reader takes any version, as the files
//! are read, and keeps the fields it does not know; the next write of the key writes them
//! back. Only digests are stored, never a secret. Every change is one
//! [`Store::txn`], committed before the server holds it in memory.

use std::collections::BTreeMap;
use std::io;

use js_state::{Cond, Conflict, Revision, Store, StoreError, Txn, Version};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// The prefix of the host bindings.
pub(crate) const BINDINGS: &str = "hosts/binding/";

/// The version this build writes.
const VERSION: u32 = 1;

/// The fields of a stored value that this build does not know, kept to be written back.
pub(crate) type Extra = BTreeMap<String, Value>;

/// One write of a commit: the key, the value to put or `None` to delete it, and the
/// condition the key must meet. A binding's write is conditional on the version of the row
/// this server read or wrote last ([`crate::agent_state::BindingStore`]), as other servers
/// over the store write the same rows; the scaler's rows and the import's are written at
/// [`Cond::Any`].
pub(crate) type KeyWrite = (String, Option<Vec<u8>>, Cond);

/// Writes to commit together.
pub(crate) type Writes = Vec<KeyWrite>;

/// The write a commit refused because its key did not meet its condition: another server
/// wrote the key since this one read it. It is the error [`commit`] returns for that, which
/// [`refused_key`] recognises.
#[derive(Debug, thiserror::Error)]
#[error("the store refused the write of {0}")]
struct Refused(String);

/// The key a failed commit was refused for, when the store refused it for the key's condition
/// (and not because it failed).
pub(crate) fn refused_key(error: &io::Error) -> Option<&str> {
    let inner = error.get_ref()?.downcast_ref::<Refused>();
    inner.map(|refused| refused.0.as_str())
}

/// Commits `writes` in one transaction, and calls the store not at all when there are none.
/// The revision it answers is the version of the row written only when the transaction wrote
/// one row, or the row written last is the one wanted (a put, as the job rows' commits use it).
///
/// # Errors
///
/// The store failed, or refused the write: nothing was written. A refusal for a key's
/// condition is told by [`refused_key`].
pub(crate) fn commit(store: &dyn Store, writes: Writes) -> io::Result<Option<Revision>> {
    if writes.is_empty() {
        return Ok(None);
    }
    committed(store.txn(transaction(writes))).map(Some)
}

/// Commits `write` alone, and answers its revision, which is the version of the row it wrote
/// (the one write of the transaction).
///
/// # Errors
///
/// As [`commit`].
pub(crate) fn commit_one(store: &dyn Store, write: KeyWrite) -> io::Result<Revision> {
    committed(store.txn(transaction(vec![write])))
}

/// The transaction of `writes`, each at its own condition.
fn transaction(writes: Writes) -> Txn {
    writes
        .into_iter()
        .fold(Txn::new(), |txn, (key, value, cond)| match value {
            Some(value) => txn.put(key, value, cond),
            None => txn.delete(key, cond),
        })
}

/// A commit's answer as the result of a file write: its revision, or an error when the store
/// failed or refused it.
pub(crate) fn committed(
    answer: Result<Result<js_state::Commit, Conflict>, StoreError>,
) -> io::Result<Revision> {
    match answer {
        Ok(Ok(commit)) => Ok(commit.revision),
        Ok(Err(conflict)) => Err(io::Error::other(Refused(conflict.key))),
        Err(err) => Err(io::Error::other(err)),
    }
}

/// The value of a key: `fields` and the `version`, over the fields `extra` kept from the
/// value read.
pub(crate) fn encode(fields: Map<String, Value>, extra: &Extra) -> Vec<u8> {
    let mut object: Map<String, Value> = extra.clone().into_iter().collect();
    object.insert("version".to_owned(), Value::from(VERSION));
    object.extend(fields);
    Value::Object(object).to_string().into_bytes()
}

/// Adds `value` to `fields` under `name`, when there is one.
pub(crate) fn field(fields: &mut Map<String, Value>, name: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        fields.insert(name.to_owned(), value.into());
    }
}

/// Every key under `prefix`, as what follows the prefix, with its value and its version.
///
/// # Errors
///
/// The store failed, as its message says.
pub(crate) fn scan(
    store: &dyn Store,
    prefix: &str,
) -> Result<Vec<(String, Vec<u8>, Version)>, String> {
    let rows = store.scan_prefix(prefix).map_err(|err| err.to_string())?;
    let rows = rows.into_iter().map(|(key, row)| {
        let name = key.get(prefix.len()..).unwrap_or_default().to_owned();
        (name, row.value, row.version)
    });
    Ok(rows.collect())
}

/// The value at `key` as `T`, or a reason that names the key and nothing of the value.
///
/// # Errors
///
/// The value is not a JSON object with `T`'s fields.
pub(crate) fn decode<T: DeserializeOwned>(key: &str, bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|_| format!("{key} does not hold this key's fields"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_jobs::interfere::{INJECTED, Interfere};
    use js_state::fake::{Fake, Flaw};
    use js_state::{ManualClock, Revision};
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    fn store() -> Interfere<Fake> {
        Interfere::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ))
    }

    /// A put and a delete go in one transaction, and no writes make no call: the store's
    /// commit count says so. A failed commit is an error with the store's reason. It catches
    /// a commit split in two (a crash between them would leave half of it) and a call made
    /// for nothing on the hot path.
    #[test]
    fn writes_commit_in_one_transaction_and_none_make_no_call() {
        let store = store();
        commit(
            &store,
            vec![("a".to_owned(), Some(b"1".to_vec()), Cond::Any)],
        )
        .unwrap();
        let writes = vec![
            ("a".to_owned(), None, Cond::Any),
            ("b".to_owned(), Some(b"2".to_vec()), Cond::Any),
        ];
        commit(&store, writes).unwrap();
        assert_eq!(store.commit_stats().txns, 2);
        assert_eq!(commit(&store, Writes::new()).unwrap(), None);
        assert_eq!(store.commit_stats().txns, 2);
        assert!(store.get("a").unwrap().is_none());
        assert_eq!(store.get("b").unwrap().unwrap().value, b"2");
        store.fail_next();
        let failed = commit(&store, vec![("c".to_owned(), None, Cond::Any)]).unwrap_err();
        assert!(failed.to_string().contains(INJECTED), "{failed}");
    }

    /// A refused write is an error naming the key, which [`refused_key`] tells from a failed
    /// store, and a commit answers its revision. It catches a refusal read as a success, and a
    /// store's failure taken for a lost version (which a save answers by reading the row again
    /// instead of failing).
    #[test]
    fn a_refused_write_is_an_error_naming_its_key() {
        let conflict = Conflict {
            index: 0,
            key: "hosts/binding/k".to_owned(),
        };
        let refused = committed(Ok(Err(conflict))).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "the store refused the write of hosts/binding/k"
        );
        assert_eq!(refused_key(&refused), Some("hosts/binding/k"));
        assert_eq!(refused_key(&io::Error::other("the store failed")), None);
        let commit = js_state::Commit {
            revision: Revision(1),
            counters: Vec::new(),
        };
        assert_eq!(committed(Ok(Ok(commit))).unwrap(), Revision(1));
    }

    /// A value holds its fields, the version, and the fields kept from what was read; a
    /// field this build writes wins over a kept one of the same name, and an absent field
    /// is left out. Reading it back keeps the unknown field. It catches a write that drops
    /// what a newer build stored, and one that writes `null` for an absent field.
    #[test]
    fn a_value_keeps_the_fields_it_does_not_know() {
        #[derive(Deserialize)]
        struct Known {
            version: u32,
            a: String,
            #[serde(flatten)]
            extra: Extra,
        }
        let mut extra = Extra::new();
        extra.insert("later".to_owned(), json!({"x": 1}));
        extra.insert("a".to_owned(), json!("old"));
        let mut fields = Map::new();
        field(&mut fields, "a", Some("new"));
        field(&mut fields, "b", None::<u64>);
        let bytes = encode(fields, &extra);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value, json!({"version": 1, "a": "new", "later": {"x": 1}}));
        let known: Known = decode("k", &bytes).unwrap();
        assert_eq!((known.version, known.a.as_str()), (1, "new"));
        assert_eq!(known.extra.get("later"), Some(&json!({"x": 1})));
        let refused = decode::<Known>("hosts/binding/k", b"{\"a\": \"secret\"}").err();
        assert_eq!(
            refused.as_deref(),
            Some("hosts/binding/k does not hold this key's fields")
        );
    }

    /// A scan names each key by what follows the prefix, and a failed read is its reason.
    #[test]
    fn a_scan_names_keys_after_the_prefix() {
        let store = store();
        let writes = vec![
            (
                "hosts/binding/d1".to_owned(),
                Some(b"x".to_vec()),
                Cond::Any,
            ),
            ("hosts/other".to_owned(), Some(b"y".to_vec()), Cond::Any),
        ];
        commit(&store, writes).unwrap();
        let rows = scan(&store, BINDINGS).unwrap();
        let version = store.get("hosts/binding/d1").unwrap().unwrap().version;
        assert_eq!(rows, vec![("d1".to_owned(), b"x".to_vec(), version)]);
        store.fail_read_after(0);
        let failed = scan(&store, BINDINGS).unwrap_err();
        assert!(failed.contains(INJECTED), "{failed}");
    }
}
