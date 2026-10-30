//! The flags that configure native jobs, checked.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel, SANDBOX_EXEC};

use super::NativeConfig;
use crate::bundle::Placer;

/// Where the helper makes lease users' homes unless `--native-homes` says otherwise.
pub const DEFAULT_HOMES: &str = "/Users";

/// The directories a start's program may lie under unless `--native-tool-root` is given:
/// the system's own.
pub const DEFAULT_TOOL_ROOTS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Where the system keeps the Mac's auto-login password: no run may read it, whatever the
/// helper's own flags say. Spelled as the kernel resolves it (`/etc` is `/private/etc`).
const KCPASSWORD: &str = "/private/etc/kcpassword";

/// The native-job flags as the command line gave them.
#[derive(Debug, Clone, Copy)]
pub struct NativeFlags<'a> {
    /// `--native-helper-socket`.
    pub socket: Option<&'a Path>,
    /// `--native-homes`.
    pub homes: Option<&'a Path>,
    /// `--native-network`.
    pub network: Option<&'a str>,
    /// `--native-tool-root`, each.
    pub tool_roots: &'a [PathBuf],
    /// `--native-deny-read`, each.
    pub deny_read: &'a [PathBuf],
}

/// The checked native-job flags: the helper's socket and the runtime's configuration.
#[derive(Debug, Clone)]
pub struct NativeSettings {
    socket: PathBuf,
    config: NativeConfig,
    sandbox_exec: PathBuf,
    bundles: Option<Arc<Placer>>,
}

impl NativeSettings {
    /// Checks `flags`; `None` without `--native-helper-socket`. `denied` are the agent's
    /// own roots and files (state and scratch roots, token, instance and CA files), which
    /// no run may read; so are the helper's socket directory, `--native-deny-read` and
    /// the auto-login password. A path that exists is read-denied by the name the kernel
    /// resolves.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault: a native flag without the socket, a path that is
    /// not absolute, a home directory that cannot be opened, a network level that is none of
    /// `off`, `loopback` and `on`, or a root a profile cannot name.
    pub fn from_flags(flags: &NativeFlags<'_>, denied: &[&Path]) -> Result<Option<Self>, String> {
        let Some(socket) = flags.socket else {
            let given = [
                ("--native-homes", flags.homes.is_some()),
                ("--native-network", flags.network.is_some()),
                ("--native-tool-root", !flags.tool_roots.is_empty()),
            ];
            return match given.iter().find(|(_, given)| *given) {
                Some((flag, _)) => Err(format!("{flag}: needs --native-helper-socket")),
                None => Ok(None),
            };
        };
        absolute("--native-helper-socket", socket)?;
        let homes = flags.homes.unwrap_or_else(|| Path::new(DEFAULT_HOMES));
        absolute("--native-homes", homes)?;
        let canonical = fs::canonicalize(homes);
        let homes =
            canonical.map_err(|why| format!("--native-homes: {}: {why}", homes.display()))?;
        let net = match flags.network.unwrap_or("off") {
            "off" => Net::Off,
            "loopback" => Net::Loopback,
            "on" => Net::On,
            other => {
                return Err(format!(
                    "--native-network: {other:?} is not off, loopback or on"
                ));
            }
        };
        let tool_roots: Vec<PathBuf> = match flags.tool_roots {
            [] => DEFAULT_TOOL_ROOTS
                .iter()
                .map(|root| PathBuf::from(*root))
                .collect(),
            given => given.to_vec(),
        };
        for root in &tool_roots {
            absolute("--native-tool-root", root)?;
            if root.parent().is_none() || !root.is_dir() {
                return Err(format!(
                    "--native-tool-root: {} is not a directory below /",
                    root.display()
                ));
            }
        }
        let mut denies: Vec<PathBuf> = Vec::new();
        for path in denied {
            denies.push(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        }
        let socket_dir = socket.parent().unwrap_or(socket);
        denies.push(fs::canonicalize(socket_dir).unwrap_or_else(|_| socket_dir.to_path_buf()));
        for path in flags.deny_read {
            let found = fs::canonicalize(path);
            denies.push(
                found.map_err(|why| format!("--native-deny-read: {}: {why}", path.display()))?,
            );
        }
        denies.push(PathBuf::from(KCPASSWORD));
        let read_denies = denies
            .iter()
            .map(|path| AbsPath::new(path).map_err(|why| format!("{}: {why}", path.display())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Self {
            socket: socket.to_path_buf(),
            config: NativeConfig {
                homes,
                net,
                tool_roots,
                read_denies,
                read_model: ReadModel::Open,
            },
            sandbox_exec: PathBuf::from(SANDBOX_EXEC),
            bundles: None,
        }))
    }

    /// The same settings with `placer` as the bundle store of the runtime they make.
    ///
    /// # Errors
    ///
    /// The store lies under a path no native run may read.
    pub fn with_bundles(self, placer: Arc<Placer>) -> Result<Self, String> {
        Ok(Self {
            bundles: Some(placer),
            ..self
        })
    }

    /// The bundle store of the runtime, if there is one.
    #[must_use]
    pub fn bundles(&self) -> Option<&Arc<Placer>> {
        self.bundles.as_ref()
    }

    /// The same settings with `program` as the `sandbox-exec` the start-up self-test runs:
    /// a seam for tests.
    #[must_use]
    pub fn with_sandbox_exec(mut self, program: &Path) -> Self {
        self.sandbox_exec = program.to_path_buf();
        self
    }

    /// The `sandbox-exec` the start-up self-test runs.
    #[must_use]
    pub fn sandbox_exec(&self) -> &Path {
        &self.sandbox_exec
    }

    /// The helper's socket.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The runtime's configuration.
    #[must_use]
    pub fn config(&self) -> &NativeConfig {
        &self.config
    }
}

/// `path` is absolute, or the error names `flag`.
fn absolute(flag: &str, path: &Path) -> Result<(), String> {
    match path.is_absolute() {
        true => Ok(()),
        false => Err(format!(
            "{flag}: {} is not an absolute path",
            path.display()
        )),
    }
}
