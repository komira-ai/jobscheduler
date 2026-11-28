//! hostkeeper's daemon for a package (the agent's, or the host group's config package: a
//! [`Service`] each), as its order path runs on a Mac: hostkeeper's own
//! installer (`MacosPkg`), version directory (`stage_filled`), seal (`Staged::seal`), switch of
//! `current` (`Placer::switch_current`), launchd step launcher (`LaunchdLauncher::system`) and
//! step runner (`StepRunner`), at the revision the workspace pins. What the daemon keeps in its
//! own crate (`pub(crate)` there) is the glue between them, written here as it is there:
//! `Repository::fill_sealed` and `stage_pkg` (the stage, then the seal), `activate_pkg` (the
//! installer's `activate`, then the switch of `current`) and `back` for a first install (the
//! `run` jobs stopped, the installer's `remove`, then `current` removed when it names the target),
//! and `update_order` (the hook's program mapped into the sealed tree, its arguments, bounds and
//! params from the manifest the release writes for the Mac, and for an update the release it
//! replaces and its hook). A daemon rolls an update back by nothing here: `Back` of an update is
//! an error, since the test that runs one expects it to succeed.
//!
//! Not reproduced: the fetch of the package by its pin (the test hands the installer the file),
//! the `target.json` the daemon writes beside the tree (nothing of the steps reads it), the
//! daemon's quarantine and code-signature check of the staged tree, `placed.json` (the
//! installer's records are kept in memory, as hostkeeper's own installer test keeps them, one
//! set for every package of the host) and
//! the signature of the package: the runner cannot sign one, so `pkgutil --check-signature` of
//! this package is answered as signed by a leaf the signers list names ([`Signed`]), as
//! hostkeeper's own installer test answers it. Every other tool call is the Mac's.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_hook::Params;
use hostkeeper_install::host::Host;
use hostkeeper_install::lock::InstallerLock;
use hostkeeper_install::macos_pkg::record::{PkgRecord, RecordError, Records};
use hostkeeper_install::macos_pkg::signers::{Fingerprint, Signers};
use hostkeeper_install::macos_pkg::{Bounds as PkgBounds, MacosPkg, Order};
use hostkeeper_install::tools::{ToolError, ToolOutput};
use hostkeeper_install::version::{FillOrder, HostFs, Owner, stage_filled};
use hostkeeper_install::{HostTools, Tool, Tools};
use hostkeeper_launchd::{
    Label, LaunchctlProcess, LaunchdLauncher, LaunchdUnitControl, SysctlBootId,
};
use hostkeeper_placed::{Placer, RealFs, Roots, UnitControl};
use hostkeeper_steps::{
    Bounds, Captured, Finished, PreviousHook, RecordStore, Staged, StepLog, StepRunner, Switch,
    SwitchError, SystemClock, Toward, UpdateOrder,
};
use sha2::{Digest as _, Sha256};

/// The agent's service, as the Mac's hostgroup names it.
pub const SERVICE: &str = "js-agent";

/// The manifest's `run` of the agent: the agent and the pf anchor's job.
pub const RUN: [&str; 2] = ["js-agent", "js-agent.pf-pool"];

/// The config package's service, as the Mac's hostgroup names it: `js-agent-config-<group>`.
pub const CONFIG_SERVICE: &str = "js-agent-config-mac-build";

/// The launcher the config package carries, at the path it installs it at.
pub const CONFIG_HOOK: &str = "/private/etc/js-agent/config-hook";

/// What differs between the packages the daemon installs: the names of the manifest, the hook
/// with its bounds, and the params.
pub struct Service {
    /// The service's name.
    pub name: &'static str,
    /// The package's identifier (the manifest's `package`).
    identifier: &'static str,
    /// The manifest's `run`.
    run: &'static [&'static str],
    /// The program of the manifest's hook command, an absolute path the package installs.
    hook_program: &'static str,
    /// The same program in the sealed tree, which is where the daemon runs it from.
    hook_binary: &'static str,
    /// The hook command's arguments after its program.
    hook_args: &'static [&'static str],
    /// The manifest's hook bounds.
    bounds: Bounds,
    /// The manifest's `params`.
    params: &'static str,
}

/// The agent's package.
pub const AGENT: Service = Service {
    name: SERVICE,
    identifier: "com.komira.js-agent",
    run: &RUN,
    hook_program: "/Library/Application Support/js-agent/bin/js-agent",
    hook_binary: "tree/Library/Application Support/js-agent/bin/js-agent",
    hook_args: &["update-hook"],
    bounds: BOUNDS,
    params: MAC_PARAMS,
};

/// The host group's config package. Its manifest (packaging/js-agent-macos/README.md) names the
/// installed agent as its hook program, but hostkeeper at the pinned revision runs a hook from
/// the package's sealed tree alone and refuses one the package does not install, so the
/// test's package carries [`CONFIG_HOOK`], a launcher that runs the installed
/// `js-agent config-hook` with the arguments it is given.
pub const CONFIG: Service = Service {
    name: CONFIG_SERVICE,
    identifier: CONFIG_SERVICE,
    run: &[],
    hook_program: CONFIG_HOOK,
    hook_binary: "tree/private/etc/js-agent/config-hook",
    hook_args: &["config-hook"],
    bounds: CONFIG_BOUNDS,
    params: CONFIG_PARAMS,
};

/// hostkeeper's state directory on a Mac (`placed.json`, `facts.json`, `identity/`).
pub const STATE_DIR: &str = "/var/lib/hostkeeper";

/// The daemon's packages directory, its default `--packages-dir`, spelt as the daemon is given
/// it: under `/var`, which on a Mac is a link to `/private/var`.
pub const PACKAGES: &str = "/var/lib/hostkeeper/packages";

/// The launchd directory the package's plists are installed in.
const UNIT_DIR: &str = "/Library/LaunchDaemons";

/// The leaf certificate's SHA-256 the answered `--check-signature` names, and the only signer.
const LEAF: &str = "a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9";

/// The most `launchctl` is waited for by the switch: a bound on a hang, not a measure of time.
const LAUNCHCTL_BOUND: Duration = Duration::from_secs(60);

/// The manifest's `installerBounds`.
const INSTALLER_BOUNDS: PkgBounds = PkgBounds {
    stage: Duration::from_secs(600),
    activate: Duration::from_secs(300),
    remove: Duration::from_secs(300),
};

/// The manifest's hook bounds (`timeoutSecs`, `verifyDeadlineSecs`, `cleanupRetries`).
const BOUNDS: Bounds = Bounds {
    prepare_secs: 3900,
    cutover_secs: 4320,
    verify_secs: 615,
    rollback_secs: 8220,
    cleanup_secs: 120,
    verify_deadline_secs: 9000,
    promotion_ceiling_secs: u64::MAX,
    cleanup_retries: 3,
};

/// The Mac manifest's `params`, as `cargo xtask release desired` writes them
/// (crates/xtask/src/release/desired/manifest.rs, `MAC_PARAMS`).
const MAC_PARAMS: &str = r#"unit                  = "js-agent"
pf_unit               = "js-agent.pf-pool"
scratch_root          = "/var/db/js-agent/scratch/runs"
state_dir             = "/Library/Application Support/js-agent/update"
release_link          = "/var/lib/hostkeeper/packages/js-agent/current"
flags_file            = "/etc/js-agent/flags"
token_file            = "/etc/js-agent/token"
state_root            = "/var/db/js-agent/state"
runtime               = "native"
cordon_lease_secs     = 300
cordon_max_secs       = 17355
drain_wait_secs       = 3600
defer_retry_secs      = 300
prepare_timeout_secs  = 3900
cutover_timeout_secs  = 4320
verify_timeout_secs   = 615
rollback_timeout_secs = 8220
cleanup_timeout_secs  = 120
verify_deadline_secs  = 9000
"#;

/// The config package's hook bounds (the `timeoutSecs` of its manifest).
const CONFIG_BOUNDS: Bounds = Bounds {
    prepare_secs: 60,
    cutover_secs: 420,
    verify_secs: 300,
    rollback_secs: 420,
    cleanup_secs: 60,
    verify_deadline_secs: 600,
    promotion_ceiling_secs: u64::MAX,
    cleanup_retries: 3,
};

/// The config package's `params`, as packaging/js-agent-macos/README.md gives them.
const CONFIG_PARAMS: &str = r#"unit                  = "js-agent"
flags_file            = "/etc/js-agent/flags"
token_file            = "/etc/js-agent/token"
state_root            = "/var/db/js-agent/state"
state_dir             = "/var/db/js-agent-config"
cutover_timeout_secs  = 420
verify_timeout_secs   = 300
rollback_timeout_secs = 420
"#;

/// `pkgutil --check-signature`'s answer for a package signed by [`LEAF`] whose chain the Mac
/// trusts, in the form `hostkeeper_install::macos_pkg::signature` reads.
fn signed_answer(package: &str) -> String {
    let hex_line = |hex: &str| {
        hex.as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).to_uppercase())
            .collect::<Vec<_>>()
            .join(" ")
    };
    [
        format!("Package \"{package}\":"),
        "   Status: signed by a certificate trusted by macOS".to_owned(),
        "   Certificate Chain:".to_owned(),
        "    1. js-agent first-install test installer".to_owned(),
        "       SHA256 Fingerprint:".to_owned(),
        format!("           {}", hex_line(&LEAF[..32])),
        format!("           {}", hex_line(&LEAF[32..])),
        format!("       {}", "-".repeat(72)),
        String::new(),
    ]
    .join("\n")
}

/// The Mac's tools, but for `pkgutil --check-signature` of the package, answered as signed.
struct Signed {
    package: String,
}

impl Tools for Signed {
    fn run(
        &self,
        tool: Tool,
        args: &[String],
        deadline: Duration,
    ) -> Result<ToolOutput, ToolError> {
        let asked = args.first().map(String::as_str) == Some("--check-signature");
        if tool == Tool::Pkgutil && asked && args.get(1) == Some(&self.package) {
            return Ok(ToolOutput {
                code: Some(0),
                stdout: signed_answer(&self.package).into_bytes(),
                stderr: Vec::new(),
            });
        }
        HostTools.run(tool, args, deadline)
    }
}

/// The installer's records, in memory.
#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, PkgRecord>>);

impl Records for Memory {
    fn load(&self, service: &ServiceName) -> Result<Option<PkgRecord>, RecordError> {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(held.get(service.as_str()).cloned())
    }

    fn save(&self, service: &ServiceName, record: &PkgRecord) -> Result<(), RecordError> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.insert(service.as_str().to_owned(), record.clone());
        Ok(())
    }

    fn holder(
        &self,
        identifier: &str,
        service: &ServiceName,
    ) -> Result<Option<ServiceName>, RecordError> {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(held
            .iter()
            .find(|(name, record)| *name != service.as_str() && record.identifier == identifier)
            .map(|(name, _)| ServiceName::new(name).expect("a service name")))
    }
}

/// One step as the launcher ran it: what `LaunchdLauncher` gives its log.
#[derive(Clone, Debug)]
pub struct Ran {
    /// The step's launchd label.
    pub unit: String,
    pub code: Option<i32>,
    /// The last line of its standard output: the hook's report.
    pub last_line: Option<String>,
    /// Its standard error: the hook's log.
    pub stderr: String,
}

/// The daemon's ports for a package: its installer over the Mac's tools, and the package.
pub struct Daemon {
    spec: &'static Service,
    host: Host,
    tools: Signed,
    records: Arc<Memory>,
    lock: InstallerLock,
    signers: Signers,
    order: Order,
    /// The package's SHA-256, lower-case hex: the order's pin.
    digest: String,
    pkg: PathBuf,
    /// Every step the launcher ran, in order.
    pub ran: Arc<Mutex<Vec<Ran>>>,
}

impl Daemon {
    /// The daemon of a Mac ordered to install the package `given` of `spec` as `version`.
    pub fn new(spec: &'static Service, given: &Path, version: &str) -> Daemon {
        Daemon::build(spec, given, version, Arc::default(), Arc::default())
    }

    /// The same daemon, ordered to install another package, of `spec` (the same service at
    /// another version, or another service): the installer's records and the log of the steps
    /// are the ones this daemon keeps, as one daemon keeps them for the host.
    pub fn another(&self, spec: &'static Service, given: &Path, version: &str) -> Daemon {
        let records = Arc::clone(&self.records);
        Daemon::build(spec, given, version, records, Arc::clone(&self.ran))
    }

    fn build(
        spec: &'static Service,
        given: &Path,
        version: &str,
        records: Arc<Memory>,
        ran: Arc<Mutex<Vec<Ran>>>,
    ) -> Daemon {
        let bytes = fs::read(given).unwrap_or_else(|e| panic!("read {}: {e}", given.display()));
        fs::create_dir_all(PACKAGES).expect("the packages directory");
        // The daemon stages its own root-owned download of the package (its package store), never
        // a file another user owns: the installer hard-links the version's `artifact` to it, and
        // the check before every step refuses an entry of the tree owned by another user.
        let pkg = own_copy(given, &bytes);
        let pkg = pkg.as_path();
        Daemon {
            spec,
            host: Host::mac(),
            tools: Signed {
                package: pkg.to_str().expect("a UTF-8 path").to_owned(),
            },
            records,
            lock: InstallerLock::new(&Path::new(STATE_DIR).join("installer.lock")),
            signers: Signers::new(vec![Fingerprint::parse(LEAF).expect("64 hex")])
                .expect("one signer"),
            order: Order {
                service: ServiceName::new(spec.name).expect("a service name"),
                version: Version::new(version).expect("a version"),
                identifier: spec.identifier.to_owned(),
                run: spec
                    .run
                    .iter()
                    .map(|label| Label::parse(label).expect("a label"))
                    .collect(),
                bounds: INSTALLER_BOUNDS,
                hook: Some(spec.hook_program.to_owned()),
            },
            digest: hex::encode(Sha256::digest(&bytes)),
            pkg: pkg.to_owned(),
            ran,
        }
    }

    fn installer(&self) -> MacosPkg<'_> {
        MacosPkg {
            tools: &self.tools,
            host: &self.host,
            records: &*self.records,
            lock: &self.lock,
            signers: &self.signers,
        }
    }

    /// The version this daemon installs.
    pub fn version(&self) -> &str {
        self.order.version.as_str()
    }

    /// `stage_pkg` then `fill_sealed`: the package staged into a new version directory, or the
    /// one already on the host for the same digest, then sealed. `true` with a directory this
    /// call made, `false` with one it found.
    pub fn stage(&self) -> (Staged, bool) {
        let digest = Sha256Hex::parse(&self.digest).expect("a SHA-256");
        let order = FillOrder {
            service: &self.order.service,
            version: &self.order.version,
            package_digest: &digest,
        };
        let packages = HostFs::open(Path::new(PACKAGES), Owner::ROOT).expect("open packages");
        let mut same = |_: &Path| Ok::<(), String>(());
        let mut fill = |stage: &Path| {
            self.installer()
                .stage(&self.order, &self.pkg, stage)
                .map_err(|failure| format!("{failure:?}"))
        };
        let filled = stage_filled(&order, &packages, &mut same, &mut fill)
            .unwrap_or_else(|e| panic!("the stage of {}: {e:?}", self.pkg.display()));
        let staged = Staged::seal(&filled.dir).unwrap_or_else(|e| panic!("the seal: {e:?}"));
        (staged, filled.fresh)
    }

    /// The order the daemon runs the hook's steps for: `update_order` of a first install.
    pub fn order(&self, staged: &Staged, order_try: u64) -> UpdateOrder {
        UpdateOrder {
            service: self.spec.name.to_owned(),
            target_version: self.version().to_owned(),
            previous_version: None,
            artifact_digest: self.digest.clone(),
            staged: staged.clone(),
            previous: None,
            hook_binary: PathBuf::from(self.spec.hook_binary),
            hook_args: self
                .spec
                .hook_args
                .iter()
                .map(|a| (*a).to_owned())
                .collect(),
            previous_hook: None,
            bounds: self.spec.bounds.clone(),
            declared_params: Params::parse_declared(self.spec.params).expect("the Mac params"),
            min_updater_protocol: None,
            order_try: Some(order_try),
            builtin_steps: false,
        }
    }

    /// `update_order` of an update from the release `replaced` (its version and its sealed
    /// directory), whose hook is the same program of that release's tree.
    pub fn order_over(
        &self,
        staged: &Staged,
        order_try: u64,
        replaced: (&str, &Staged),
    ) -> UpdateOrder {
        let (version, previous) = replaced;
        UpdateOrder {
            previous_version: Some(version.to_owned()),
            previous: Some(previous.clone()),
            previous_hook: Some(PreviousHook {
                binary: PathBuf::from(self.spec.hook_binary),
                args: self
                    .spec
                    .hook_args
                    .iter()
                    .map(|a| (*a).to_owned())
                    .collect(),
            }),
            ..self.order(staged, order_try)
        }
    }

    /// The daemon's switch around the steps. `Target` is `activate_pkg`: the installer's
    /// `activate` (from the release the order replaces, if it has one), then `current` switched to
    /// the target, or, unless `moves_current`, left as it is. `Back` is `back` of a first install.
    /// `Confirmed` does nothing, as the daemon's does.
    pub fn switch(self: &Arc<Self>, moves_current: bool) -> Switch {
        let daemon = Arc::clone(self);
        Arc::new(move |toward, order: &UpdateOrder| match toward {
            Toward::Target => daemon.activate(order, moves_current),
            Toward::Back => daemon.back(order),
            Toward::Confirmed => Ok(()),
        })
    }

    fn activate(&self, order: &UpdateOrder, moves_current: bool) -> Result<(), SwitchError> {
        let from = order.previous.as_ref().map(Staged::dir);
        self.installer()
            .activate(&self.order, order.staged.dir(), from)
            .map_err(|e| SwitchError::Failed(format!("activate: {e:?}")))?;
        if moves_current {
            let units = units()?;
            let fs = RealFs::new(Path::new("/"));
            Placer::new(roots(), &fs, &units)
                .switch_current(&self.order.service, &self.order.version)
                .map_err(|e| SwitchError::Failed(format!("switch current: {e:?}")))?;
        }
        Ok(())
    }

    fn back(&self, order: &UpdateOrder) -> Result<(), SwitchError> {
        let failed = |what: &str, why: String| SwitchError::Failed(format!("{what}: {why}"));
        if order.previous.is_some() {
            return Err(failed(
                "back",
                "this test does not roll an update back to its previous release".to_owned(),
            ));
        }
        let units = units()?;
        for label in &self.order.run {
            units
                .stop_unit(label.as_str())
                .map_err(|e| failed("stop", format!("{e:?}")))?;
        }
        self.installer()
            .remove(&self.order, &[order.staged.dir()])
            .map_err(|e| failed("remove", format!("{e:?}")))?;
        let fs = RealFs::new(Path::new("/"));
        let placer = Placer::new(roots(), &fs, &units);
        let named = placer
            .current(self.spec.name)
            .map_err(|e| failed("current", format!("{e:?}")))?;
        if named.as_deref() != Some(self.version()) {
            return Ok(());
        }
        let dir = service_dir_of(self.spec.name);
        fs::remove_file(dir.join("current"))
            .and_then(|()| fs::File::open(&dir)?.sync_all())
            .map_err(|e| failed("remove current", e.to_string()))
    }

    /// The daemon's runner over `store` (the service's record directory), launching each step as
    /// a transient launchd job, through `switch`, to its verdict.
    pub fn run(&self, order: &UpdateOrder, switch: Switch, store: &Path) -> Finished {
        fs::create_dir_all(store).expect("the record directory");
        // The runner keeps its records only in a directory private to it, as the daemon's is.
        fs::set_permissions(
            store,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .expect("the record directory private");
        let ran = Arc::clone(&self.ran);
        let log: StepLog = Arc::new(move |unit: &str, captured: &Captured| {
            let step = Ran {
                unit: unit.to_owned(),
                code: captured.code,
                last_line: captured.stdout.last_line(),
                stderr: String::from_utf8_lossy(&captured.stderr.bytes()).into_owned(),
            };
            ran.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(step);
        });
        let runner = StepRunner::new(
            Arc::new(LaunchdLauncher::system().with_log(log)),
            Arc::new(SystemClock::new(Box::new(SysctlBootId))),
            RecordStore::new(store),
        )
        .with_switch(switch);
        runner
            .run(order)
            .unwrap_or_else(|e| panic!("the runner stopped with no verdict: {e:?}"))
    }
}

/// The unit control of the daemon's switch.
fn units() -> Result<LaunchdUnitControl<LaunchctlProcess>, SwitchError> {
    LaunchdUnitControl::new(
        LaunchctlProcess::system(LAUNCHCTL_BOUND),
        Path::new(UNIT_DIR),
    )
    .map_err(|e| SwitchError::Failed(format!("the unit control: {e}")))
}

/// The daemon's roots on a Mac.
fn roots() -> Roots {
    Roots::new(
        Path::new(STATE_DIR),
        Path::new(UNIT_DIR),
        Path::new(PACKAGES),
    )
}

/// The agent's directory in the packages directory.
pub fn service_dir() -> PathBuf {
    service_dir_of(SERVICE)
}

/// The directory of `service` in the packages directory.
pub fn service_dir_of(service: &str) -> PathBuf {
    Path::new(PACKAGES).join(service)
}

/// The package's `current` link, which the params name as `release_link`.
pub fn current_link() -> PathBuf {
    service_dir().join("current")
}

/// Where the daemon keeps the package it fetched, root's alone, as its package store does.
const DOWNLOADS: &str = "/var/lib/hostkeeper/downloads";

/// A root-owned copy of the package at `given` (its bytes `bytes`), read-only, in a directory
/// private to root.
fn own_copy(given: &Path, bytes: &[u8]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(DOWNLOADS).expect("the download directory");
    fs::set_permissions(DOWNLOADS, fs::Permissions::from_mode(0o700))
        .expect("the download directory private");
    let copy = Path::new(DOWNLOADS).join(given.file_name().expect("the package's file name"));
    fs::write(&copy, bytes).expect("the copy of the package");
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o444)).expect("the copy read-only");
    copy
}
