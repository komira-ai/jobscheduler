//! The host bindings, kept in a file beside the lease file or, with a store setting, in the
//! state store.
//!
//! A lease on a registered host survives a restart because the lease file names the
//! host; what it cannot hold is which credential and instance the host id was bound to
//! when the host was admitted. Without that, the first hello for the id after a restart
//! would take the lease from anyone holding a join token. The server keeps the bindings
//! itself and hands them back to the registry when it seats the lease file's bookings.
//!
//! A taint is kept so that a restart does not make a host that reported one clean: nothing
//! in the server clears a taint, and a tainted host's binding is kept until an operator
//! stops the server and removes the three `taint_` fields of its entry. A cordon is kept so
//! that a restart does not lift it; it is written only while it is in force, so it leaves
//! the entry at the first save after it lapses or is lifted. An entry from before the field
//! is read as no cordon.
//!
//! # Without a store setting: the file
//!
//! The bindings are kept in a file of their own, `<state file>.agents`. The format is
//! TOML, written whole through a temporary file and renamed, like the lease file, and
//! readers ignore keys they do not know:
//!
//! ```toml
//! version = 1
//!
//! [[host]]
//! id = "builder-7"
//! credential = "<64 hex digits: SHA-256 of the join token>"
//! instance_provider = "aws"      # with instance_id, when the first hello named one
//! instance_id = "i-0abc"
//! taint_reason = "residue"       # when the host reported a taint
//! taint_run_id = "run-3-1"       # optional, with taint_reason
//! taint_check = "tmp-files"      # optional, with taint_reason
//! cordon_until_ms = 1760000000000 # when the host is cordoned: when the cordon lapses
//! ```
//!
//! The file is written after the registry changed: the save after a hello, a taint report,
//! a cordon or its lift, and the save of each tick, which also retries a failed one.
//!
//! # With a store setting
//!
//! With `--store` (or `server.store`) no file is written, and the file is read only by the
//! first start over an empty store, which imports it once (`crate::store`). Each host's binding is
//! the key `hosts/binding/<host id>` of the store ([`crate::host_keys`]): a JSON object
//! with a `version` and the fields of a file entry (all but `id`, which is the key). The
//! keys are read once at start, and nothing on the hot path reads the store. A change the
//! server makes itself (a hello that binds a host, a taint, a cordon or its lift) is
//! committed before the registry holds it ([`BindingStore::change`]), and a failed commit
//! leaves memory as it was. The save of each tick then makes the store hold what the
//! registry has bound ([`BindingStore::save`]), in one transaction: it drops the binding
//! of a host the registry forgot, and a cordon that lapsed. Fields of a value that this
//! build does not know are kept and written back.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent_wire::{InstanceIdentity, Taint};
use js_registry::{Binding, Credential};
use js_state::Store;
use js_types::HostId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::read_private;
use crate::error::StartupError;
use crate::host_keys::{self, BINDINGS, Extra, Writes};

/// The flag the refusals name: the file lives beside the lease file.
const FLAG: &str = "--state-file";

/// The version this build writes.
const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    #[serde(default)]
    host: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    id: String,
    credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taint_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taint_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taint_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cordon_until_ms: Option<u64>,
}

impl Entry {
    fn of(id: &HostId, binding: &Binding) -> Self {
        let instance = binding.instance();
        let taint = binding.taint();
        Self {
            id: id.to_string(),
            credential: binding.credential().to_hex(),
            instance_provider: instance.map(|instance| instance.provider.clone()),
            instance_id: instance.map(|instance| instance.instance_id.clone()),
            taint_reason: taint.map(|taint| taint.reason.clone()),
            taint_run_id: taint.and_then(|taint| taint.run_id.clone()),
            taint_check: taint.and_then(|taint| taint.check.clone()),
            cordon_until_ms: binding.cordon_until_ms(),
        }
    }

    fn into_binding(self) -> Result<(HostId, Binding), String> {
        let id = HostId::new(&self.id).map_err(|_| "a host id is not valid".to_string())?;
        let credential = Credential::from_hex(&self.credential)
            .ok_or_else(|| "a credential is not 64 lower-case hex digits".to_string())?;
        let instance = match (self.instance_provider, self.instance_id) {
            (None, None) => None,
            (Some(provider), Some(instance_id)) => Some(InstanceIdentity {
                provider,
                instance_id,
            }),
            _ => return Err("an instance has only one of provider and id".to_string()),
        };
        let taint = match (self.taint_reason, self.taint_run_id, self.taint_check) {
            (None, None, None) => None,
            (Some(reason), run_id, check) => Some(Taint {
                reason,
                run_id,
                check,
            }),
            _ => return Err("a taint has a run or a check but no reason".to_string()),
        };
        let binding = Binding::new(credential, instance).with_taint(taint);
        Ok((id, binding.with_cordon(self.cordon_until_ms)))
    }
}

/// A binding as the store holds it under `hosts/binding/<host id>`: the fields of a file
/// entry but the id, and the fields this build does not know.
#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taint_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taint_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taint_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cordon_until_ms: Option<u64>,
    #[serde(flatten)]
    extra: Extra,
}

impl Stored {
    /// The binding of the host `id` this value holds, checked as a file entry is, and the
    /// fields kept with it.
    fn into_binding(self, id: String) -> Result<(HostId, Binding, Extra), String> {
        let entry = Entry {
            id,
            credential: self.credential,
            instance_provider: self.instance_provider,
            instance_id: self.instance_id,
            taint_reason: self.taint_reason,
            taint_run_id: self.taint_run_id,
            taint_check: self.taint_check,
            cordon_until_ms: self.cordon_until_ms,
        };
        let (host, binding) = entry.into_binding()?;
        Ok((host, binding, self.extra))
    }
}

/// The value of `host`'s key that holds `binding`, over the fields `extra` kept.
fn stored_value(host: &HostId, binding: &Binding, extra: &Extra) -> Vec<u8> {
    let entry = Entry::of(host, binding);
    let mut fields = Map::new();
    fields.insert("credential".to_owned(), Value::from(entry.credential));
    host_keys::field(&mut fields, "instance_provider", entry.instance_provider);
    host_keys::field(&mut fields, "instance_id", entry.instance_id);
    host_keys::field(&mut fields, "taint_reason", entry.taint_reason);
    host_keys::field(&mut fields, "taint_run_id", entry.taint_run_id);
    host_keys::field(&mut fields, "taint_check", entry.taint_check);
    host_keys::field(&mut fields, "cordon_until_ms", entry.cordon_until_ms);
    host_keys::encode(fields, extra)
}

/// The key of `host`'s binding.
fn key(host: &HostId) -> String {
    format!("{BINDINGS}{host}")
}

/// Reads the bindings of the file at `path`; none when there is no file. The file must
/// be private like the token files (it names what each host is bound to), and one that
/// cannot be read as written is a refusal rather than a start with no bindings, which
/// would reopen the gap it exists to close.
pub(crate) fn load(path: &Path) -> Result<BTreeMap<HostId, Binding>, StartupError> {
    let Some(bytes) = read_if_present(path)? else {
        return Ok(BTreeMap::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| "it is not text".to_string());
    let parsed = text.and_then(|text| parse(&text));
    parsed.map_err(|reason| {
        let named = format!(
            "{} is not a valid agent state file: {reason}",
            path.display()
        );
        StartupError::new(FLAG, named)
    })
}

/// The keys the first start over a store imports from the file at `path` (none when there is
/// no file): each binding at `hosts/binding/<host id>`, as the store keeps it. The file is
/// read as [`load`] reads it, so its refusals stand.
pub(crate) fn import(path: &Path) -> Result<Vec<(String, Vec<u8>)>, StartupError> {
    let bindings = load(path)?;
    let extra = Extra::new();
    let value =
        |(host, binding): (&HostId, &Binding)| (key(host), stored_value(host, binding, &extra));
    Ok(bindings.iter().map(value).collect())
}

/// The bytes of the owner-only file at `path`; none when there is no file. Any other
/// failure to look at it is a refusal to start.
pub(crate) fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, StartupError> {
    match fs::metadata(path) {
        Ok(_) => read_private(path, FLAG).map(Some),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(StartupError::new(
            FLAG,
            format!("{}: {err}", path.display()),
        )),
    }
}

/// The bindings a file's text holds, or what is wrong with it.
fn parse(text: &str) -> Result<BTreeMap<HostId, Binding>, String> {
    let document: Document = toml::from_str(text).map_err(|err| err.message().to_string())?;
    let mut bindings = BTreeMap::new();
    for entry in document.host {
        let (id, binding) = entry.into_binding()?;
        if bindings.insert(id, binding).is_some() {
            return Err("a host id is listed twice".to_string());
        }
    }
    Ok(bindings)
}

/// Where the bindings are kept: the file beside the lease file, or the state store.
enum Keeper {
    File(PathBuf),
    Store(Arc<dyn Store>),
}

/// The bindings the file or the store holds, and the fields of each stored value that this
/// build does not know (none in the file, whose reader drops them).
#[derive(Default)]
struct Held {
    bindings: BTreeMap<HostId, Binding>,
    extra: BTreeMap<HostId, Extra>,
}

impl Held {
    /// The write that makes the store hold `binding` for `host`, or no binding.
    fn write(&self, host: &HostId, binding: Option<&Binding>) -> (String, Option<Vec<u8>>) {
        let extra = self.extra.get(host).cloned().unwrap_or_default();
        let value = binding.map(|binding| stored_value(host, binding, &extra));
        (key(host), value)
    }

    /// The writes that make the store hold `bindings` instead of these.
    fn writes_to(&self, bindings: &BTreeMap<HostId, Binding>) -> Writes {
        let gone = self
            .bindings
            .keys()
            .filter(|host| !bindings.contains_key(*host));
        let gone = gone.map(|host| self.write(host, None));
        let changed = bindings
            .iter()
            .filter(|(host, binding)| self.bindings.get(*host) != Some(*binding));
        let changed = changed.map(|(host, binding)| self.write(host, Some(binding)));
        gone.chain(changed).collect()
    }

    /// Holds `binding` for `host`, or no binding and nothing kept for it.
    fn hold(&mut self, host: &HostId, binding: Option<&Binding>) {
        match binding {
            Some(binding) => {
                self.bindings.insert(host.clone(), binding.clone());
            }
            None => {
                self.bindings.remove(host);
                self.extra.remove(host);
            }
        }
    }
}

/// One host's binding as a commit is to leave it, worked out before the commit, with the
/// writes that commit it and the writes that put the binding before it back.
pub(crate) struct Change {
    host: HostId,
    before: Option<Binding>,
    after: Binding,
    /// What the commit writes: nothing in file mode (the save after it writes the file), and
    /// nothing when the store holds `after` already.
    pub(crate) writes: Writes,
    /// What a compensating commit writes to put `before` back.
    pub(crate) undo: Writes,
}

/// Writes the bindings when they differ from what the file or the store holds.
pub(crate) struct BindingStore {
    keeper: Keeper,
    written: Mutex<Held>,
}

impl BindingStore {
    /// A store over the file at `path`, which holds `current` (what [`load`] read).
    pub(crate) fn new(path: PathBuf, current: BTreeMap<HostId, Binding>) -> Self {
        let held = Held {
            bindings: current,
            extra: BTreeMap::new(),
        };
        Self {
            keeper: Keeper::File(path),
            written: Mutex::new(held),
        }
    }

    /// The bindings `store` holds under `hosts/binding/`, read once at start, and a store
    /// over them. A value that cannot be read as written is a refusal, as a damaged file is.
    ///
    /// # Errors
    ///
    /// The store could not be read, or a value is not a binding (the reason names its key).
    pub(crate) fn in_store(
        store: Arc<dyn Store>,
    ) -> Result<(Self, BTreeMap<HostId, Binding>), String> {
        let mut held = Held::default();
        for (id, bytes) in host_keys::scan(&*store, BINDINGS)? {
            let named = format!("{BINDINGS}{id}");
            let stored: Stored = host_keys::decode(&named, &bytes)?;
            let read = stored.into_binding(id);
            let (host, binding, extra) = read.map_err(|reason| format!("{named}: {reason}"))?;
            held.extra.insert(host.clone(), extra);
            held.bindings.insert(host, binding);
        }
        let bindings = held.bindings.clone();
        let kept = Self {
            keeper: Keeper::Store(store),
            written: Mutex::new(held),
        };
        Ok((kept, bindings))
    }

    /// Whether the bindings are kept in the file (no store setting).
    pub(crate) fn in_file(&self) -> bool {
        matches!(self.keeper, Keeper::File(_))
    }

    fn lock(&self) -> MutexGuard<'_, Held> {
        self.written.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the file or the store hold what `current` returns, which is called with the
    /// store's lock held so that the last save to write has read the newest bindings.
    /// Nothing is written when they hold it already; a failed write leaves them as they
    /// were and is retried by the next call, which finds the bindings still different. The
    /// store is written in one transaction: a put for each binding that changed, a delete
    /// for each host that has none any more.
    pub(crate) fn save<F>(&self, current: F) -> io::Result<()>
    where
        F: FnOnce() -> BTreeMap<HostId, Binding>,
    {
        let mut written = self.lock();
        let bindings = current();
        if written.bindings == bindings {
            return Ok(());
        }
        match &self.keeper {
            Keeper::File(path) => replace(path, render(&bindings)?.as_bytes())?,
            Keeper::Store(store) => host_keys::commit(&**store, written.writes_to(&bindings))?,
        }
        let Held {
            bindings: held,
            extra,
        } = &mut *written;
        extra.retain(|host, _| bindings.contains_key(host));
        *held = bindings;
        Ok(())
    }

    /// The change that makes `host`'s binding `after`. Its writes are empty in file mode and
    /// when the store holds `after` already; otherwise they put `after`, and its undo puts
    /// back what the store holds now.
    pub(crate) fn change(&self, host: &HostId, after: Binding) -> Change {
        let written = self.lock();
        let before = written.bindings.get(host).cloned();
        let stored = matches!(self.keeper, Keeper::Store(_));
        let (writes, undo) = match stored && before.as_ref() != Some(&after) {
            true => (
                vec![written.write(host, Some(&after))],
                vec![written.write(host, before.as_ref())],
            ),
            false => (Writes::new(), Writes::new()),
        };
        Change {
            host: host.clone(),
            before,
            after,
            writes,
            undo,
        }
    }

    /// Commits `writes` to the store in one transaction. A file keeps nothing here (its
    /// writes are empty): the save after the change writes it.
    ///
    /// # Errors
    ///
    /// The store failed or refused the transaction: nothing was written.
    pub(crate) fn commit(&self, writes: Writes) -> io::Result<()> {
        match &self.keeper {
            Keeper::File(_) => Ok(()),
            Keeper::Store(store) => host_keys::commit(&**store, writes),
        }
    }

    /// Holds `change` as the store holds it once its writes are committed. Nothing in file
    /// mode, where what the file holds changes only when [`BindingStore::save`] writes it.
    pub(crate) fn applied(&self, change: &Change) {
        self.hold(&change.host, Some(&change.after));
    }

    /// Holds the binding before `change` once its undo is committed. Nothing in file mode.
    pub(crate) fn undone(&self, change: &Change) {
        self.hold(&change.host, change.before.as_ref());
    }

    fn hold(&self, host: &HostId, binding: Option<&Binding>) {
        if let Keeper::Store(_) = self.keeper {
            self.lock().hold(host, binding);
        }
    }
}

/// The text of the file that holds `bindings`.
fn render(bindings: &BTreeMap<HostId, Binding>) -> io::Result<String> {
    let host = bindings
        .iter()
        .map(|(id, binding)| Entry::of(id, binding))
        .collect();
    let document = Document {
        version: VERSION,
        host,
    };
    toml::to_string(&document).map_err(io::Error::other)
}

/// Replaces the file at `path` with `bytes`, readable by its owner only: written and
/// fsynced beside it (in a file made new, whatever an earlier crash left under that
/// name), renamed over it, and the directory fsynced, so a reader sees the old file or
/// the new one.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let temp = path.with_file_name(name);
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    // A file left by a crash keeps the mode it was made with, which `mode` below would not
    // change: remove it, so that the new file is made here, owner-only.
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(token: &str, instance: Option<(&str, &str)>) -> Binding {
        let instance = instance.map(|(provider, instance_id)| InstanceIdentity {
            provider: provider.to_string(),
            instance_id: instance_id.to_string(),
        });
        Binding::new(Credential::of_token(token), instance)
    }

    fn host(name: &str) -> HostId {
        HostId::new(name).unwrap()
    }

    const CREDENTIAL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The snapshot is taken while the store's lock is held, so of two saves the one that
    /// writes last holds the newest bindings. It catches a save that reads the registry
    /// first and locks after, which lets a slower save put an older snapshot over a newer
    /// file; the closure runs in the place of that read and finds the lock taken or not.
    #[test]
    fn the_snapshot_is_taken_under_the_lock() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_state_unit");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.agents");
        let store = BindingStore::new(path.clone(), BTreeMap::new());
        let mut bindings = BTreeMap::new();
        bindings.insert(host("a"), bound("token-a", None));
        let mut locked = false;
        store
            .save(|| {
                locked = store.written.try_lock().is_err();
                bindings.clone()
            })
            .unwrap();
        assert!(locked, "the snapshot was read before the store's lock");
        assert_eq!(load(&path).unwrap(), bindings);
    }

    /// What is written reads back as the same bindings, with and without an instance, and
    /// the text holds a digest and no token. It catches a binding that comes back as
    /// another credential or without its instance (a restart that lets a different
    /// machine take the lease) and a file that carries the secret itself.
    #[test]
    fn bindings_survive_the_file() {
        let mut bindings = BTreeMap::new();
        bindings.insert(host("a"), bound("token-a", None));
        bindings.insert(host("b"), bound("token-b", Some(("aws", "i-0abc"))));
        let text = render(&bindings).unwrap();
        assert_eq!(parse(&text).unwrap(), bindings);
        assert!(text.contains("version = 1"), "{text}");
        assert!(!text.contains("token-a"), "{text}");
        assert!(!text.contains("token-b"), "{text}");
        assert_eq!(parse("version = 1\n").unwrap(), BTreeMap::new());
    }

    /// A taint, with and without its run and check, reads back as written, and a host
    /// without one reads back without. It catches a restart that makes a tainted host
    /// clean, and one that taints a host that was not.
    #[test]
    fn a_taint_survives_the_file() {
        let taint = |run_id: Option<&str>, check: Option<&str>| Taint {
            reason: "residue".to_string(),
            run_id: run_id.map(str::to_string),
            check: check.map(str::to_string),
        };
        let mut bindings = BTreeMap::new();
        bindings.insert(
            host("a"),
            bound("t", None).with_taint(Some(taint(None, None))),
        );
        bindings.insert(
            host("b"),
            bound("t", None).with_taint(Some(taint(Some("run-1-1"), Some("tmp")))),
        );
        bindings.insert(host("c"), bound("t", None));
        let text = render(&bindings).unwrap();
        assert_eq!(parse(&text).unwrap(), bindings);
        assert_eq!(text.matches("taint_reason").count(), 2, "{text}");
    }

    /// A run or a check of a taint with no reason is a file that cannot be read as
    /// written. It catches a taint that loads as no taint.
    #[test]
    fn a_taint_without_a_reason_is_refused() {
        for extra in ["taint_run_id = \"r\"\n", "taint_check = \"c\"\n"] {
            let text = format!(
                "version = 1\n[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\n{extra}"
            );
            let reason = parse(&text).unwrap_err();
            assert!(reason.contains("taint"), "{reason}");
        }
    }

    /// A later version's keys, at the top and in an entry, are ignored whatever the
    /// version number says, and a file with no version at all is refused as not this
    /// file: the format is additive, so an older build reads a newer file. It catches a
    /// reader that refuses a file for a key it does not know, and one that takes any
    /// TOML with a `host` table for its own.
    #[test]
    fn unknown_keys_are_ignored() {
        let text = format!(
            "version = 9\nfuture = true\n[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\nnote = \"x\"\n"
        );
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.len(), 1);
        let unversioned = format!("[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\n");
        assert!(parse(&unversioned).is_err());
    }

    /// Every way a file can be wrong is refused with a reason that names the fault and
    /// repeats nothing of the entry: text that is not TOML, a bad host id, a credential
    /// that is not a digest, half an instance, and a host listed twice. It catches a
    /// file that loads as a smaller set of bindings (a start that quietly drops the
    /// protection it exists to give).
    #[test]
    fn a_file_that_cannot_be_read_as_written_is_refused() {
        let entry = |id: &str, credential: &str, extra: &str| {
            format!("version = 1\n[[host]]\nid = \"{id}\"\ncredential = \"{credential}\"\n{extra}")
        };
        let twice = format!(
            "{}[[host]]\nid = \"a\"\ncredential = \"{CREDENTIAL}\"\n",
            entry("a", CREDENTIAL, "")
        );
        let cases = [
            ("version = [".to_string(), ""),
            (entry("Bad Id", CREDENTIAL, ""), "host id"),
            (entry("a", "abc", ""), "credential"),
            (entry("a", CREDENTIAL, "instance_id = \"i\"\n"), "instance"),
            (
                entry("a", CREDENTIAL, "instance_provider = \"aws\"\n"),
                "instance",
            ),
            (twice, "twice"),
        ];
        for (text, names) in cases {
            let reason = parse(&text).unwrap_err();
            assert!(reason.contains(names), "{names}: {reason}");
            assert!(!reason.contains("Bad Id"), "{reason}");
            assert!(!reason.contains(CREDENTIAL), "{reason}");
        }
        let both = entry(
            "a",
            CREDENTIAL,
            "instance_provider = \"aws\"\ninstance_id = \"i\"\n",
        );
        assert!(parse(&both).is_ok());
    }

    /// A temporary file left beside the target by a crash, with a wider mode and other
    /// bytes, does not decide the mode of what is written: the result is owner-only and
    /// holds the new bytes. A temporary name that cannot be removed (a directory) is an
    /// error and the target is untouched. It catches a rename of a world-readable stale file
    /// into place.
    #[test]
    fn a_stale_temporary_file_does_not_decide_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent_state_unit");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stale.state");
        let temp = dir.join("stale.state.tmp");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&temp);
        fs::write(&temp, "left by a crash").unwrap();
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o644)).unwrap();
        replace(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!temp.exists());

        fs::create_dir(&temp).unwrap();
        assert!(replace(&path, b"newer").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        fs::remove_dir(&temp).unwrap();
    }
}
