//! The host bindings, kept in a file beside the state file or, with a store setting, in the
//! state store.
//!
//! A binding holds which credential and instance a host id was bound to when the host was
//! admitted. Without it, the first hello for the id after a restart would take the id from
//! anyone holding a join token. The server keeps the bindings itself and hands them back to
//! the registry at start (a host that reported a taint, or is cordoned, is put back as an
//! awaiting host, bound as it was).
//!
//! A taint is kept so that a restart does not make a host that reported one clean: the only
//! things in the server that clear a taint are the host's own heartbeat (reporting the taint it
//! holds as cleared, `taint_cleared`, or saying it holds none, `tainted: false`, which ends a
//! taint the agent made; see the server's "Taint") and the operator's `ClearHostTaint` call,
//! which each remove the three `taint_` fields of its entry; otherwise a binding keeps it. A cordon is kept so
//! that a restart does not lift it; it is written only while it is in force, so it leaves
//! the entry at the first save after it lapses or is lifted. An entry from before the field
//! is read as no cordon.
//!
//! # Without a store setting: the file
//!
//! The bindings are kept in a file of their own, `<state file>.agents`. The format is
//! TOML, written whole through a temporary file and renamed, and
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
//! keys are read at start; after it, one key is read at the hello of a host the registry does
//! not know ([`BindingStore::stored`]), as another server over the same store may have
//! changed it, and nothing else on the hot path reads the store. A change the
//! server makes itself (a hello that binds a host, a taint, a cordon or its lift) is
//! committed before the registry holds it ([`BindingStore::change`]), and a failed commit
//! leaves memory as it was. The save of each tick then makes the store hold what the
//! registry has bound ([`BindingStore::save`]): it drops the binding of a host the registry
//! held and forgot (never of one it did not hold, which another server may serve), and a
//! cordon that lapsed. Fields of a value that this build does not know are kept and written
//! back.
//!
//! Other servers write the same rows, so every write of a binding is conditional on the
//! version of the row this server last read or wrote ([`Held`]): `Cond::Version` for a row it
//! saw, `Cond::Absent` for one it saw absent. A write that loses its version (another server
//! wrote the row in between) changes nothing: a hello is refused as a failed write and the
//! agent says hello again, a save reads the row again, keeps what it holds, and leaves the
//! row as the other server wrote it, so a taint another server stored is never overwritten
//! with a binding made before it.
//!
//! A stored `cordon_until_ms` is a time of the store's clock (milliseconds since the Unix
//! epoch, as the file's), since another server over the store reads it: the store's clock less
//! this server's ([`Skew`]) is read from the store at start, at each read of one key, at each
//! save and before each cordon's commit ([`BindingStore::measure`]), each binding is put on the
//! store's clock as it is written and back on this server's as it is read, and what is held in
//! memory is on this server's clock, as the registry's is. A save or read whose store cannot
//! read its clock fails, writing nothing. The file is one server's and holds this server's
//! clock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent_wire::refusal as refusal_code;
use js_agent_wire::{InstanceIdentity, Taint};
use js_registry::{Binding, Credential};
use js_state::{Cond, Revision, Store, Txn, Version};
use js_types::HostId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::clock::SharedClock;
use crate::config::read_private;
use crate::error::StartupError;
use crate::host_keys::{self, BINDINGS, Extra, KeyWrite, Writes};

/// The flag the refusals name: the file lives beside the state file.
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
/// no file): each binding at `hosts/binding/<host id>`, as the store keeps it, its cordon end
/// moved from this server's clock (the file's) onto the store's by `skew`. The file is read as
/// [`load`] reads it, so its refusals stand.
pub(crate) fn import(path: &Path, skew: Skew) -> Result<Vec<(String, Vec<u8>)>, StartupError> {
    let bindings = load(path)?;
    let extra = Extra::new();
    let value = |(host, binding): (&HostId, &Binding)| {
        (
            key(host),
            stored_value(host, &skew.to_store(binding), &extra),
        )
    };
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

/// Where the bindings are kept: the file beside the state file, or the state store, with the
/// clock of this server that the bindings in memory are on.
enum Keeper {
    File(PathBuf),
    Store(Arc<dyn Store>, SharedClock),
}

/// The store's clock less this server's, in milliseconds, as last read: what puts a time of
/// this server's clock (a cordon end the registry holds) on the store's, and back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Skew(i64);

impl Skew {
    /// The store's clock less `clock`, read now from `store`.
    ///
    /// # Errors
    /// The store could not read its clock.
    pub(crate) fn read(store: &dyn Store, clock: &SharedClock) -> io::Result<Skew> {
        let at = store.now().map_err(io::Error::other)?;
        let store_ms = i64::try_from(at.as_duration().as_millis()).unwrap_or(i64::MAX);
        let server_ms = i64::try_from(js_types::Clock::now(clock).millis()).unwrap_or(i64::MAX);
        Ok(Skew(store_ms.saturating_sub(server_ms)))
    }

    /// `binding` with its cordon end on the store's clock, from this server's.
    fn to_store(self, binding: &Binding) -> Binding {
        let moved = binding
            .cordon_until_ms()
            .map(|ms| ms.saturating_add_signed(self.0));
        binding.clone().with_cordon(moved)
    }

    /// `binding` with its cordon end on this server's clock, from the store's.
    fn to_server(self, binding: Binding) -> Binding {
        let back = self.0.saturating_neg();
        let moved = binding
            .cordon_until_ms()
            .map(|ms| ms.saturating_add_signed(back));
        binding.with_cordon(moved)
    }
}

/// The bindings the file or the store holds, the fields of each stored value that this
/// build does not know (none in the file, whose reader drops them), the version of each
/// row, and the hosts this server's registry has held a binding of.
///
/// Over a store other servers share, a binding this server read but whose host its registry
/// never held (a host that says hello to another server) is not this server's to delete: a
/// save deletes only the binding of a host its registry held and then let go of. A server
/// that starts beside another therefore leaves every binding the other one serves as it
/// is.
///
/// Every write of a row is conditional on `versions`, the version of the row as this server
/// last read or wrote it. A host with no version is written at [`Cond::Absent`]: it was read
/// absent, or its version is not known (a restore wrote it in a commit of several rows); a row that is
/// there then refuses the write, and the refusal reads it again, so a version not known costs
/// a read and overwrites nothing.
///
/// The bindings held are on this server's clock; `skew` is the store's clock less this
/// server's as last read, by which a write puts a binding's cordon end on the store's clock
/// (zero in file mode, which holds this server's).
#[derive(Default)]
struct Held {
    bindings: BTreeMap<HostId, Binding>,
    extra: BTreeMap<HostId, Extra>,
    versions: BTreeMap<HostId, Version>,
    held: BTreeSet<HostId>,
    skew: Skew,
}

/// A row of the store as it was read: the binding on the store's clock, the fields this build
/// does not know, and the version.
struct Row {
    binding: Binding,
    extra: Extra,
    version: Version,
}

/// One host's write of a save, with the binding it leaves (none for a delete).
struct Pending {
    host: HostId,
    after: Option<Binding>,
    write: KeyWrite,
}

impl Held {
    /// The condition a write of `host`'s row is made at: the version this server holds for it,
    /// or that the row is absent.
    fn cond(&self, host: &HostId) -> Cond {
        self.versions
            .get(host)
            .map_or(Cond::Absent, |version| Cond::Version(*version))
    }

    /// The write that makes the store hold `binding` for `host`, or no binding, its cordon end
    /// put on the store's clock, at the version held for the row.
    fn write(&self, host: &HostId, binding: Option<&Binding>) -> KeyWrite {
        let extra = self.extra.get(host).cloned().unwrap_or_default();
        let on_store = binding.map(|binding| self.skew.to_store(binding));
        let value = on_store.map(|binding| stored_value(host, &binding, &extra));
        (key(host), value, self.cond(host))
    }

    /// The writes that make the store hold `bindings`, the registry's, instead of these: a put
    /// for each binding that differs, and a delete for each host the registry held before and
    /// holds no more.
    fn writes_to(&self, bindings: &BTreeMap<HostId, Binding>) -> Vec<Pending> {
        let gone = self.gone(bindings);
        let gone = gone.map(|host| (host, None));
        let changed = bindings
            .iter()
            .filter(|(host, binding)| self.bindings.get(*host) != Some(*binding));
        let changed = changed.map(|(host, binding)| (host, Some(binding)));
        let pending = |(host, after): (&HostId, Option<&Binding>)| Pending {
            host: host.clone(),
            after: after.cloned(),
            write: self.write(host, after),
        };
        gone.chain(changed).map(pending).collect()
    }

    /// The hosts the registry held a binding of and holds none of in `bindings`.
    fn gone<'a>(
        &'a self,
        bindings: &'a BTreeMap<HostId, Binding>,
    ) -> impl Iterator<Item = &'a HostId> + 'a {
        self.held
            .iter()
            .filter(|host| !bindings.contains_key(*host))
    }

    /// Commits the writes that make the store hold `bindings`, the registry's, one transaction
    /// for each host, and holds what each left. A write that loses the version of its row
    /// (another server wrote it since this one read it) is not made: the row is read again and
    /// held as it is, and answered with the binding it holds, so the caller can take a taint
    /// of it into the registry. A delete that loses leaves the row to the server that wrote it
    /// and the host is no longer one this server held. The other hosts are written all the
    /// same. One transaction for each host, since the revision of a commit that wrote one row
    /// is that row's version, which the next write is conditional on; a commit of several
    /// rows would leave the version of each to be read.
    ///
    /// # Errors
    ///
    /// The store failed: the hosts written before it stay written and held, and the rest are
    /// left to the next save.
    fn save_each(
        &mut self,
        store: &dyn Store,
        bindings: BTreeMap<HostId, Binding>,
        served: &dyn Fn(&HostId) -> bool,
    ) -> io::Result<Vec<(HostId, Binding)>> {
        let pending = self.writes_to(&bindings);
        self.held.extend(bindings.into_keys());
        let mut lost = Vec::new();
        for Pending { host, after, write } in pending {
            // A host this registry let go of that a server still serves (its session row is
            // there) keeps its binding: the row is left to that server, and is not this one's.
            if after.is_none() && served(&host) {
                self.hold(&host, None);
                continue;
            }
            match host_keys::commit_one(store, write) {
                Ok(revision) => self.wrote(&host, after.as_ref(), revision),
                Err(error) if host_keys::refused_key(&error).is_some() => {
                    if after.is_none() {
                        self.held.remove(&host);
                    }
                    let row = fetch(store, &host)?;
                    let seen = self.saw(&host, row);
                    lost.extend(seen.map(|binding| (host, binding)));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(lost)
    }

    /// Holds `binding` as what the store holds for `host` and the registry holds as well,
    /// written by a commit of that one row at `revision`, or no binding and nothing kept for
    /// it.
    fn wrote(&mut self, host: &HostId, binding: Option<&Binding>, revision: Revision) {
        self.hold(host, binding);
        if binding.is_some() {
            self.versions.insert(host.clone(), Version(revision.0));
        }
    }

    /// Holds `binding` as what the store holds for `host`, written with other rows by one
    /// commit, so that the version of its row is not known, without taking the host for one
    /// the registry held.
    fn remember(&mut self, host: &HostId, binding: &Binding) {
        self.bindings.insert(host.clone(), binding.clone());
        self.versions.remove(host);
    }

    /// Holds `binding` for `host` as the registry's, or no binding and nothing kept for it.
    /// The version of the row is no longer known to be what it was: the caller that knows it
    /// holds it after.
    fn hold(&mut self, host: &HostId, binding: Option<&Binding>) {
        self.versions.remove(host);
        match binding {
            Some(binding) => {
                self.bindings.insert(host.clone(), binding.clone());
                self.held.insert(host.clone());
            }
            None => {
                self.bindings.remove(host);
                self.extra.remove(host);
                self.held.remove(host);
            }
        }
    }

    /// Holds `row`, which a read of the host's key found (none when the key is absent), as
    /// what the store holds for `host`, with its fields and version, and answers the binding
    /// on this server's clock. What the registry held of `host` is not changed.
    fn saw(&mut self, host: &HostId, row: Option<Row>) -> Option<Binding> {
        let Some(Row {
            binding,
            extra,
            version,
        }) = row
        else {
            // Another server dropped it since it was read: what was read is not what the
            // store holds, so a change puts the binding again (at `Cond::Absent`) rather than
            // taking it for stored.
            self.bindings.remove(host);
            self.extra.remove(host);
            self.versions.remove(host);
            return None;
        };
        let binding = self.skew.to_server(binding);
        self.bindings.insert(host.clone(), binding.clone());
        self.extra.insert(host.clone(), extra);
        self.versions.insert(host.clone(), version);
        Some(binding)
    }
}

/// The row of `host`'s key as the store holds it now; none when the key is absent.
///
/// # Errors
///
/// The store could not be read, or its value is not a binding.
fn fetch(store: &dyn Store, host: &HostId) -> io::Result<Option<Row>> {
    let name = key(host);
    let Some(row) = store.get(&name).map_err(io::Error::other)? else {
        return Ok(None);
    };
    let stored: Stored = host_keys::decode(&name, &row.value).map_err(io::Error::other)?;
    let (_, binding, extra) = stored
        .into_binding(host.to_string())
        .map_err(|reason| io::Error::other(format!("{name}: {reason}")))?;
    Ok(Some(Row {
        binding,
        extra,
        version: row.version,
    }))
}

/// One host's binding as a commit is to leave it, worked out before the commit, with the
/// writes that commit it.
pub(crate) struct Change {
    host: HostId,
    after: Binding,
    /// What the commit writes: nothing in file mode (the save after it writes the file), and
    /// nothing when the store holds `after` already. The write is conditional on the version
    /// of the row this server read last (`Cond::Absent` when it read none), so a row another
    /// server wrote since is not overwritten.
    pub(crate) writes: Writes,
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
            ..Held::default()
        };
        Self {
            keeper: Keeper::File(path),
            written: Mutex::new(held),
        }
    }

    /// The bindings `store` holds under `hosts/binding/`, read once at start, each put on
    /// `clock` (this server's), and a store over them. A value that cannot be read as written
    /// is a refusal, as a damaged file is.
    ///
    /// # Errors
    ///
    /// The store could not be read (its clock included), or a value is not a binding (the
    /// reason names its key).
    pub(crate) fn in_store(
        store: Arc<dyn Store>,
        clock: SharedClock,
    ) -> Result<(Self, BTreeMap<HostId, Binding>), String> {
        let skew = Skew::read(&*store, &clock).map_err(|error| error.to_string())?;
        let mut held = Held {
            skew,
            ..Held::default()
        };
        for (id, bytes, version) in host_keys::scan(&*store, BINDINGS)? {
            let named = format!("{BINDINGS}{id}");
            let stored: Stored = host_keys::decode(&named, &bytes)?;
            let read = stored.into_binding(id);
            let (host, binding, extra) = read.map_err(|reason| format!("{named}: {reason}"))?;
            held.extra.insert(host.clone(), extra);
            held.versions.insert(host.clone(), version);
            held.bindings.insert(host, skew.to_server(binding));
        }
        let bindings = held.bindings.clone();
        let kept = Self {
            keeper: Keeper::Store(store, clock),
            written: Mutex::new(held),
        };
        Ok((kept, bindings))
    }

    /// Reads the store's clock less this server's, which the writes made from here on put
    /// cordon ends on the store's clock by; nothing in file mode. A blocking call: the cordon
    /// routes make it before a cordon's commit, as the save and the read of one key do.
    ///
    /// # Errors
    ///
    /// The store could not read its clock: the skew read before stands, and the caller
    /// writes nothing.
    pub(crate) fn measure(&self) -> io::Result<()> {
        let Keeper::Store(store, clock) = &self.keeper else {
            return Ok(());
        };
        let skew = Skew::read(&**store, clock)?;
        self.lock().skew = skew;
        Ok(())
    }

    /// The binding the store holds for `host` now, read by its key, which is also held from
    /// here on as what the store holds, with the version of the row (so a change made after it
    /// is conditional on that version). `None`
    /// when the store holds none, and in file mode: the file is the one server's, and its
    /// bindings are those read at start.
    ///
    /// A server reads it at the hello of a host, before the hello is judged
    /// ([`crate::agents::Agents::restore_stored`]): the host may come from another server over
    /// the same store, which stored a taint or a cordon for it after this one started.
    ///
    /// # Errors
    ///
    /// The store could not be read, or its value is not a binding.
    pub(crate) fn stored(&self, host: &HostId) -> io::Result<Option<Binding>> {
        let Keeper::Store(store, _) = &self.keeper else {
            return Ok(None);
        };
        self.measure()?;
        let row = fetch(&**store, host)?;
        Ok(self.lock().saw(host, row))
    }

    /// Deletes the stored binding of `host`, a host the operator retired whose record the
    /// registry has forgotten, at the version [`BindingStore::stored`] last read it at, and
    /// holds none for it, so that no save puts it back or deletes it again: whether a binding
    /// was deleted (`false` when that read found none). A binding this server's registry never
    /// held is deleted too, so that no server's restart puts the host back from it. Nothing in
    /// file mode, where the save after the registry forgot the host writes the file without it.
    ///
    /// # Errors
    ///
    /// The store could not be written, or another server wrote the row since it was read:
    /// nothing was deleted, and the same call again (after a read) is the retry.
    pub(crate) fn forget(&self, host: &HostId) -> io::Result<bool> {
        let Keeper::Store(store, _) = &self.keeper else {
            return Ok(false);
        };
        let mut written = self.lock();
        let Some(version) = written.versions.get(host).copied() else {
            written.hold(host, None);
            return Ok(false);
        };
        host_keys::commit_one(&**store, (key(host), None, Cond::Version(version)))?;
        written.hold(host, None);
        Ok(true)
    }

    /// Whether the bindings are kept in the file (no store setting).
    pub(crate) fn in_file(&self) -> bool {
        matches!(self.keeper, Keeper::File(_))
    }

    fn lock(&self) -> MutexGuard<'_, Held> {
        self.written.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the file or the store hold what `current` returns (the registry's bindings),
    /// which is called with the store's lock held so that the last save to write has read the
    /// newest bindings. Nothing is written when they hold it already; a failed write leaves
    /// them as they were and is retried by the next call, which finds the bindings still
    /// different.
    ///
    /// The file is the one server's, and holds exactly `current`. The store is written one
    /// host at a time, each at the version of its row this server holds: a put for each
    /// binding that changed, and a delete for each host the registry held and let go of; a
    /// binding of a host the registry never held is another server's and is left as it is
    /// ([`Held`]). A write that loses its version to another server's is not made, and the
    /// row is read again: the answer is the binding of each such row that holds one, for the
    /// caller to take what another server stored (a taint) into the registry. The next save
    /// writes the registry's binding over the row it read, or none when they agree.
    ///
    /// # Errors
    ///
    /// The file or the store could not be written, or a row could not be read again.
    pub(crate) fn save<F>(
        &self,
        current: F,
        served: &dyn Fn(&HostId) -> bool,
    ) -> io::Result<Vec<(HostId, Binding)>>
    where
        F: FnOnce() -> BTreeMap<HostId, Binding>,
    {
        self.measure()?;
        let mut written = self.lock();
        let bindings = current();
        match &self.keeper {
            Keeper::File(path) => {
                if written.bindings == bindings {
                    return Ok(Vec::new());
                }
                replace(path, render(&bindings)?.as_bytes())?;
                written.bindings = bindings;
                Ok(Vec::new())
            }
            Keeper::Store(store, _) => written.save_each(&**store, bindings, served),
        }
    }

    /// Makes the file or the store hold `bindings`, the bindings a start read with the taints
    /// it put back on them ([`crate::agents_taints`]), before the registry holds any of them.
    /// The file is written whole, as [`BindingStore::save`] writes it. In the store only the
    /// bindings that differ from what it holds are put, in one transaction, each at the
    /// version the start read for its row, and nothing is deleted or held as the registry's: a
    /// host whose binding is not the registry's yet may be served by another server over the
    /// same store ([`Held`]). The version of a row it wrote is not known after (the revision
    /// of a commit of several rows is the last one's), so the next write of it reads the row
    /// first, as every hello and cordon does.
    ///
    /// # Errors
    ///
    /// The file or the store could not be written, or another server wrote a row since the
    /// start read it (the write is not made over it): nothing is written, the start is
    /// refused, and the next one reads the rows again.
    pub(crate) fn restore(&self, bindings: &BTreeMap<HostId, Binding>) -> io::Result<()> {
        let Keeper::Store(store, _) = &self.keeper else {
            return self.save(|| bindings.clone(), &|_| false).map(drop);
        };
        let mut written = self.lock();
        let changed = bindings
            .iter()
            .filter(|(host, binding)| written.bindings.get(*host) != Some(*binding));
        let changed: Vec<_> = changed.collect();
        let writes = changed
            .iter()
            .map(|(host, binding)| written.write(host, Some(binding)));
        host_keys::commit(&**store, writes.collect())?;
        for (host, binding) in changed {
            written.remember(host, binding);
        }
        Ok(())
    }

    /// The change that makes `host`'s binding `after`. Its writes are empty in file mode and
    /// when the store holds `after` already; otherwise they put `after` at the version of the
    /// row this server holds ([`Held`]).
    pub(crate) fn change(&self, host: &HostId, after: Binding) -> Change {
        let written = self.lock();
        let before = written.bindings.get(host).cloned();
        let stored = matches!(self.keeper, Keeper::Store(..));
        let writes = match stored && before.as_ref() != Some(&after) {
            true => vec![written.write(host, Some(&after))],
            false => Writes::new(),
        };
        Change {
            host: host.clone(),
            after,
            writes,
        }
    }

    /// Commits `writes` to the store in one transaction, and answers its revision: the version
    /// of the binding written when `writes` is a change's one write, or the last of the writes
    /// of a hello's transaction ([`crate::join_tokens::JoinTokens::admit_writing`]); none when
    /// there was nothing to commit. A file keeps nothing here (its writes are empty): the save
    /// after the change writes it.
    ///
    /// # Errors
    ///
    /// The store failed or refused the transaction (a write whose row another server wrote
    /// since is refused): nothing was written.
    pub(crate) fn commit(&self, writes: Writes) -> io::Result<Option<Revision>> {
        match &self.keeper {
            Keeper::File(_) => Ok(None),
            Keeper::Store(store, _) => host_keys::commit(&**store, writes),
        }
    }

    /// Commits `change`, which changes the host's taint, only when the store still holds the
    /// binding with the taint `expected` (the taint this server's registry holds for the
    /// host) and the binding is as it was read: the write is conditional on the version just
    /// read, so a write of another server between the read and the commit is refused, and a
    /// taint another server reported or cleared is never overwritten by a report or a clear
    /// made on what this server saw before. Answers the revision of the commit, the version of
    /// the binding written. Nothing in file mode (the one server is the file's only writer),
    /// and nothing to commit is a success with no revision.
    ///
    /// # Errors
    ///
    /// The store failed or refused the write, or holds a different taint than `expected`:
    /// nothing was written, and the heartbeat is answered with an error so that the agent
    /// reports again.
    pub(crate) fn commit_taint(
        &self,
        change: &Change,
        expected: Option<&Taint>,
    ) -> io::Result<Option<Revision>> {
        let (Keeper::Store(store, _), Some((name, value, _))) =
            (&self.keeper, change.writes.first())
        else {
            return Ok(None);
        };
        let name = name.as_str();
        let row = store
            .get(name)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other(format!("{name} is not in the store")))?;
        let stored: Stored = host_keys::decode(name, &row.value).map_err(io::Error::other)?;
        let (_, binding, _) = stored
            .into_binding(change.host.to_string())
            .map_err(io::Error::other)?;
        if binding.taint() != expected {
            return Err(io::Error::other(format!(
                "{name} holds another taint than this server's: another server changed it"
            )));
        }
        // A change that sets a taint is a put, so there is a value; a delete would put nothing.
        let value = value.clone().unwrap_or_default();
        let txn = Txn::new().put(name, value, Cond::Version(row.version));
        host_keys::committed(store.txn(txn)).map(Some)
    }

    /// Ends the taint the store holds for `host` when the agent reports `cleared`: the stored
    /// taint is `cleared`, field for field, or one a refused start made (reason
    /// `host-tainted`, which the agent never reports, so any cleared report ends it; an agent
    /// still tainted refuses its next start and taints the host again). The binding without
    /// the taint is committed by the conditional write of [`BindingStore::commit_taint`],
    /// expecting the taint read, and is then held as stored. Answers the change, or `None`
    /// when nothing was written: the store holds no binding of `host`, another taint or none,
    /// and in file mode. Unless the commit is made, what this server holds as stored for
    /// `host` is left as it was, so a later [`BindingStore::save`] never writes this
    /// server's binding over a taint the store holds that the registry does not.
    ///
    /// # Errors
    ///
    /// The store could not be read, failed or refused the write: nothing was written.
    pub(crate) fn end_taint(&self, host: &HostId, cleared: &Taint) -> io::Result<Option<Change>> {
        let kept = {
            let written = self.lock();
            (
                written.bindings.get(host).cloned(),
                written.extra.get(host).cloned(),
                written.versions.get(host).copied(),
            )
        };
        let ended = self.stored(host).and_then(|stored| {
            let ends =
                |taint: &Taint| taint == cleared || taint.reason == refusal_code::HOST_TAINTED;
            let stored = stored.filter(|bound| bound.taint().is_some_and(ends));
            let expected = stored.as_ref().and_then(|bound| bound.taint().cloned());
            let change = stored.map(|bound| self.change(host, bound.with_taint(None)));
            let commit = |change| self.commit_taint(change, expected.as_ref());
            let revision = change.as_ref().map_or(Ok(None), commit)?;
            Ok(change.map(|change| (change, revision)))
        });
        match &ended {
            Ok(Some((change, revision))) => self.applied(change, *revision),
            _ => {
                let mut written = self.lock();
                let (binding, extra, version) = kept;
                written.bindings.remove(host);
                written.extra.remove(host);
                written.versions.remove(host);
                written
                    .bindings
                    .extend(binding.map(|binding| (host.clone(), binding)));
                written
                    .extra
                    .extend(extra.map(|extra| (host.clone(), extra)));
                written
                    .versions
                    .extend(version.map(|version| (host.clone(), version)));
            }
        }
        ended.map(|ended| ended.map(|(change, _)| change))
    }

    /// Holds `change` as the store holds it once its writes are committed at `revision` (that
    /// of [`BindingStore::commit`]): the binding, and the version of its row, which the next
    /// write of the host is conditional on. Nothing in file mode, where what the file holds
    /// changes only when [`BindingStore::save`] writes it.
    pub(crate) fn applied(&self, change: &Change, revision: Option<Revision>) {
        if let Keeper::Store(..) = self.keeper {
            let mut written = self.lock();
            let read = written.versions.get(&change.host).copied();
            written.hold(&change.host, Some(&change.after));
            // A change that wrote leaves its commit's revision as the row's version; one with
            // no write left the row as it was, at the version read.
            let wrote = change.writes.first().and(revision);
            let version = wrote.map(|revision| Version(revision.0)).or(read);
            let host = &change.host;
            written
                .versions
                .extend(version.map(|version| (host.clone(), version)));
        }
    }
}

/// A clock at the Unix epoch, for the tests whose bindings hold no time of their own: over a
/// store whose clock is at its origin too, it reads no skew.
#[cfg(test)]
pub(crate) fn epoch_clock() -> SharedClock {
    SharedClock::new(js_types::ManualClock::new(
        js_types::Timestamp::from_millis(0),
    ))
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

#[path = "agent_state_scan.rs"]
mod scan;

#[cfg(test)]
#[path = "agent_state_clock_tests.rs"]
mod clock_tests;

#[cfg(test)]
#[path = "agent_state_conditional_tests.rs"]
mod conditional_tests;

#[cfg(test)]
#[path = "agent_state_unit_tests.rs"]
mod tests;
