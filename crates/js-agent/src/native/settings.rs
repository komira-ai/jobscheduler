//! The flags that configure native jobs, checked.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_agent_wire::PortRange;
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

use super::NativeConfig;
use super::accounts::Accounts;
use super::fence::PRIVACY_ONLY_PLACES;
use super::mounts::MountRoot;
use super::network::Ceiling;
use super::pf::{Enforcer, PfFlags, PfPolicy};
use super::put::PutConfig;
use crate::bundle::Placer;
use crate::logs::{DEFAULT_FILE_CAP, LogPlan};
use crate::mac_accounts::Slots;
use crate::mac_usage::Series;
use crate::xcode::Xcode;

/// Where the account pool makes its accounts' homes: the one value `--native-homes` takes.
pub const DEFAULT_HOMES: &str = "/Users";

/// The directories a start's program may lie under unless `--native-tool-root` is given:
/// the system's own.
pub const DEFAULT_TOOL_ROOTS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// The largest file a native run may write unless `--native-max-file-mib` says otherwise.
pub const DEFAULT_MAX_FILE_MIB: u64 = 4096;

/// The smallest `--native-max-file-mib`: a run's largest file is its one output (a build's
/// pack, at most 128 MiB by default), which the run must be able to write and the removal to
/// collect.
pub const MIN_MAX_FILE_MIB: u64 = 128;

/// The most processes a native run's user may have unless `--native-max-processes` says
/// otherwise.
pub const DEFAULT_MAX_PROCESSES: u32 = 512;

/// Where the system keeps the Mac's auto-login password: no run may read it. Spelled as the kernel resolves it (`/etc` is `/private/etc`).
const KCPASSWORD: &str = "/private/etc/kcpassword";

/// The native-job flags as the command line gave them.
#[derive(Debug, Clone, Copy)]
pub struct NativeFlags<'a> {
    /// `--native-accounts`: the switch that turns native jobs on.
    pub accounts: Option<&'a Path>,
    /// `--native-account-range`, as given.
    pub account_range: Option<&'a str>,
    /// `--native-account-ready`.
    pub account_ready: Option<usize>,
    /// `--native-homes`.
    pub homes: Option<&'a Path>,
    /// `--native-network`.
    pub network: Option<&'a str>,
    /// `--native-tool-root`, each.
    pub tool_roots: &'a [PathBuf],
    /// `--native-deny-read`, each.
    pub deny_read: &'a [PathBuf],
    /// `--native-max-file-mib`.
    pub max_file_mib: Option<u64>,
    /// `--native-max-processes`.
    pub max_processes: Option<u32>,
    /// `--native-pfctl` and the `--native-pf-*` flags.
    pub pf: PfFlags<'a>,
}

/// The checked native-job flags: the runtime's configuration.
#[derive(Debug, Clone)]
pub struct NativeSettings {
    config: NativeConfig,
    bundles: Option<Arc<Placer>>,
    /// The Xcodes the host holds, which a job start may name by build.
    xcodes: Vec<Xcode>,
    /// Live logs: the file cap of a run with a sink and the host's credential.
    logs: LogPlan,
    /// The put ingest runs' outputs are committed to (`native/put.rs`).
    put: Option<PutConfig>,
    /// The series each run records (`--cpu-series`, `--memory-series`).
    series: Series,
}

impl NativeSettings {
    /// Checks `flags`; `None` without `--native-accounts`. `denied` are the agent's own
    /// roots and files (state and scratch roots, token, instance and CA files), which no run
    /// may read; so are `--native-accounts`, `--native-deny-read`, the auto-login password
    /// and the places only the privacy controls guard ([`PRIVACY_ONLY_PLACES`]). Each is
    /// read-denied by the name the kernel resolves, a path that is not there yet by the real
    /// path of its deepest ancestor that is (see [`real_or_planned`]).
    ///
    /// # Errors
    ///
    /// The text names the flag at fault: a native flag without `--native-accounts`, no
    /// `--native-account-range` or one the pool refuses, more ready accounts than slots, a
    /// path that is not absolute, a home directory other than the pool's, a network level
    /// that is none of `off`, `loopback`, `on` and `open`, a `--native-pf-*` flag beside
    /// `open` (or beside any value but `on`), a limit of 0 or one too large, or a root a
    /// profile cannot name.
    pub fn from_flags(
        flags: &NativeFlags<'_>,
        denied: &[&Path],
        journal: &Path,
    ) -> Result<Option<Self>, String> {
        let Some(state) = flags.accounts else {
            let given = [
                ("--native-account-range", flags.account_range.is_some()),
                ("--native-account-ready", flags.account_ready.is_some()),
                ("--native-homes", flags.homes.is_some()),
                ("--native-network", flags.network.is_some()),
                ("--native-tool-root", !flags.tool_roots.is_empty()),
                ("--native-deny-read", !flags.deny_read.is_empty()),
                ("--native-max-file-mib", flags.max_file_mib.is_some()),
                ("--native-max-processes", flags.max_processes.is_some()),
                ("--native-pfctl", flags.pf.pfctl.is_some()),
                ("--native-pf-anchor", flags.pf.anchor.is_some()),
                ("--native-pf-state-dir", flags.pf.state_dir.is_some()),
                ("--native-pf-uid-start", flags.pf.uid_start.is_some()),
                ("--native-pf-uid-count", flags.pf.uid_count.is_some()),
                ("--native-pf-endpoint", !flags.pf.endpoints.is_empty()),
            ];
            return match given.iter().find(|(_, given)| *given) {
                Some((flag, _)) => Err(format!("{flag}: needs --native-accounts")),
                None => Ok(None),
            };
        };
        absolute("--native-accounts", state)?;
        let accounts = accounts_of(state, flags.account_range, flags.account_ready)?;
        let homes = flags.homes.unwrap_or_else(|| Path::new(DEFAULT_HOMES));
        absolute("--native-homes", homes)?;
        let canonical = fs::canonicalize(homes);
        let homes =
            canonical.map_err(|why| format!("--native-homes: {}: {why}", homes.display()))?;
        if fs::canonicalize(DEFAULT_HOMES).ok().as_deref() != Some(homes.as_path()) {
            return Err(format!(
                "--native-homes: {}: the account pool makes its homes in {DEFAULT_HOMES}",
                homes.display()
            ));
        }
        let given = flags.network.unwrap_or(Ceiling::Off.name());
        let network = Ceiling::parse(given).ok_or_else(|| {
            format!("--native-network: {given:?} is not off, loopback, on or open")
        })?;
        // `open` filters nothing, so a pf flag beside it is a mistake: refused by name rather
        // than read as asking for a filter the agent would not check.
        if let (Ceiling::Open, Some(flag)) = (network, flags.pf.first_given()) {
            return Err(format!(
                "{flag}: not valid with --native-network open, which sets no packet filter \
                 (the pf flags limit the network of --native-network on)"
            ));
        }
        let egress = PfPolicy::from_flags(&flags.pf, network == Ceiling::On)?;
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
        let mut denies: Vec<PathBuf> = denied.iter().map(|path| real_or_planned(path)).collect();
        denies.push(real_or_planned(state));
        for path in flags.deny_read {
            let found = fs::canonicalize(path);
            denies.push(
                found.map_err(|why| format!("--native-deny-read: {}: {why}", path.display()))?,
            );
        }
        denies.push(PathBuf::from(KCPASSWORD));
        denies.extend(PRIVACY_ONLY_PLACES.iter().map(PathBuf::from));
        let mib = flags.max_file_mib.unwrap_or(DEFAULT_MAX_FILE_MIB);
        let max_processes = flags.max_processes.unwrap_or(DEFAULT_MAX_PROCESSES);
        if mib == 0 || max_processes == 0 {
            return Err("--native-max-file-mib and --native-max-processes: above 0".to_owned());
        }
        if mib < MIN_MAX_FILE_MIB {
            return Err(format!(
                "--native-max-file-mib: {mib} is below {MIN_MAX_FILE_MIB}, the largest output \
                 a build writes by default"
            ));
        }
        let max_file_bytes = mib
            .checked_mul(1 << 20)
            .ok_or_else(|| format!("--native-max-file-mib: {mib} is too large"))?;
        // The run's process limit is this and one, for the supervisor (`NativeConfig`).
        if max_processes == u32::MAX {
            return Err(format!(
                "--native-max-processes: {max_processes} is too large"
            ));
        }
        let read_denies = denies
            .iter()
            .map(|path| AbsPath::new(path).map_err(|why| format!("{}: {why}", path.display())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Self {
            config: NativeConfig {
                accounts,
                homes,
                network,
                tool_roots,
                read_denies,
                mount_roots: Vec::new(),
                host_ports: Vec::new(),
                read_model: ReadModel::Open,
                journal: journal.to_path_buf(),
                max_file_bytes,
                max_processes,
                egress: egress.map(|policy| Arc::new(policy) as Arc<dyn Enforcer>),
                full_disk_access: FullDiskAccess::Undetermined,
            },
            bundles: None,
            xcodes: Vec::new(),
            logs: LogPlan::new(DEFAULT_FILE_CAP),
            put: None,
            series: Series::default(),
        }))
    }

    /// The same settings with `placer` as the bundle store of the runtime they make.
    ///
    /// # Errors
    ///
    /// The store lies under a path no native run may read: every bundle job would fail at
    /// its first read of the toolchain.
    pub fn with_bundles(self, placer: Arc<Placer>) -> Result<Self, String> {
        let store = placer.root();
        let covered = |denied: &AbsPath| store.starts_with(denied.as_str());
        if let Some(denied) = self
            .config
            .read_denies
            .iter()
            .find(|denied| covered(denied))
        {
            return Err(format!(
                "--bundle-store: {} lies under {}, which no native run may read",
                store.display(),
                denied.as_str()
            ));
        }
        Ok(Self {
            bundles: Some(placer),
            ..self
        })
    }

    /// The same settings with `roots` (`--mount-root`, `--mount-root-ro`) as the roots a
    /// job's mounts lie beneath, and each of them but the trees root a read deny of every run
    /// ([`crate::native_fs::denied_roots`]): a run reads only the mounts it was given, which
    /// its profile grants back. The trees root is left out: the profile denies reads below it
    /// and grants back the run's own trees before its read denies, which a later deny of the
    /// root would hide.
    ///
    /// # Errors
    ///
    /// A root the profile cannot name.
    pub fn with_mount_roots(mut self, roots: Vec<MountRoot>) -> Result<Self, String> {
        for root in &roots {
            AbsPath::new(&root.path)
                .map_err(|why| format!("--mount-root: {}: {why}", root.path.display()))?;
        }
        for root in crate::native_fs::denied_roots(&roots) {
            let denied = AbsPath::new(&root.path)
                .map_err(|why| format!("--mount-root: {}: {why}", root.path.display()))?;
            if !self.config.read_denies.contains(&denied) {
                self.config.read_denies.push(denied);
            }
        }
        self.config.mount_roots = roots;
        Ok(self)
    }

    /// The same settings with `allowed` (`--host-port`, checked by
    /// [`crate::host_ports::allowed`]) as the ports of the host's loopback a job may reach.
    #[must_use]
    pub fn with_host_ports(mut self, allowed: Vec<PortRange>) -> Self {
        self.config.host_ports = allowed;
        self
    }

    /// The same settings with `put` (`--native-put-root`, `--native-put-user`, checked by
    /// [`super::put_of`]) as the put ingest of the runtime they make.
    #[must_use]
    pub fn with_put(self, put: Option<PutConfig>) -> Self {
        Self { put, ..self }
    }

    /// The put ingest of the runtime these settings make, if any.
    #[must_use]
    pub fn put(&self) -> Option<&PutConfig> {
        self.put.as_ref()
    }

    /// The same settings with each run recording the series `series` names.
    #[must_use]
    pub fn with_series(self, series: Series) -> Self {
        Self { series, ..self }
    }

    /// The series each run of the runtime these settings make records.
    #[must_use]
    pub fn series(&self) -> Series {
        self.series
    }

    /// The same settings with `file_cap` (`--log-file-cap`) as the most bytes of each log file
    /// of a run with a sink that its uploader reads.
    #[must_use]
    pub fn with_log_file_cap(mut self, file_cap: u64) -> Self {
        self.logs.file_cap = file_cap;
        self
    }

    /// Live logs: the file cap of a run with a sink, and the cell of the host's credential,
    /// which the session fills from the heartbeat replies and the runtime's uploaders read.
    #[must_use]
    pub fn logs(&self) -> &LogPlan {
        &self.logs
    }

    /// The same settings with `state` as whether the agent holds Full Disk Access, as it
    /// found at start ([`super::grant::ask`]). The flags alone say nothing of it, so
    /// settings are made `Undetermined`, which refuses every native job.
    #[must_use]
    pub fn with_full_disk_access(mut self, state: FullDiskAccess) -> Self {
        self.config.full_disk_access = state;
        self
    }

    /// The same settings with `xcodes` as the Xcodes the runtime runs a job with, by build.
    #[must_use]
    pub fn with_xcodes(self, xcodes: Vec<Xcode>) -> Self {
        Self { xcodes, ..self }
    }

    /// The Xcodes of the runtime.
    #[must_use]
    pub fn xcodes(&self) -> &[Xcode] {
        &self.xcodes
    }

    /// The bundle store of the runtime, if there is one.
    #[must_use]
    pub fn bundles(&self) -> Option<&Arc<Placer>> {
        self.bundles.as_ref()
    }

    /// The same settings with `placer` as the bundle store ([`NativeSettings::with_bundles`])
    /// and the read model `read_model` names (`--read-model`, checked with the bundle flags,
    /// which need `--bundle-store`: with no store open no run pins a bundle, and the open model
    /// is all there is) as that of the profile of a run that pins a bundle.
    ///
    /// # Errors
    ///
    /// As [`NativeSettings::with_bundles`].
    pub fn with_store(self, placer: Arc<Placer>, read_model: Option<&str>) -> Result<Self, String> {
        let mut settings = self.with_bundles(placer)?;
        settings.config.read_model = super::read_model_named(read_model);
        Ok(settings)
    }

    /// The same settings with `enforcer` as the check of the pf anchor: a seam for tests.
    #[must_use]
    pub fn with_enforcer(mut self, enforcer: Arc<dyn Enforcer>) -> Self {
        self.config.egress = Some(enforcer);
        self
    }

    /// The runtime's configuration.
    #[must_use]
    pub fn config(&self) -> &NativeConfig {
        &self.config
    }
}

/// The account pool of `--native-accounts` (`state`): its slots, which
/// `--native-account-range` must give, and how many accounts it keeps ready (by default one, at
/// most one per slot).
fn accounts_of(
    state: &Path,
    range: Option<&str>,
    ready: Option<usize>,
) -> Result<Accounts, String> {
    let range = range.ok_or("--native-accounts: needs --native-account-range")?;
    let slots: Slots = range
        .parse()
        .map_err(|why| format!("--native-account-range: {why}"))?;
    let ready = ready.unwrap_or(1);
    let count = usize::try_from(slots.count()).unwrap_or(usize::MAX);
    if ready > count {
        return Err(format!(
            "--native-account-ready: {ready} is more than the {count} slots of \
             --native-account-range"
        ));
    }
    Ok(Accounts {
        state: state.to_path_buf(),
        slots,
        ready,
    })
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

/// The name the kernel resolves `path` to: its real path, or, for a path that is not there (the
/// token file of a host that joins by its certificate), the real path of its deepest ancestor
/// that is, with the rest of `path` after it. So `/etc/js-agent/token` is denied as
/// `/private/etc/js-agent/token` whether the file exists or not: a profile cannot name `/etc`,
/// which is a link, and a run that makes the file later is still denied it. `path` as it is when
/// no ancestor resolves (a relative path), for the profile's check to refuse by name.
fn real_or_planned(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|at| {
            let real = fs::canonicalize(at).ok()?;
            let rest = path.strip_prefix(at).ok()?;
            Some(match rest.as_os_str().is_empty() {
                true => real,
                false => real.join(rest),
            })
        })
        .unwrap_or_else(|| path.to_path_buf())
}
