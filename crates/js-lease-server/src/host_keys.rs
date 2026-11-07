//! The keys the server keeps its hosts under in the state store (`--store`, `server.store`):
//! one key per host binding, `hosts/binding/<host id>` ([`crate::agent_state`]), and one per
//! join token or host credential, `hosts/join/<digest>` ([`crate::join_tokens`]). The scaler's
//! two keys (`crate::scaler::kept`) and the first start's import commit through here too.
//!
//! A value is a JSON object with a `version` field. A reader takes any version, as the files
//! are read, and keeps the fields it does not know; the next write of the key writes them
//! back. Only digests are stored, never a token or a host credential. Every change is one
//! [`Store::txn`], committed before the server holds it in memory.

use std::collections::BTreeMap;
use std::io;

use js_state::{Commit, Cond, Conflict, Store, StoreError, Txn};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// The prefix of the host bindings.
pub(crate) const BINDINGS: &str = "hosts/binding/";

/// The prefix of the join tokens and host credentials, each under its digest.
pub(crate) const JOINS: &str = "hosts/join/";

/// The version this build writes.
const VERSION: u32 = 1;

/// The fields of a stored value that this build does not know, kept to be written back.
pub(crate) type Extra = BTreeMap<String, Value>;

/// Writes to commit together: each key with the value to put, or `None` to delete it.
pub(crate) type Writes = Vec<(String, Option<Vec<u8>>)>;

/// Commits `writes` in one transaction, and calls the store not at all when there are none.
///
/// # Errors
///
/// The store failed, or refused the write: nothing was written.
pub(crate) fn commit(store: &dyn Store, writes: Writes) -> io::Result<()> {
    if writes.is_empty() {
        return Ok(());
    }
    committed(store.txn(transaction(writes)))
}

/// The transaction of `writes`. The server is the store's one writer, under its lock, so
/// no write has a precondition.
fn transaction(writes: Writes) -> Txn {
    writes
        .into_iter()
        .fold(Txn::new(), |txn, (key, value)| match value {
            Some(value) => txn.put(key, value, Cond::Any),
            None => txn.delete(key, Cond::Any),
        })
}

/// A commit's answer as the result of a file write: an error when the store failed or
/// refused it.
pub(crate) fn committed(answer: Result<Result<Commit, Conflict>, StoreError>) -> io::Result<()> {
    match answer {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(conflict)) => Err(io::Error::other(format!(
            "the store refused the write of {}",
            conflict.key
        ))),
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

/// Every key under `prefix`, as what follows the prefix, with its value.
///
/// # Errors
///
/// The store failed, as its message says.
pub(crate) fn scan(store: &dyn Store, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let rows = store.scan_prefix(prefix).map_err(|err| err.to_string())?;
    let rows = rows.into_iter().map(|(key, row)| {
        let name = key.get(prefix.len()..).unwrap_or_default().to_owned();
        (name, row.value)
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
        commit(&store, vec![("a".to_owned(), Some(b"1".to_vec()))]).unwrap();
        let writes = vec![
            ("a".to_owned(), None),
            ("b".to_owned(), Some(b"2".to_vec())),
        ];
        commit(&store, writes).unwrap();
        assert_eq!(store.commit_stats().txns, 2);
        commit(&store, Writes::new()).unwrap();
        assert_eq!(store.commit_stats().txns, 2);
        assert!(store.get("a").unwrap().is_none());
        assert_eq!(store.get("b").unwrap().unwrap().value, b"2");
        store.fail_next();
        let failed = commit(&store, vec![("c".to_owned(), None)]).unwrap_err();
        assert!(failed.to_string().contains(INJECTED), "{failed}");
    }

    /// A refused write (which the server's writes, with no precondition, never meet) is an
    /// error naming the key, as a failed store is. It catches a refusal read as a success.
    #[test]
    fn a_refused_write_is_an_error_naming_its_key() {
        let conflict = Conflict {
            index: 0,
            key: "hosts/join/k".to_owned(),
        };
        let refused = committed(Ok(Err(conflict))).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "the store refused the write of hosts/join/k"
        );
        let commit = Commit {
            revision: Revision(1),
            counters: Vec::new(),
        };
        assert!(committed(Ok(Ok(commit))).is_ok());
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
        let refused = decode::<Known>("hosts/join/k", b"{\"a\": \"secret\"}").err();
        assert_eq!(
            refused.as_deref(),
            Some("hosts/join/k does not hold this key's fields")
        );
    }

    /// A scan names each key by what follows the prefix, and a failed read is its reason.
    #[test]
    fn a_scan_names_keys_after_the_prefix() {
        let store = store();
        let writes = vec![
            ("hosts/join/d1".to_owned(), Some(b"x".to_vec())),
            ("hosts/other".to_owned(), Some(b"y".to_vec())),
        ];
        commit(&store, writes).unwrap();
        let rows = scan(&store, JOINS).unwrap();
        assert_eq!(rows, vec![("d1".to_owned(), b"x".to_vec())]);
        store.fail_read_after(0);
        let failed = scan(&store, JOINS).unwrap_err();
        assert!(failed.contains(INJECTED), "{failed}");
    }
}
