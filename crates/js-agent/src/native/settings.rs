//! The flags that configure native jobs, checked.

use std::fs;
use std::path::{Path, PathBuf};

use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};

use super::NativeConfig;

/// Where the helper makes lease users' homes unless `--native-homes` says otherwise.
pub const DEFAULT_HOMES: &str = "/Users";

/// The directories a start's program may lie under unless `--native-tool-root` is given:
/// the system's own.
pub const DEFAULT_TOOL_ROOTS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

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
}

/// The checked native-job flags: the helper's socket and the runtime's configuration.
#[derive(Debug, Clone)]
pub struct NativeSettings {
    socket: PathBuf,
    config: NativeConfig,
}

impl NativeSettings {
    /// Checks `flags`; `None` without `--native-helper-socket`. `denied` are the agent's
    /// own roots (state and scratch), which no run may read.
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
        }
        let read_denies = denied
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
        }))
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
