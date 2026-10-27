//! The file of host bindings beside the lease file.
//!
//! A lease on a registered host survives a restart because the lease file names the
//! host; what it cannot hold is which credential and instance the host id was bound to
//! when the host was admitted. Without that, the first hello for the id after a restart
//! would take the lease from anyone holding a join token. The server writes the
//! bindings to a file of its own, `<state file>.agents`, and hands them back to the
//! registry when it seats the lease file's bookings.
//!
//! The format is TOML, written whole through a temporary file and renamed, like the
//! lease file, and readers ignore keys they do not know:
//!
//! ```toml
//! version = 1
//!
//! [[host]]
//! id = "builder-7"
//! credential = "<64 hex digits: SHA-256 of the join token>"
//! instance_provider = "aws"      # with instance_id, when the first hello named one
//! instance_id = "i-0abc"
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use js_agent_wire::InstanceIdentity;
use js_registry::{Binding, Credential};
use js_types::HostId;
use serde::{Deserialize, Serialize};

use crate::config::read_private;
use crate::error::StartupError;

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
}

impl Entry {
    fn of(id: &HostId, binding: &Binding) -> Self {
        let instance = binding.instance();
        Self {
            id: id.to_string(),
            credential: binding.credential().to_hex(),
            instance_provider: instance.map(|instance| instance.provider.clone()),
            instance_id: instance.map(|instance| instance.instance_id.clone()),
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
        Ok((id, Binding::new(credential, instance)))
    }
}

/// Reads the bindings of the file at `path`; none when there is no file. The file must
/// be private like the token files (it names what each host is bound to), and one that
/// cannot be read as written is a refusal rather than a start with no bindings, which
/// would reopen the gap it exists to close.
pub(crate) fn load(path: &Path) -> Result<BTreeMap<HostId, Binding>, StartupError> {
    let bytes = match fs::metadata(path) {
        Ok(_) => read_private(path, FLAG)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => {
            return Err(StartupError::new(
                FLAG,
                format!("{}: {err}", path.display()),
            ));
        }
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

/// Writes the bindings when they differ from what the file holds.
pub(crate) struct BindingStore {
    path: PathBuf,
    written: Mutex<BTreeMap<HostId, Binding>>,
}

impl BindingStore {
    /// A store over `path`, whose file holds `current` (what [`load`] read).
    pub(crate) fn new(path: PathBuf, current: BTreeMap<HostId, Binding>) -> Self {
        Self {
            path,
            written: Mutex::new(current),
        }
    }

    /// Makes the file hold what `current` returns, which is called with the store's lock
    /// held so that the last save to write has read the newest bindings. Nothing is
    /// written when the file already holds them; a failed
    /// write leaves the old file and is retried by the next call, which finds the
    /// bindings still different.
    pub(crate) fn save<F>(&self, current: F) -> io::Result<()>
    where
        F: FnOnce() -> BTreeMap<HostId, Binding>,
    {
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        let bindings = current();
        if *written == bindings {
            return Ok(());
        }
        replace(&self.path, render(&bindings)?.as_bytes())?;
        *written = bindings;
        Ok(())
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
/// fsynced beside it, renamed over it, and the directory fsynced, so a reader sees the
/// old file or the new one.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let temp = path.with_file_name(name);
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
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
}
