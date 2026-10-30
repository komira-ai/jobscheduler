//! A bundle host that lives in memory: list files, the manifests the store holds, what each
//! needs, the numbers of the volume, and the answers of a fetch, all set by the test.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use js_agent::bundle::{BundleHost, Fetch, HostXcode, Needs};
use js_bundle::DottedVersion;
use js_treestore::mac::{VolumeFacts, check_securelevel, check_volumes};

/// One volume's facts, as `statfs` gives them.
pub fn apfs(fsid: i32) -> VolumeFacts {
    VolumeFacts {
        fs_type: "apfs".to_owned(),
        flags: 0,
        fsid: [fsid, 0],
    }
}

pub struct State {
    pub files: BTreeMap<PathBuf, String>,
    pub held: Result<Vec<String>, String>,
    pub needs: BTreeMap<String, Needs>,
    pub os: Result<DottedVersion, String>,
    pub xcodes: Vec<HostXcode>,
    pub used: Result<u64, String>,
    pub free: Result<u64, String>,
    pub fetch: Fetch,
    /// The volume of a path: the first prefix that matches.
    pub volumes: Vec<(PathBuf, VolumeFacts)>,
    pub securelevel: i64,
    /// Whether the agent can run a bundle.
    pub can_run: bool,
    /// Whether the server takes a long `bundle` capability value.
    pub long_values: bool,
    pub fetches: Vec<(String, String)>,
    pub started: Vec<(PathBuf, PathBuf)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            held: Ok(Vec::new()),
            needs: BTreeMap::new(),
            os: Ok(DottedVersion::parse("14.8").unwrap()),
            xcodes: Vec::new(),
            used: Ok(0),
            free: Ok(1 << 40),
            fetch: Fetch::Started,
            volumes: vec![(PathBuf::from("/"), apfs(1))],
            securelevel: -1,
            can_run: true,
            long_values: true,
            fetches: Vec::new(),
            started: Vec::new(),
        }
    }
}

/// The host the tests give the agent; the state is shared, so a test changes it after the
/// agent has it.
#[derive(Default)]
pub struct FakeHost {
    state: Mutex<State>,
}

impl FakeHost {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Changes the state.
    pub fn set(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
    }

    /// Reads the state.
    pub fn get<T>(&self, read: impl FnOnce(&State) -> T) -> T {
        read(&self.state.lock().unwrap())
    }

    pub fn file(&self, path: &str, text: &str) {
        self.set(|s| {
            s.files.insert(PathBuf::from(path), text.to_owned());
        });
    }
}

impl BundleHost for FakeHost {
    fn check_start(&self, store: &Path, scratch: &Path) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        state
            .started
            .push((store.to_path_buf(), scratch.to_path_buf()));
        let volumes = state.volumes.clone();
        let read = |path: &Path| {
            volumes
                .iter()
                .find(|(prefix, _)| path.starts_with(prefix))
                .map(|(_, facts)| facts.clone())
                .ok_or_else(|| std::io::Error::other("no such volume"))
        };
        check_volumes(store, scratch, read).map_err(|why| why.to_string())?;
        let level = state.securelevel;
        check_securelevel(&move || Ok(level)).map_err(|why| why.to_string())
    }

    fn read_file(&self, path: &Path) -> Result<String, String> {
        self.get(|s| s.files.get(path).cloned())
            .ok_or_else(|| format!("{}: No such file or directory", path.display()))
    }

    fn held(&self, _store: &Path) -> Result<Vec<String>, String> {
        self.get(|s| s.held.clone())
    }

    fn needs(&self, _store: &Path, digest: &str) -> Result<Needs, String> {
        self.get(|s| s.needs.get(digest).cloned())
            .ok_or_else(|| "no config".to_owned())
    }

    fn os_version(&self) -> Result<DottedVersion, String> {
        self.get(|s| s.os.clone())
    }

    fn xcodes(&self) -> Vec<HostXcode> {
        self.get(|s| s.xcodes.clone())
    }

    fn used_bytes(&self, _store: &Path) -> Result<u64, String> {
        self.get(|s| s.used.clone())
    }

    fn free_bytes(&self, _path: &Path) -> Result<u64, String> {
        self.get(|s| s.free.clone())
    }

    fn can_run(&self) -> bool {
        self.get(|s| s.can_run)
    }

    fn fetch(&self, _store: &Path, name: &str, digest: &str) -> Fetch {
        self.set(|s| s.fetches.push((name.to_owned(), digest.to_owned())));
        self.get(|s| s.fetch.clone())
    }
}

/// `sha256:` and the hex digit `c` 64 times.
pub fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
