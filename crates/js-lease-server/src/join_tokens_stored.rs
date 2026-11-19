//! The rows of the join tokens read one by one, by key, from a store other servers share
//! ([`JoinTokens::identify_stored`]), and the entry each row holds.

use js_registry::Credential;

use super::{Entry, JOINS, JoinTokens, Keeper, Row, Stored, entry_of, host_keys};

impl JoinTokens {
    /// [`JoinTokens::identify`] for a token this set does not hold, from the store: the row
    /// of `given` is read by its key and, when there is one, held from here on as if it had
    /// been read at start. Over a store other servers share, a token minted or a host
    /// credential issued by another server after this one started is known here at its
    /// first use, so the host it admitted can say hello here when that server stops. `None`
    /// when the store holds no such row, and in file mode (the file is the one server's).
    ///
    /// # Errors
    ///
    /// The store could not be read, or the row is not a token's (the reason names its key).
    pub(crate) fn identify_stored(&self, given: &str) -> Result<Option<Credential>, String> {
        let Keeper::Store(store) = &self.keeper else {
            return Ok(None);
        };
        let credential = Credential::of_token(given);
        let hash = credential.to_hex();
        let named = format!("{JOINS}{hash}");
        let Some(row) = store.get(&named).map_err(|error| error.to_string())? else {
            return Ok(None);
        };
        let (hash, entry) = stored_entry(hash, &row.value)?;
        self.lock().entry(hash).or_insert(entry);
        Ok(Some(credential))
    }
}

/// The entry of the row stored under `hash` with the value `bytes`.
pub(super) fn stored_entry(hash: String, bytes: &[u8]) -> Result<(String, Entry), String> {
    let named = format!("{JOINS}{hash}");
    let stored: Stored = host_keys::decode(&named, bytes)?;
    let row = Row {
        hash,
        provider: stored.provider,
        instance_id: stored.instance_id,
        expires_at_ms: stored.expires_at_ms,
        used_by: stored.used_by,
        credential_expires_at_ms: stored.credential_expires_at_ms,
        replaces: stored.replaces,
    };
    let (hash, entry) = entry_of(row).map_err(|reason| format!("{named}: {reason}"))?;
    let extra = stored.extra;
    Ok((hash, Entry { extra, ..entry }))
}
