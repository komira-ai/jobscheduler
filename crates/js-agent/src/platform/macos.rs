//! The macOS build: jobs run as native programs, each as an account of the agent's own pool
//! lent for the run ([`crate::mac_accounts`]), started by the agent's supervisor under a
//! Seatbelt profile ([`crate::native`]), with the VM backend beside them when `--vm-driver`
//! turns it on ([`crate::vm`]). A start with `--no-jobs` runs no job. Neither `crun` nor the
//! container engine is built here.
//!
//! # Flags
//!
//! The flags of this build only, refused by name in a Linux build (`super::MACOS_ONLY`); the
//! flags both builds have are in the crate's module doc.
//!
//! | flag | meaning |
//! |---|---|
//! | `--native-accounts <dir>` | the state directory of the agent's pool of slot accounts (its ledger; a directory root owns that no one else may write in, made mode 0700 when missing): with it the macOS build runs native jobs, each as a fresh account of the pool lent for the run, started by the agent's supervisor (`js-agent __supervise-native`) under a Seatbelt profile, and reports `runtime` and `exec` as `sandbox-exec`, the wire's name of a Seatbelt-confined program (no `sandbox-exec` program runs; see [`native`]); required unless `--no-jobs`; needs `--native-account-range`, `--scratch-root` and `--state-root`; the agent must run as root; its path is a read deny of every run. The pool is opened only when the agent holds Full Disk Access (which a privacy profile grants it, `packaging/js-agent-macos/mdm/`): at start the agent lists the privacy database's directory to learn whether it does, reports the answer as the capability `full-disk-access` (`granted`, `not-granted` or `undetermined`), and refuses every native job, `no-full-disk-access`, unless it is `granted`. With the grant, the agent runs `/usr/bin/true` once through a lent account and its retirement before it says hello, and does not start when that fails. Native runs are stopped when the agent stops, and an agent's start ends every account a run of an earlier process held (the run is reported ended) |
//! | `--native-account-range <first>-<last>` | the uids of the pool's slots, both included (`700-799` is 100 slots); no default; from 501 to 60000; nothing else on the Mac may use them; needs `--native-accounts` |
//! | `--native-account-ready <n>` | how many accounts the pool keeps made and ready to lend; by default 1; at most the number of slots; needs `--native-accounts` |
//! | `--native-homes <path>` | (macOS build only) the directory the accounts' homes are in; the pool makes them in `/Users`, so the only value is `/Users`, the default; needs `--native-accounts` |
//! | `--native-network <level>` | (macOS build only) the most network a native job may ask for: `off` (the default), `loopback`, `on` or `open`; a job that asks for more is refused at start (`network-above-ceiling`), never run with less; a job's level maps onto these values (`none` is `off`, `allow-list` is `on`, `open` is `open`), and a job of the level `none`, like a native body that asks for `loopback`, gets loopback under every value but `off`, which gives it no network at all; loopback is shared by every run of the Mac, and the host's own services on it are denied by port (`--native-host-port` below); `on` needs the pf anchor of `js-mac-provision` (the `--native-pf-*` flags below): the agent does not start with `on` unless the anchor is enforced, and before every `allow-list` job it checks again and refuses the job (`egress-unchecked`) if it is not; `open` allows the network with no filter and needs no anchor: the scheduler, the cloud metadata address and the rest of the network are reachable from the job, and a `--native-pf-*` flag beside it stops the agent at start; a start that declares ports is refused (`ports-not-supported`); the hello reports the value as `egress` (`none` for `off` and `loopback`); needs `--native-accounts` |
//! | `--native-host-port <port>` | (macOS build only) a port the host's own services hold on loopback, such as the tierstore node's file service and its S3 address; repeatable, from 1 to 65535, at most 64; every run whose profile reaches loopback is denied listening on it at any address and connecting to it on loopback, whether or not the service listens then (so no job takes the port while its service restarts); a run is also denied the port of every listener on loopback or a wildcard address that no slot account holds, as the agent lists them at the run's start; with a value of `--native-network` above `off` the agent reads those listeners at its own start and does not start when it cannot; needs `--native-accounts` |
//! | `--native-pf-uid-start <uid>`, `--native-pf-uid-count <n>` | (macOS build only) the uids whose egress the anchor limits; both required with `--native-network on`, refused otherwise (with `open` the agent does not start) |
//! | `--native-pf-endpoint <ip:port>` | (macOS build only) an endpoint the pool may reach, as given to `js-mac-provision install --endpoint`; repeatable; needs `--native-network on` |
//! | `--native-pf-anchor <name>`, `--native-pf-state-dir <path>`, `--native-pfctl <path>` | (macOS build only) the anchor (default `com.apple/com.komira.js.pool`), the provisioner's state directory (default `/var/db/com.komira.js-pf`) and the `pfctl` (default `/sbin/pfctl`); absolute paths; need `--native-network on` |
//! | `--native-trees-root <path>` | (macOS build only) the mount point under which the `--mount-provider` serves every job's trees (tierstore's one NFS mount of the Mac); must be a `--mount-root-ro` that no `--mount-root` names, with no other mount root above or below it, and needs `--mount-provider`; it is the one mount root that is not a read deny of every run: a run's profile denies reads below it and grants back the run's own trees; a job's mount below it must be read-only and `no_exec` (`input-not-allowed`) |
//! | `--native-put-root <path>` | (macOS build only) the put ingest's directory (tierstore's put root): a read-write mount of `<this>/cas/b/<build>/<nonce>` at `/work/<rest>` is a run's put target, never linked into its folder nor granted in its profile; after the run the agent commits the run's `<rest>/pack` there as it is (`pack.part`, `pack.sha256-<hex>.part`, `pack`) and notes the key, digest and size as the finding `outputs-committed`; any other mount beneath it is refused (`input-not-allowed`); it is no `--mount-root` and every run's profile denies reads below it; needs `--native-put-user` and `--native-accounts` |
//! | `--native-put-user <user>` | (macOS build only) the user the tierstore node runs as, by name or number (a number is its uid and its group): the folders of each key become its and each run's put directory its group's (mode 0770); needs `--native-put-root` |
//! | `--vm-driver <path>` | turns the VM backend on (see [`vm`]): the absolute path of the signed `js-vmm` binary, started as `js-vmm lease` (one process for each VM, through the disclaiming spawner) to make and drive the VMs; a VM start (`kind: vm`) then runs in a fresh VM cloned from the base image and destroyed when it ends, at most two at once (a third is refused `no-capacity`), and the hello reports `vm.slots`, `vm.max_cpus` and `vm.max_mem_gib`; without it a VM start is refused `vm-unavailable`; macOS build only; needs `--vm-store` and `--native-accounts` |
//! | `--vm-store <dir>` | (macOS build only) the directory of the VM backend: the base image in `base` and each run's clone under `runs/`, on one APFS volume; needs `--vm-driver` |
//! | `--native-tool-root <path>` | (macOS build only) a directory a native start's program must lie under; repeatable; by default `/usr/bin`, `/bin`, `/usr/sbin` and `/sbin`; needs `--native-accounts` |
//! | `--native-deny-read <path>` | (macOS build only) a path no native run may read, beside the agent's roots, token, instance and CA files, `--native-accounts`, `/etc/kcpassword` and the places only the privacy controls guard (`native/fence.rs`); repeatable; must exist; needs `--native-accounts` |
//! | `--native-max-file-mib <n>` | (macOS build only) the most MiB any one file a native run writes may reach, its output included (the run's `RLIMIT_FSIZE`); by default 4096; a run that writes more is ended by `SIGXFSZ`; needs `--native-accounts` |
//! | `--native-max-processes <n>` | (macOS build only) the most processes a native run may start (the run's `RLIMIT_NPROC` is this and one, for the supervisor that waits on the job as the run's account); by default 512; needs `--native-accounts` |

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;

use crate::bundle::Settings as BundleSettings;
use crate::cli::Cli;
use crate::cli_runtime::Settings;
use crate::contract;
use crate::lifecycle::CheckHook;
use crate::lifecycle::NoChecks;
use crate::lifecycle::{
    Local, MountProviders, NATIVE_JOURNAL, Runtime, StateRoot, UserNames, scratch_volume,
};
use crate::mac_supervisor::{Launcher, SystemLauncher};
use crate::mac_usage::Series;
use crate::native::Ceiling;
use crate::native::{self, NativeFlags, NativeRuntime, NativeSettings, PfFlags, PutFlags};
use crate::registry_credentials::RegistryCredentials;
use crate::system::{Machine, RuntimeBuild, System};
use crate::usage::{HostSampler, NodeSampler, macos::MacHost};
use crate::{Failure, Services};
use js_agent_wire::LOG_UPLOAD_CAPABILITY;
use js_caps::{EXEC, FLAG_SET, RUNTIME};
use js_mac_session::full_disk_access::FullDiskAccess;
use js_types::ExecMode;

/// What `--version` prints after the version: the runtime this build runs jobs with, on a
/// line a script can read (`js-agent --version | grep -x 'runtimes: native'`).
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\nruntimes: native");

/// The refusal of a flag that only a Linux build has (`name` is a flag of
/// [`super::LINUX_ONLY`], without its `--`): it is refused by name, before clap would call it
/// unknown.
pub fn foreign_flag(name: &str) -> String {
    format!(
        "--{name}: Linux only (a flag of crun or of the container engine): this macOS build runs \
         native jobs (--native-accounts)"
    )
}

/// The flags of the macOS build's runtime, flattened into [`Cli`]: none of them exists in a
/// Linux build.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// The state directory of the agent's pool of slot accounts (its ledger): a directory
    /// root owns that no one else may write in, made mode 0700 when it is missing. With it this agent runs native jobs, each as a
    /// fresh account of the pool lent for the run and retired after it, started by the agent's
    /// supervisor under a Seatbelt profile, and reports `exec = sandbox-exec`. This flag is in
    /// the macOS build only, and the build requires it unless `--no-jobs` is given. Needs
    /// `--native-account-range`, `--scratch-root` and `--state-root`, and the agent must run as
    /// root. No run may read it.
    /// At start the agent lists the privacy database's directory to learn whether it holds
    /// Full Disk Access, reports the answer as the capability `full-disk-access` and, unless it
    /// is `granted`, opens no pool and refuses every native job (`no-full-disk-access`). With
    /// the grant it runs `/usr/bin/true` once through a lent account before the hello, and does
    /// not start when that fails. An agent's start ends every account a run of an earlier
    /// process held.
    #[arg(long, value_name = "DIR")]
    pub native_accounts: Option<PathBuf>,
    /// The uids of the pool's slots, `<first>-<last>`, both included (`700-799` is 100 slots),
    /// from 501 to 60000; no default. Nothing else on the Mac may use them. Needs
    /// `--native-accounts`.
    #[arg(long, value_name = "FIRST-LAST")]
    pub native_account_range: Option<String>,
    /// How many accounts the pool keeps made and ready to lend; by default 1, at most the
    /// number of slots. Needs `--native-accounts`.
    #[arg(long, value_name = "N")]
    pub native_account_ready: Option<usize>,
    /// The directory the accounts' homes are in. The pool makes them in `/Users`, the
    /// default, and any other value is refused. Needs `--native-accounts`.
    #[arg(long, value_name = "PATH")]
    pub native_homes: Option<PathBuf>,
    /// The most network a native job may ask for: `off` (the default), `loopback`, `on` or
    /// `open`. A job that asks for more is refused at start (`network-above-ceiling`) and is
    /// never run with less. A job's level maps onto these values: `none` is `off`,
    /// `allow-list` is `on`, `open` is `open`; a job of the level `none` gets loopback under
    /// every value but `off`, which gives it no network at all. Loopback is shared by every
    /// run of the Mac; the host's services on it are denied by port (`--native-host-port`).
    /// `on` is limited by the pf anchor of `js-mac-provision` (see
    /// `--native-pf-uid-start`): the agent does not start with `on` unless the anchor is in
    /// force, and refuses each `allow-list` job (`egress-unchecked`) while it is not. `open`
    /// allows the network with no filter and needs no anchor: it leaves the scheduler, the
    /// cloud metadata address and the rest of the network reachable from the job, and any
    /// `--native-pf-*` flag beside it stops the agent at start. The hello reports the value
    /// as `egress` (`none` for `off` and `loopback`). Needs `--native-accounts`.
    #[arg(long, value_name = "off|loopback|on|open")]
    pub native_network: Option<String>,
    /// A port the host's own services hold on loopback (repeatable, 1 to 65535, at most 64),
    /// denied to every run that reaches loopback whether or not it is listened on, beside the
    /// ports of the host's listeners found at the run's start. Needs `--native-accounts`.
    #[arg(long = "native-host-port", value_name = "PORT")]
    pub native_host_ports: Vec<u16>,
    /// A path no native run may read, in addition to the agent's own roots, its token,
    /// instance and CA files, `--native-accounts`, `/etc/kcpassword` and the places only the
    /// privacy controls guard; repeatable. It must exist. Needs `--native-accounts`.
    #[arg(long = "native-deny-read", value_name = "PATH")]
    pub native_deny_read: Vec<PathBuf>,
    /// The most MiB any one file a native run writes may reach, its output included (the
    /// run's `RLIMIT_FSIZE`); by default 4096. A run that writes more is ended by
    /// `SIGXFSZ`. Needs `--native-accounts`.
    #[arg(long, value_name = "MIB")]
    pub native_max_file_mib: Option<u64>,
    /// The most processes a native run may start; by default 512. The run's `RLIMIT_NPROC` is
    /// this and one, for the supervisor that waits on the job as the run's account. A fork
    /// bomb stops there; the agent's kill, as root, is not held by it. Needs
    /// `--native-accounts`.
    #[arg(long, value_name = "N")]
    pub native_max_processes: Option<u32>,
    /// A directory a native start's program must lie under; repeatable. By default `/usr/bin`,
    /// `/bin`, `/usr/sbin` and `/sbin`. Needs `--native-accounts`.
    #[arg(long = "native-tool-root", value_name = "PATH")]
    pub native_tool_roots: Vec<PathBuf>,
    /// The first uid whose egress the pf anchor limits. With `--native-network on` the agent refuses to start, and
    /// refuses every job that asks for the network (`egress-unchecked`), unless the anchor
    /// `js-mac-provision` installed is in force (pf enabled, the main ruleset reaches the
    /// anchor, the loaded rules are the ones these flags render); required then, and
    /// refused otherwise (with `--native-network open` the agent does not start). Needs
    /// `--native-network on`.
    #[arg(long, value_name = "UID")]
    pub native_pf_uid_start: Option<u32>,
    /// How many uids the pf anchor limits. Required with
    /// `--native-network on`; see `--native-pf-uid-start`.
    #[arg(long, value_name = "N")]
    pub native_pf_uid_count: Option<u32>,
    /// An endpoint the pool may reach over TCP, as `192.0.2.10:443`; repeatable. These must
    /// be the `--endpoint` flags `js-mac-provision install` was given, or the anchor reads
    /// back as different and every job is refused. Needs `--native-network on`.
    #[arg(long = "native-pf-endpoint", value_name = "IPV4:PORT")]
    pub native_pf_endpoints: Vec<String>,
    /// The pf anchor the rules are loaded under (the `--anchor` of `js-mac-provision`); by
    /// default `com.apple/com.komira.js.pool`. Needs `--native-network on`.
    #[arg(long, value_name = "NAME")]
    pub native_pf_anchor: Option<String>,
    /// The state directory of `js-mac-provision` (its `--state-dir`); by default
    /// `/var/db/com.komira.js-pf`. Needs `--native-network on`.
    #[arg(long, value_name = "PATH")]
    pub native_pf_state_dir: Option<PathBuf>,
    /// The `pfctl` program to ask (by default `/sbin/pfctl`). Needs `--native-network on`.
    #[arg(long, value_name = "PATH")]
    pub native_pfctl: Option<PathBuf>,
    /// The mount point under which the `--mount-provider` serves every job's trees (tierstore's
    /// one NFS mount of the Mac). It must be a `--mount-root-ro` that no `--mount-root` names,
    /// with no other mount root above or below it, and needs a `--mount-provider`. It is the one mount root that is not a read deny of
    /// every run: a run's profile denies reads below it and grants back the run's own trees.
    /// A job's mount below it is one of its trees, mounted read-only and `no_exec`, or the
    /// start is refused (`input-not-allowed`).
    #[arg(long, value_name = "PATH")]
    pub native_trees_root: Option<PathBuf>,
    /// The put ingest's directory (tierstore's `local_cache.put_ingest` put root): a run's
    /// read-write mount of `<this>/cas/b/<build>/<nonce>` is where its output is committed
    /// after it (`native/put.rs`), and is never given to the job; no run may read below it.
    /// Needs `--native-put-user` and `--native-accounts`.
    #[arg(long, value_name = "PATH")]
    pub native_put_root: Option<PathBuf>,
    /// The user the tierstore node runs as, by name or by number (a number is its uid and
    /// its group): the folders of each key become its, and each job's put directory its
    /// group's. Needs `--native-put-root`.
    #[arg(long, value_name = "USER")]
    pub native_put_user: Option<String>,
    /// Turns the VM backend on (macOS build only, beside `--native-accounts`): the
    /// absolute path of the signed `js-vmm` binary, started as `js-vmm lease` for each VM. A
    /// VM start then runs in a fresh VM cloned from the base image and destroyed when it
    /// ends, at most two at once, and the hello reports `vm.slots`, `vm.max_cpus` and `vm.max_mem_gib`. Without it a VM start is
    /// refused `vm-unavailable`. Needs `--vm-store`.
    #[arg(long, value_name = "PATH")]
    pub vm_driver: Option<PathBuf>,
    /// The directory of the VM backend: the base image in `base` and each run's clone under
    /// `runs/`, on one APFS volume so a clone is a copy-on-write copy. Needs `--vm-driver`.
    #[arg(long, value_name = "DIR")]
    pub vm_store: Option<PathBuf>,
}

impl Flags {
    /// Every flag of the set with whether it was given: an agent that runs no job
    /// (`--no-jobs`) refuses them all. A test walks the flags clap knows and fails when one is
    /// missing here.
    #[must_use]
    pub fn given(&self) -> [(&'static str, bool); 21] {
        [
            ("--native-accounts", self.native_accounts.is_some()),
            (
                "--native-account-range",
                self.native_account_range.is_some(),
            ),
            (
                "--native-account-ready",
                self.native_account_ready.is_some(),
            ),
            ("--native-homes", self.native_homes.is_some()),
            ("--native-network", self.native_network.is_some()),
            ("--native-host-port", !self.native_host_ports.is_empty()),
            ("--native-deny-read", !self.native_deny_read.is_empty()),
            ("--native-max-file-mib", self.native_max_file_mib.is_some()),
            (
                "--native-max-processes",
                self.native_max_processes.is_some(),
            ),
            ("--native-tool-root", !self.native_tool_roots.is_empty()),
            ("--native-pf-uid-start", self.native_pf_uid_start.is_some()),
            ("--native-pf-uid-count", self.native_pf_uid_count.is_some()),
            ("--native-pf-endpoint", !self.native_pf_endpoints.is_empty()),
            ("--native-pf-anchor", self.native_pf_anchor.is_some()),
            ("--native-pf-state-dir", self.native_pf_state_dir.is_some()),
            ("--native-pfctl", self.native_pfctl.is_some()),
            ("--native-trees-root", self.native_trees_root.is_some()),
            ("--native-put-root", self.native_put_root.is_some()),
            ("--native-put-user", self.native_put_user.is_some()),
            ("--vm-driver", self.vm_driver.is_some()),
            ("--vm-store", self.vm_store.is_some()),
        ]
    }

    /// The native flags, as the native settings are made from them.
    fn native(&self) -> NativeFlags<'_> {
        NativeFlags {
            accounts: self.native_accounts.as_deref(),
            account_range: self.native_account_range.as_deref(),
            account_ready: self.native_account_ready,
            homes: self.native_homes.as_deref(),
            network: self.native_network.as_deref(),
            host_ports: &self.native_host_ports,
            tool_roots: &self.native_tool_roots,
            deny_read: &self.native_deny_read,
            max_file_mib: self.native_max_file_mib,
            max_processes: self.native_max_processes,
            pf: PfFlags {
                pfctl: self.native_pfctl.as_deref(),
                anchor: self.native_pf_anchor.as_deref(),
                state_dir: self.native_pf_state_dir.as_deref(),
                uid_start: self.native_pf_uid_start,
                uid_count: self.native_pf_uid_count,
                endpoints: &self.native_pf_endpoints,
            },
        }
    }

    /// No image registry: a native job runs no image.
    pub fn image_registries(&self) -> impl Iterator<Item = &String> {
        std::iter::empty()
    }

    /// No image store: the disk the agent offers is not reduced for one.
    #[must_use]
    pub fn image_budget_gib(&self) -> Option<u64> {
        None
    }
}

/// What the agent runs jobs with: native programs.
#[derive(Debug, Clone)]
pub enum Backend {
    /// Programs run as accounts of the agent's pool, under a Seatbelt profile.
    Native(NativeSettings),
}

impl Backend {
    /// The settings of the native runtime.
    #[must_use]
    pub fn settings(&self) -> &NativeSettings {
        let Self::Native(settings) = self;
        settings
    }

    /// The flag a failure to start the jobs is told under.
    pub(crate) fn flag(&self) -> &'static str {
        "--native-accounts"
    }
}

/// What the start check found, which the rest of the start is made with.
pub(crate) struct Start {
    /// The bundle store, opened before the hello: the hello says the agent holds bundles only
    /// when its store is open, and the runtime is made over the same store.
    bundles: Option<Arc<crate::bundle::Placer>>,
    /// The VM backend, when `--vm-driver` turned it on.
    vm: Option<crate::vm::VmSettings>,
    /// The Xcodes the hello lists ([`crate::host::Offer::xcodes`]), which the native runtime
    /// runs a job that names one with.
    xcodes: Vec<crate::xcode::Xcode>,
}

impl Start {
    /// A macOS build has no crun runtime.
    pub(crate) fn take_crun(&mut self) -> Option<Settings> {
        None
    }
}

/// Mounts the scratch volume, which nothing mounts after a boot, on the scratch root's parent
/// ([`scratch_volume::mount_at_start`]), before anything opens the scratch root or looks at the
/// bundle store beside it (the store is made and checked on that volume). An agent that runs no
/// job, or has no `--native-accounts` (which [`open`] refuses), mounts nothing.
///
/// # Errors
///
/// [`Failure::Usage`] naming `--scratch-root` and the command or the answer at fault.
pub(crate) fn mount_scratch(cli: &Cli, services: &Services<'_>) -> Result<(), Failure> {
    if cli.no_jobs || cli.os.native_accounts.is_none() {
        return Ok(());
    }
    if let (Some(state), Some(scratch)) = (&cli.state_root, &cli.scratch_root) {
        let (commands, files) = (services.commands, services.files);
        scratch_volume::mount_at_start(commands, files, state, scratch).map_err(Failure::Usage)?;
    }
    Ok(())
}

/// Checks this build's runtime and the flags that belong to it, before the host is described:
/// the runtime is native, over the account pool of `--native-accounts`, unless the agent
/// runs no job (`--no-jobs`, which takes no flag of [`Flags`]). The VM flags are checked, and
/// the bundle store of `bundle` is opened now: the hello says the agent holds bundles only
/// when its store is open.
///
/// # Errors
///
/// [`Failure::Usage`] naming the flag at fault.
pub(crate) fn open(
    cli: &Cli,
    services: &Services<'_>,
    bundle: Option<&BundleSettings>,
) -> Result<Start, Failure> {
    let os = &cli.os;
    let usage = |text: String| Failure::Usage(text);
    let given = os
        .given()
        .into_iter()
        .chain(cli.providers.given())
        .chain(cli.samples.given())
        .find_map(|(flag, given)| given.then_some(flag));
    if cli.no_jobs {
        return match given {
            Some(flag) => Err(usage(format!(
                "{flag}: not with --no-jobs (an agent that runs no job)"
            ))),
            None => Ok(Start {
                bundles: None,
                vm: None,
                xcodes: Vec::new(),
            }),
        };
    }
    if os.native_accounts.is_none() {
        return Err(usage(
            "--native-accounts: required: this macOS build runs native jobs as accounts of its \
             own pool (give --no-jobs for an agent that runs no job)"
                .to_owned(),
        ));
    }
    let bundles = match bundle {
        Some(settings) => services
            .bundle
            .open(settings)
            .map_err(|why| usage(format!("--bundle-store: {why}")))?,
        None => None,
    };
    let vm = crate::vm::VmSettings::new(os.vm_driver.as_deref(), os.vm_store.as_deref());
    Ok(Start {
        bundles,
        vm: vm.map_err(Failure::Usage)?,
        xcodes: Vec::new(),
    })
}

/// The checks a Linux build makes once the host is described: a macOS build made its own
/// before ([`open`]); it keeps the Xcodes the hello lists for the runtime.
pub(crate) fn check(
    _cli: &Cli,
    _services: &Services<'_>,
    _image_allow: Vec<js_oci::Allow>,
    (_gpus, xcodes): (&[u32], &[crate::xcode::Xcode]),
    opened: Opened,
    _err: &mut dyn Write,
) -> Result<Start, Failure> {
    Ok(Start {
        xcodes: xcodes.to_vec(),
        ..opened
    })
}

/// The `--image-allow` entries of `cli`, each with its `credentials`: what a bundle named by
/// its image is pulled by. It is checked before the agent connects.
///
/// # Errors
///
/// [`crate::ImageAllowError`]: the first entry that is wrong.
pub(crate) fn image_allow(
    cli: &Cli,
    credentials: &RegistryCredentials,
) -> Result<Vec<js_oci::Allow>, crate::ImageAllowError> {
    credentials.image_allow(&cli.image_allow, &[])
}

/// What [`open`] found: the start, already checked.
pub(crate) type Opened = Start;

/// The jobs of a macOS agent, made from the flags of [`Flags`] by [`jobs`] and [`Jobs::finish`].
pub(crate) struct Jobs {
    /// The native settings, once [`Jobs::finish`] has made them.
    native: Option<NativeSettings>,
    /// The mount roots (`--mount-root`, `--mount-root-ro`): read denies of every run, and
    /// what a job's mounts must lie beneath.
    mounts: Vec<crate::native::MountRoot>,
    /// The run files root's layout: the run's user reaches its own directory.
    pub(crate) layout: crate::lifecycle::DirLayout,
    /// Egress none: the native ceiling is `--native-network`'s.
    pub(crate) egress: crate::egress::Config,
    /// The default settle time: the post-job checks are Linux's.
    pub(crate) settle: Duration,
    /// The default taint re-check.
    pub(crate) taint_recheck: Duration,
    /// `--host-second-samples`.
    pub(crate) host_second_samples: bool,
    /// No host jobs.
    pub(crate) allow_host_jobs: bool,
    /// No FUSE device: only the crun runtime gives it.
    pub(crate) allow_fuse_jobs: bool,
    /// The mount providers (`--mount-provider`): a job takes a provider's tree as a
    /// read-only mount only.
    providers: MountProviders,
}

/// The job flags of a macOS agent: the mount roots, which become read denies of every run
/// (but the trees root, `--native-trees-root`, checked by [`crate::native_fs::trees_root`])
/// and the roots a job's mounts lie beneath, and the mount providers, a user named by name
/// looked up in the directory service. The native settings are checked once the roots are known
/// ([`Jobs::finish`]). A macOS agent has no crun settings (`start`) and no egress, settle or
/// taint re-check flags.
///
/// # Errors
///
/// The text names the flag at fault.
pub(crate) fn jobs(
    cli: &Cli,
    _start: Option<Settings>,
    mounts: &crate::lifecycle::MountRoots,
) -> Result<Jobs, String> {
    let flags = &cli.providers;
    let providers = MountProviders::from_flags(
        &flags.mount_providers,
        &flags.mount_provider_users,
        flags.mount_provider_wait,
        UserNames::Directory(&uid_of_name),
    )?;
    let trees = crate::native_fs::trees_root(
        cli.os.native_trees_root.as_deref(),
        mounts,
        !flags.mount_providers.is_empty(),
    )?;
    Ok(Jobs {
        native: None,
        mounts: crate::native::roots_of(mounts, trees.as_deref()),
        layout: crate::lifecycle::DirLayout::Private,
        egress: crate::egress::Config::default(),
        settle: crate::lifecycle::DEFAULT_SETTLE,
        taint_recheck: crate::lifecycle::taint::DEFAULT_TAINT_RECHECK,
        host_second_samples: cli.samples.host_second_samples,
        allow_host_jobs: false,
        allow_fuse_jobs: false,
        providers,
    })
}

/// The uid and the primary group of the user `name` in the directory service, as
/// [`uid_of_name`] looks it up: the put ingest's node (`--native-put-user`).
fn node_of_name(name: &str) -> std::io::Result<Option<crate::mac_put::Node>> {
    use js_mac_session::helper::Host as _;
    let account = js_mac_session::macos::MacHost.account(name)?;
    Ok(account.map(|account| crate::mac_put::Node {
        uid: account.uid,
        gid: account.gid,
    }))
}

/// The uid of the user `name` in the directory service (`getpwnam_r`): a Mac's service
/// accounts, such as a provider's, are not in `/etc/passwd`, and their uids differ from Mac to
/// Mac.
fn uid_of_name(name: &str) -> std::io::Result<Option<u32>> {
    use js_mac_session::helper::Host as _;
    let account = js_mac_session::macos::MacHost.account(name)?;
    Ok(account.map(|account| account.uid))
}

impl Jobs {
    /// The mount providers the flags name.
    pub(crate) fn providers(&self) -> MountProviders {
        self.providers.clone()
    }

    /// The native settings of `cli`'s flags, checked: `denied` are the agent's own roots and
    /// files, which no run may read, and the lease journal is kept in `state`. A native agent
    /// must be root (`euid`): it gives each run's directory to the run's user.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub(crate) fn finish(
        &mut self,
        cli: &Cli,
        denied: &[&Path],
        state: Option<&StateRoot>,
        euid: u32,
    ) -> Result<(), String> {
        let journal = state.map_or_else(PathBuf::new, |state| state.path().join(NATIVE_JOURNAL));
        let native = NativeSettings::from_flags(&cli.os.native(), denied, &journal)?
            .map(|native| native.with_mount_roots(self.mounts.clone()))
            .transpose()?;
        let put = &PutFlags {
            native_put_root: cli.os.native_put_root.clone(),
            native_put_user: cli.os.native_put_user.clone(),
        };
        let native = match native {
            Some(native) => {
                let put = native::put_of(put, &node_of_name)?;
                let series = Series {
                    cpu: cli.samples.cpu_series,
                    memory: cli.samples.memory_series,
                };
                Some(native.with_put(put).with_series(series))
            }
            None if put.native_put_root.is_some() || put.native_put_user.is_some() => {
                return Err("--native-put-root: needs --native-accounts".to_owned());
            }
            None => None,
        };
        if native.is_some() && euid != 0 {
            return Err(
                "--native-accounts: the agent must run as root: it gives each run's \
                 directory to the run's user"
                    .to_owned(),
            );
        }
        self.native = native;
        Ok(())
    }

    /// The flag that turned the jobs on, for a refusal that needs it; none for no jobs.
    pub(crate) fn needs(&self) -> Option<&'static str> {
        self.native.as_ref().map(|_| "--native-accounts")
    }

    /// What the run files root is for.
    pub(crate) fn runs(&self) -> crate::lifecycle::run_files::Runs {
        match self.native {
            Some(_) => crate::lifecycle::run_files::Runs::Native,
            None => crate::lifecycle::run_files::Runs::None,
        }
    }

    /// The paths a run files root may not be, hold or lie in, beyond the agent's roots: a
    /// path the profiles deny reads of (the job could not read its files) and the homes of
    /// the native users.
    pub(crate) fn denied_paths(&self) -> Vec<(&'static str, &Path)> {
        let Some(native) = &self.native else {
            return Vec::new();
        };
        let config = native.config();
        let denies = config
            .read_denies
            .iter()
            .map(|path| Path::new(path.as_str()));
        denies
            .map(|path| ("path no native run may read", path))
            .chain([("--native-homes", config.homes.as_path())])
            .collect()
    }

    /// A macOS agent watches no directory after a job: the post-job checks are Linux's.
    pub(crate) fn watch(_cli: &Cli, _busy: &[&Path]) -> Result<Vec<PathBuf>, String> {
        Ok(Vec::new())
    }

    /// The backend the worker runs.
    pub(crate) fn backend(
        self,
        _scratch: Option<&crate::lifecycle::ScratchRoot>,
        _files: Option<&crate::lifecycle::FilesRoot>,
    ) -> Option<Backend> {
        self.native.map(Backend::Native)
    }
}

/// What the start check found, joined to the native settings once they are made: the bundle
/// store, `--log-file-cap`, and the VM backend.
///
/// # Errors
///
/// The text names the flag at fault.
pub(crate) fn complete(
    local: &mut Local,
    cli: &Cli,
    start: &Start,
    files: &dyn crate::FileReader,
) -> Result<(), String> {
    let backend = local.backend.take();
    local.backend = match backend {
        Some(Backend::Native(native)) => {
            let native = match &start.bundles {
                Some(store) => native.with_bundles(Arc::clone(store))?,
                None => native,
            };
            // The agent asks itself once, here: the state its listing of the privacy
            // database's directory shows is the one the hello reports and the runtime
            // refuses jobs by.
            let privacy = std::path::Path::new(native::grant::PRIVACY_DIR);
            let held = native::grant::ask(files.entries(privacy).map(drop));
            let native = native
                .with_full_disk_access(held)
                .with_xcodes(start.xcodes.clone());
            Some(Backend::Native(native.with_log_file_cap(cli.log_file_cap)))
        }
        None => None,
    };
    local.vm = start.vm.clone();
    Ok(())
}

/// A macOS agent takes no claims: its account range is its own.
pub(crate) struct Claims;

/// See [`Claims`].
///
/// # Errors
///
/// None; the signature is the Linux build's.
pub(crate) fn claim(
    _local: &mut Local,
    _services: &Services<'_>,
    _host_id: &str,
) -> Result<Claims, Failure> {
    Ok(Claims)
}

/// Puts what the native runtime does into `hello` (and the VM backend's capabilities); an
/// agent that runs no job says nothing.
///
/// # Errors
///
/// None; the signature is the Linux build's.
pub(crate) fn report(
    hello: &mut Hello,
    local: &Local,
    _start: &Start,
    _services: &Services<'_>,
    _default_gib: Option<u64>,
) -> Result<Option<Vec<String>>, Failure> {
    let Some(native) = local.backend.as_ref().map(Backend::settings) else {
        return Ok(None);
    };
    let config = native.config();
    let found = report_native(
        hello,
        local.files.is_some(),
        config.network,
        config.full_disk_access,
        &local.providers,
    );
    if local.vm.is_some() {
        crate::vm::report(hello);
    }
    Ok(Some(found))
}

/// The grant a native agent's session fills from the replies, which its live logs are
/// uploaded with; none for no jobs.
pub(crate) fn log_grant(local: &Local) -> Option<Arc<crate::logs::HostGrant>> {
    let native = local.backend.as_ref().map(Backend::settings);
    native.map(|native| Arc::clone(&native.logs().grant))
}

/// A macOS agent keeps no image store.
pub(crate) fn image_budget(_local: &Local) -> Option<u64> {
    None
}

/// A macOS agent has no post-job checks to warn of.
pub(crate) fn warn(_local: &Local, _err: &mut dyn Write) {}

/// A Mac has no tmpfs: a native agent's run files root is wherever the operator put it.
pub(crate) const RUN_FILES_ON_TMPFS: bool = false;

/// The bits of a native runtime's term that are random. A run's lease is `<term>.<sequence>`
/// and the job name the pool records for its account is `<term>-<sequence>` (at most 63
/// bytes): 40 bits is below 10^13, which leaves room for any sequence.
const TERM_BITS: u32 = 40;

/// The native runtime over the settings `build` names: the account pool of
/// `--native-accounts` (opened when the agent holds Full Disk Access), the agent's own
/// supervisor, and, with the pool open, a self-test that runs `/usr/bin/true` through a lent
/// account and its retirement before the agent says hello. Its term is random, so two
/// processes of the agent never name a lease alike.
pub(crate) fn runtime(
    _system: &System,
    build: &RuntimeBuild<'_>,
) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
    let native = build.backend.settings();
    let mut bytes = [0u8; 8];
    // A source of randomness that fails leaves a term of zero: the leases of two processes
    // may then share a name, which the journal tells apart only by order.
    let _ = getrandom::getrandom(&mut bytes);
    let term = u64::from_le_bytes(bytes) >> (u64::BITS - TERM_BITS);
    let launcher = SystemLauncher::current()
        .map_err(|why| format!("--native-accounts: the agent's own path: {why}"))?;
    let launcher: Arc<dyn Launcher> = Arc::new(launcher);
    let opened = NativeRuntime::open(launcher, native.config().clone(), term);
    let runtime = opened.map_err(|why| format!("--native-accounts: {why}"))?;
    let runtime = match native.bundles() {
        Some(store) => runtime.with_bundles(Arc::clone(store)),
        None => runtime,
    };
    let runtime = runtime
        .with_xcodes(native.xcodes().to_vec())
        .with_live_logs(native.logs().live())
        .with_put(native.put().cloned())
        .with_series(native.series());
    self_tested(runtime)
}

/// `runtime` after its self-test, which runs before the agent says hello: a failed self-test
/// ends the start with the text of its flag. Split from [`runtime`] so a test can run it over a
/// runtime made with a fake launcher.
///
/// # Errors
///
/// The self-test failed, naming why.
#[doc(hidden)]
pub fn self_tested(mut runtime: NativeRuntime) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
    runtime
        .self_test()
        .map_err(|why| format!("--native-accounts: the self-test failed: {why}"))?;
    Ok((Box::new(runtime), Vec::new()))
}

/// The post-job checks of a backend's jobs, and what the host's load is sampled with.
pub(crate) type Checks = (
    Box<dyn CheckHook>,
    Result<Box<dyn NodeSampler>, &'static str>,
);

/// The checks after a native job: none. The account's retirement proves everything of the
/// run's uid gone ([`crate::mac_accounts`]); a leak scan of the rest of the Mac needs a
/// baseline nothing records yet. And the sampler of the Mac's load ([`HostSampler`] over
/// [`MacHost`]: its CPU load and memory statistics, carried as a Linux host's `/proc`).
///
/// # Errors
///
/// None; the signature is the Linux build's.
pub(crate) fn checks(
    _backend: &Backend,
    _machine: &dyn Machine,
    _watch: Vec<PathBuf>,
    _settle: Duration,
) -> Result<Checks, String> {
    let sampler: Box<dyn NodeSampler> = Box::new(HostSampler::new(MacHost::new()));
    Ok((Box::new(NoChecks), Ok(sampler)))
}

/// Adds the capabilities of the host contract to the hello of an agent that runs native
/// jobs: `runtime` and `exec` are `sandbox-exec`, `run-files` when `run_files` (the agent
/// has a files root), and `egress` is the agent's `--native-network` ceiling as a job level
/// ([`Ceiling::advertised`]: `off` and `loopback` are `none`, `on` is `allow-list`, `open` is
/// `open`), with no `egress-ports`, since a Mac forwards no port, `full-disk-access` (the state
/// the agent found at start: [`native::grant::report`]), and `log-upload`, since the
/// native runtime uploads live logs (`native/logs.rs`), and the job half (`per-job`,
/// `start-fence`, `held-beat`, `dispatch-poll`, `layered-rootfs`: [`contract::report_native_jobs`]),
/// and `mount-provider.<name>` for each of `providers`. The hello is sent only after
/// the job half has started, which ran the runtime's self-test, so no hello names a mode the
/// agent cannot serve. Returns the warnings the operator should read: none.
fn report_native(
    hello: &mut Hello,
    run_files: bool,
    ceiling: Ceiling,
    full_disk_access: FullDiskAccess,
    providers: &MountProviders,
) -> Vec<String> {
    // `sandbox-exec` is the wire's name of "a program under a Seatbelt profile on a Mac":
    // the profile is applied in the job's own process by the supervisor now, and no
    // `sandbox-exec` program runs, but the value is the one servers read.
    let mode = ExecMode::SandboxExec.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    contract::report_run_files(hello, run_files);
    // The native runtime uploads the live logs of every run whose start carries a sink.
    hello
        .capabilities
        .insert(LOG_UPLOAD_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    contract::report_native_jobs(hello);
    native::grant::report(&mut hello.capabilities, full_disk_access);
    contract::report_mount_providers(hello, providers, true);
    ceiling.advertised().insert_into(&mut hello.capabilities);
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_agent_wire::{HeldRun, Hello, NetworkCeiling, Phase, ProtocolRange, WireShape};
    use js_caps::{EXEC, FLAG_SET};

    use super::{
        Ceiling, FullDiskAccess, LOG_UPLOAD_CAPABILITY, MountProviders, UserNames, report_native,
    };

    /// A hello of a host with no capabilities but its os.
    fn hello() -> Hello {
        Hello {
            agent_process: None,
            protocol: ProtocolRange { min: 1, max: 1 },
            host_id: "mac-1".into(),
            pool: "macs".into(),
            agent_version: "0.1.0".into(),
            instance: None,
            shape: WireShape {
                cpu_millis: 1000,
                memory_bytes: 1 << 30,
                gpus: 0,
                disk_bytes: 0,
                gpu_class: None,
                max_leases: None,
                accelerator: None,
            },
            capabilities: BTreeMap::from([("os".to_owned(), "macos".to_owned())]),
            gpu_devices: Vec::new(),
            labels: BTreeMap::new(),
            phase: Phase::Serving,
            held: Vec::<HeldRun>::new(),
            accepts_host_credential: false,
            identity: None,
        }
    }

    /// Asserts that a native agent's hello says it uploads live logs, takes the job-keyed
    /// start (`per-job`, `start-fence`, `held-beat`), polls the dispatch route (`dispatch-poll`) and gives mounts (`layered-rootfs`), and still
    /// validates. Catches a server never told that a native agent uploads or takes job starts
    /// and mounts.
    #[test]
    fn a_native_agent_reports_log_upload_the_job_keys_and_the_dispatch_poll() {
        let mut native = hello();
        let none = MountProviders::default();
        assert!(
            report_native(
                &mut native,
                true,
                Ceiling::Off,
                FullDiskAccess::Granted,
                &none
            )
            .is_empty()
        );
        let flag = |key: &str| native.capabilities.get(key).cloned();
        assert_eq!(flag(LOG_UPLOAD_CAPABILITY).as_deref(), Some(FLAG_SET));
        assert_eq!(
            flag(js_agent_wire::START_FENCE_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(
            flag(js_agent_wire::PER_JOB_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(
            flag(js_agent_wire::HELD_BEAT_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(
            flag(js_agent_wire::DISPATCH_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(
            flag(js_agent_wire::LOAD_ADMISSION_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(flag(js_caps::LAYERED_ROOTFS).as_deref(), Some(FLAG_SET));
        assert!(
            !native
                .capabilities
                .keys()
                .any(|key| key.starts_with("mount-provider.")),
            "{native:?}"
        );
        assert_eq!(native.validate(), Ok(()));
    }

    /// Asserts that a native agent's hello reports `mount-provider.<name>` for each provider
    /// it was given, beside `layered-rootfs`, and still validates. Catches a Mac given
    /// tierstore that a server never places a provider's job on.
    #[test]
    fn a_native_hello_reports_each_provider_it_was_given() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "native-hello-providers");
        let given = [("tierstore".to_owned(), dir.path().join("provider.sock"))];
        let none = |_: &str| Ok(None);
        let providers =
            MountProviders::from_flags(&given, &[], None, UserNames::Directory(&none)).unwrap();
        let mut native = hello();
        assert!(
            report_native(
                &mut native,
                false,
                Ceiling::Off,
                FullDiskAccess::Granted,
                &providers
            )
            .is_empty()
        );
        let named: Vec<&String> = native
            .capabilities
            .keys()
            .filter(|key| key.starts_with("mount-provider."))
            .collect();
        assert_eq!(named, ["mount-provider.tierstore"]);
        assert_eq!(native.capabilities["mount-provider.tierstore"], FLAG_SET);
        assert_eq!(native.capabilities[js_caps::LAYERED_ROOTFS], FLAG_SET);
        assert_eq!(native.validate(), Ok(()));
    }

    /// Catches a Mac agent that does not say its ceiling, says `loopback` as a level a job
    /// could ask for, or claims ports it does not forward: the hello of a native agent says
    /// `sandbox-exec`, and `egress` is `none` for `off` and `loopback`, `allow-list` for `on`
    /// and `open` for `open`, never with `egress-ports`; the wire's reader reads each back as
    /// written and every hello is valid.
    #[test]
    fn a_native_agent_advertises_its_ceiling_in_hello() {
        let table = [
            (Ceiling::Off, js_types::JobNetwork::None),
            (Ceiling::Loopback, js_types::JobNetwork::None),
            (Ceiling::On, js_types::JobNetwork::AllowList),
            (Ceiling::Open, js_types::JobNetwork::Open),
        ];
        for (ceiling, level) in table {
            let mut hello = hello();
            let none = MountProviders::default();
            assert!(
                report_native(&mut hello, false, ceiling, FullDiskAccess::Granted, &none)
                    .is_empty()
            );
            assert_eq!(hello.capabilities[EXEC], "sandbox-exec", "{ceiling}");
            assert_eq!(hello.capabilities["egress"], level.name(), "{ceiling}");
            assert!(
                !hello.capabilities.contains_key("egress-ports"),
                "{ceiling}"
            );
            let read = NetworkCeiling::from_capabilities(&hello.capabilities).map(|c| c.level);
            assert_eq!(read, Ok(level), "{ceiling}");
            assert_eq!(hello.validate(), Ok(()), "{ceiling}");
        }
    }

    /// Asserts that a native agent's hello carries the state it found at start as the
    /// capability `full-disk-access`, spelled `granted`, `not-granted` or `undetermined`, and
    /// still validates. Catches a host listing that shows no grant state, or one that shows
    /// a state other than the one the agent refuses jobs by.
    #[test]
    fn a_native_agent_reports_whether_it_holds_full_disk_access() {
        for (state, text) in [
            (FullDiskAccess::Granted, "granted"),
            (FullDiskAccess::NotGranted, "not-granted"),
            (FullDiskAccess::Undetermined, "undetermined"),
        ] {
            let mut hello = hello();
            assert!(
                report_native(
                    &mut hello,
                    false,
                    Ceiling::Off,
                    state,
                    &MountProviders::default()
                )
                .is_empty()
            );
            assert_eq!(hello.capabilities[js_caps::FULL_DISK_ACCESS], text);
            assert_eq!(hello.validate(), Ok(()), "{text}");
        }
    }
}
