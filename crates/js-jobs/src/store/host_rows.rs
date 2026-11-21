//! The rows of a host that a placement's own transaction reads beside its booking, so that a
//! host is refused on what the commit finds and not on a picture one server holds (design
//! `multi-instance.md`, G7): the host's taint (`taint/<host>`, and the taint its binding
//! `hosts/binding/<host>` holds), its scale-down mark (`hostmark/<host>`, [`ScaleMark`]) and the
//! host ports its live allocations hold.
//!
//! The scale-down mark is written by whoever marks the host, at the version it read
//! ([`JobStore::mark_host`], [`JobStore::unmark_host`]), and listed whole
//! ([`JobStore::host_marks`]). The table is additive: a store written before it has no
//! `hostmark/` rows, and no `idx/` space names one.

use std::collections::{BTreeMap, BTreeSet};

use js_state::{Cond, Store, Txn, TxnRead, Version, Versioned};
use js_types::HostId;

use super::{JobStore, allocation_at, ensure};
use crate::codec::DecodeError;
use crate::codec::host_fields::{decode_hostmark, encode_hostmark};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{AllocationStep, ScaleMark};

/// The host and mark the row `key` holds.
fn hostmark_at(key: &str, row: &Versioned) -> Result<(HostId, ScaleMark), JobStoreError> {
    let host = HostId::new(key.trim_start_matches(keys::HOSTMARK))
        .map_err(|_| JobStoreError::row(key, DecodeError::WrongKey))?;
    let mark = decode_hostmark(&row.value).map_err(|why| JobStoreError::row(key, why))?;
    Ok((host, mark))
}

/// Whether the binding row `key`, whose value is `value`, holds a taint: a JSON object whose
/// `taint_reason` is text. A value that is not a JSON object, or whose `taint_reason` is
/// neither text nor null, is [`JobStoreError::Corrupt`], so a placement is never made on a
/// binding it cannot read.
pub(crate) fn binding_tainted(key: &str, value: &[u8]) -> Result<bool, JobStoreError> {
    let corrupt = || JobStoreError::row(key, DecodeError::BadValue { tag: 0 });
    let parsed: serde_json::Value = serde_json::from_slice(value).map_err(|_| corrupt())?;
    let object = parsed.as_object().ok_or_else(corrupt)?;
    match object.get("taint_reason") {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::String(_)) => Ok(true),
        Some(_) => Err(corrupt()),
    }
}

/// The host ports that the live allocations on `host` hold, as the reads of `read` show them:
/// the rows of the host's on-host entries.
fn ports_held(read: &mut dyn TxnRead, host: &HostId) -> Result<BTreeSet<u16>, JobStoreError> {
    let entries = read.scan_prefix(&keys::on_host_prefix(host))?;
    let rows: BTreeSet<String> = entries
        .iter()
        .filter_map(|(entry, _)| keys::indexed_row(entry))
        .collect();
    let mut held = BTreeSet::new();
    for key in rows {
        let Some(row) = read.get(&key)? else {
            continue;
        };
        let allocation = allocation_at(&key, &row.value)?;
        if (allocation.host == *host) & (allocation.step != AllocationStep::Freed) {
            held.extend(allocation.host_ports);
        }
    }
    Ok(held)
}

/// The check the commit makes of the host's binding row (its key and the condition of the
/// version read) once the rows of `host` that `read` shows let the job `id`, which declares the
/// host ports `ports`, be placed there; otherwise the refusal, and nothing is written:
///
/// - a `taint/<host>` row is [`JobStoreError::HostTainted`] (the commit also checks it absent
///   for a contained job, and puts it at its absence for a host job);
/// - a binding that holds a taint is [`JobStoreError::HostTainted`]; the commit checks the
///   binding row at the version read, so a taint stored in between makes it conflict;
/// - a `hostmark/<host>` row is [`JobStoreError::HostMarked`] (the commit checks it absent);
/// - a host port of the job that a live allocation on the host holds is
///   [`JobStoreError::PortTaken`], naming the first.
pub(super) fn refuse_host(
    read: &mut dyn TxnRead,
    (id, host): (&JobId, &HostId),
    ports: &BTreeSet<u16>,
) -> Result<(String, Cond), JobStoreError> {
    let tainted = || JobStoreError::HostTainted {
        id: id.clone(),
        host: host.clone(),
    };
    ensure(read.get(&keys::taint(host))?.is_none(), tainted())?;
    let binding_key = keys::binding(host);
    let binding = read.get(&binding_key)?;
    if let Some(row) = &binding {
        ensure(!binding_tainted(&binding_key, &row.value)?, tainted())?;
    }
    let binding_cond = binding.map_or(Cond::Absent, |row| Cond::Version(row.version));
    ensure(
        read.get(&keys::hostmark(host))?.is_none(),
        JobStoreError::HostMarked {
            id: id.clone(),
            host: host.clone(),
        },
    )?;
    if !ports.is_empty() {
        let held = ports_held(read, host)?;
        if let Some(port) = ports.intersection(&held).next() {
            return Err(JobStoreError::PortTaken {
                id: id.clone(),
                host: host.clone(),
                port: *port,
            });
        }
    }
    Ok((binding_key, binding_cond))
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Every host's scale-down mark, with the version of its row, by host. A row this build
    /// cannot read (a later build's, one that does not parse, or one under a key that names no
    /// host) is left out and logged; every placement's transaction still finds the row and
    /// refuses the host.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn host_marks(&self) -> Result<BTreeMap<HostId, (ScaleMark, Version)>, JobStoreError> {
        let mut marks = BTreeMap::new();
        for (key, row) in self.store.scan_prefix(keys::HOSTMARK)? {
            match hostmark_at(&key, &row) {
                Ok((host, mark)) => {
                    marks.insert(host, (mark, row.version));
                }
                Err(error) => {
                    tracing::warn!(%key, %error, "a host mark this build cannot read is skipped");
                }
            }
        }
        Ok(marks)
    }

    /// The scale-down mark of `host`, with the version of its row, or `None` when it has none.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] when a later build wrote the row, [`JobStoreError::Corrupt`]
    /// when it does not parse; [`JobStoreError::Store`].
    pub fn read_host_mark(
        &self,
        host: &HostId,
    ) -> Result<Option<(ScaleMark, Version)>, JobStoreError> {
        let key = keys::hostmark(host);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        Ok(Some((hostmark_at(&key, &row)?.1, row.version)))
    }

    /// Puts the scale-down mark `mark` on `host`, at the version `read` (the version the caller
    /// read the row at, or `None` for a row it read absent): `true` when it was written,
    /// `false` when the row changed since, and nothing was written.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn mark_host(
        &self,
        host: &HostId,
        mark: ScaleMark,
        read: Option<Version>,
    ) -> Result<bool, JobStoreError> {
        let cond = read.map_or(Cond::Absent, Cond::Version);
        let txn = Txn::new().put(keys::hostmark(host), encode_hostmark(mark)?, cond);
        Ok(self.write(txn)?.is_ok())
    }

    /// Deletes the scale-down mark of `host` if its row is still at `at`: `true` when it was
    /// deleted, `false` when the row is absent or was written since, and nothing was deleted.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn unmark_host(&self, host: &HostId, at: Version) -> Result<bool, JobStoreError> {
        let txn = Txn::new().delete(keys::hostmark(host), Cond::Version(at));
        Ok(self.write(txn)?.is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::binding_tainted;
    use crate::codec::DecodeError;
    use crate::error::JobStoreError;

    /// Asserts how a binding's value is read for its taint: a `taint_reason` that is text is a
    /// taint, a missing or null one is none, and a value that is not an object, or whose
    /// reason is something else, cannot be read. Catches a binding read as untainted when it
    /// cannot be read, which would place a job on a host whose taint is not known.
    #[test]
    fn a_binding_is_tainted_only_by_a_text_reason_and_an_unreadable_one_is_corrupt() {
        let key = "hosts/binding/h";
        let tainted = |value: &str| binding_tainted(key, value.as_bytes());
        assert_eq!(
            tainted(r#"{"version":1,"taint_reason":"residue"}"#),
            Ok(true)
        );
        assert_eq!(tainted(r#"{"version":1}"#), Ok(false));
        assert_eq!(tainted(r#"{"version":1,"taint_reason":null}"#), Ok(false));
        let corrupt = Err(JobStoreError::Corrupt {
            key: key.to_owned(),
            why: DecodeError::BadValue { tag: 0 },
        });
        for value in ["not json", "[1]", r#"{"taint_reason":7}"#] {
            assert_eq!(tainted(value), corrupt, "{value}");
        }
    }
}
