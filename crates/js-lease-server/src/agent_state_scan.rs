//! Every binding the store holds, read by one scan: what a pass over the queued jobs offers a
//! host by, whichever server took its hello ([`BindingStore::scan_stored`]).

use std::collections::BTreeMap;
use std::io;

use js_registry::Binding;
use js_types::HostId;
use tracing::warn;

use super::{BindingStore, Keeper, Stored};
use crate::host_keys::{self, BINDINGS};

impl BindingStore {
    /// Every binding the store holds now, on this server's clock, by host; `None` in file
    /// mode, where the registry holds every binding there is. A value that cannot be read as a
    /// binding is logged and left out: the placement transaction reads the row itself and
    /// refuses a host whose binding it cannot read.
    ///
    /// # Errors
    ///
    /// The store could not be scanned.
    pub(crate) fn scan_stored(&self) -> io::Result<Option<BTreeMap<HostId, Binding>>> {
        let Keeper::Store(store, _) = &self.keeper else {
            return Ok(None);
        };
        let skew = self.lock().skew;
        let mut found = BTreeMap::new();
        for (id, bytes) in host_keys::scan(&**store, BINDINGS).map_err(io::Error::other)? {
            let named = format!("{BINDINGS}{id}");
            let read = host_keys::decode::<Stored>(&named, &bytes)
                .and_then(|stored| stored.into_binding(id));
            match read {
                Ok((host, binding, _)) => {
                    found.insert(host, skew.to_server(binding));
                }
                Err(error) => {
                    warn!(key = %named, %error, "a binding that cannot be read is skipped")
                }
            }
        }
        Ok(Some(found))
    }
}
