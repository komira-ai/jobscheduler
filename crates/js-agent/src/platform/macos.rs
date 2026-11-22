//! The macOS build: jobs run as native programs, each as a user the root helper
//! `js-mac-session` lends from its pool for the run, under a Seatbelt profile
//! ([`crate::native`]), with the VM backend beside them when `--vm-driver` turns it on
//! ([`crate::vm`]). A start with `--no-jobs` runs no job. Neither `crun` nor the container
//! engine is built here.

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
use crate::lifecycle::{Local, NATIVE_JOURNAL, Runtime, StateRoot, scratch_volume};
use crate::native::Ceiling;
use crate::native::{self, NativeChecks, NativeFlags, NativeRuntime, NativeSettings, PfFlags};
use crate::registry_credentials::RegistryCredentials;
use crate::system::{Machine, RuntimeBuild, System};
use crate::{Failure, Services};
use js_agent_wire::LOG_UPLOAD_CAPABILITY;
use js_caps::{EXEC, FLAG_SET, RUNTIME};
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
         native jobs (--native-helper-socket)"
    )
}

/// The flags of the macOS build's runtime, flattened into [`Cli`]: none of them exists in a
/// Linux build.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// The Unix socket of the root helper `js-mac-session` (its `--socket`): with it this
    /// agent runs native jobs, programs started as a user the helper lends from its pool for each run, under
    /// a Seatbelt profile, and reports `exec = sandbox-exec`. This flag is in the macOS build
    /// only, and the build requires it unless `--no-jobs` is given. Needs `--scratch-root`
    /// and `--state-root`, and the agent must run as root. The agent checks at start that
    /// `sandbox-exec` applies a profile.
    /// The helper must be dedicated to this agent, on a Mac used for native jobs only, and
    /// started with its own explicit `--socket`, `--state-dir`, `--uid-range` and `--homes`,
    /// never the defaults of another helper: the agent asks it for its boot reset, which ends
    /// the lease users of whoever used it before and clears the Mac's auto-login. Native runs
    /// are NOT stopped when the agent stops, restarts or dies: the agent's next start adopts
    /// each run the lease journal in the state root recorded and ends the rest, as the rescue
    /// of an agent whose worker died does; meanwhile the helper ends a run at its own
    /// deadline, its wall limit plus `--stop-grace` (a helper from before that deadline holds
    /// none, and its runs are bounded only while an agent runs).
    /// The profile denies creating POSIX shared memory, but not System V shared memory,
    /// semaphores or message queues (which the helper's reset removes) nor POSIX named
    /// semaphores, and the helper reuses uids, so a named semaphore a run leaves can be
    /// opened by a later run that gets the same uid, until the Mac restarts.
    #[arg(long, value_name = "PATH")]
    pub native_helper_socket: Option<PathBuf>,
    /// The directory the helper makes its lease users' homes in (its `--homes`), by default
    /// `/Users`. It must exist; the agent canonicalises it. Needs `--native-helper-socket`.
    #[arg(long, value_name = "PATH")]
    pub native_homes: Option<PathBuf>,
    /// The most network a native job may ask for: `off` (the default), `loopback`, `on` or
    /// `open`. A job that asks for more is refused at start (`network-above-ceiling`) and is
    /// never run with less. A job's level maps onto these values: `none` is `off`,
    /// `allow-list` is `on`, `open` is `open`; `loopback` is a ceiling only, shared by every
    /// run of the Mac. `on` is limited by the pf anchor of `js-mac-provision` (see
    /// `--native-pf-uid-start`): the agent does not start with `on` unless the anchor is in
    /// force, and refuses each `allow-list` job (`egress-unchecked`) while it is not. `open`
    /// allows the network with no filter and needs no anchor: it leaves the scheduler, the
    /// cloud metadata address and the rest of the network reachable from the job, and any
    /// `--native-pf-*` flag beside it stops the agent at start. The hello reports the value
    /// as `egress` (`none` for `off` and `loopback`). Needs `--native-helper-socket`.
    #[arg(long, value_name = "off|loopback|on|open")]
    pub native_network: Option<String>,
    /// A path no native run may read, in addition to the agent's own roots, its token,
    /// instance and CA files, the helper's socket directory and `/etc/kcpassword`; repeatable.
    /// Name the helper's `--state-dir` here. It must exist. Needs `--native-helper-socket`.
    #[arg(long = "native-deny-read", value_name = "PATH")]
    pub native_deny_read: Vec<PathBuf>,
    /// The most MiB any one file a native run writes may reach, its output included (the
    /// run's `RLIMIT_FSIZE`); by default 4096. A run that writes more is ended by
    /// `SIGXFSZ`. Needs `--native-helper-socket`.
    #[arg(long, value_name = "MIB")]
    pub native_max_file_mib: Option<u64>,
    /// The most processes a native run's user may have (the run's `RLIMIT_NPROC`); by
    /// default 512. A fork bomb stops there; the helper's kill is not held by it. Needs
    /// `--native-helper-socket`.
    #[arg(long, value_name = "N")]
    pub native_max_processes: Option<u32>,
    /// A directory a native start's program must lie under; repeatable. By default `/usr/bin`,
    /// `/bin`, `/usr/sbin` and `/sbin`. Needs `--native-helper-socket`.
    #[arg(long = "native-tool-root", value_name = "PATH")]
    pub native_tool_roots: Vec<PathBuf>,
    /// The first uid of the pool whose egress the pf anchor limits: the helper's
    /// `--uid-range` start. With `--native-network on` the agent refuses to start, and
    /// refuses every job that asks for the network (`egress-unchecked`), unless the anchor
    /// is in force (pf enabled, the main ruleset reaches the anchor, the loaded rules are
    /// the ones these flags render); the agent loads the anchor itself from these flags, at
    /// its start and every 30 seconds; required then, and
    /// refused otherwise (with `--native-network open` the agent does not start). Needs
    /// `--native-network on`.
    #[arg(long, value_name = "UID")]
    pub native_pf_uid_start: Option<u32>,
    /// How many uids the pool holds: the helper's `--uid-range` count. Required with
    /// `--native-network on`; see `--native-pf-uid-start`.
    #[arg(long, value_name = "N")]
    pub native_pf_uid_count: Option<u32>,
    /// An endpoint the pool may reach over TCP, as `192.0.2.10:443`; repeatable. The agent
    /// loads the anchor with these endpoints, so an edit takes effect at the agent's next
    /// start. Needs `--native-network on`.
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
    /// Turns the VM backend on (macOS build only, beside `--native-helper-socket`): the
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
    pub fn given(&self) -> [(&'static str, bool); 15] {
        [
            (
                "--native-helper-socket",
                self.native_helper_socket.is_some(),
            ),
            ("--native-homes", self.native_homes.is_some()),
            ("--native-network", self.native_network.is_some()),
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
            ("--vm-driver", self.vm_driver.is_some()),
            ("--vm-store", self.vm_store.is_some()),
        ]
    }

    /// The native flags, as the native settings are made from them.
    fn native(&self) -> NativeFlags<'_> {
        NativeFlags {
            socket: self.native_helper_socket.as_deref(),
            homes: self.native_homes.as_deref(),
            network: self.native_network.as_deref(),
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
    /// Programs run as users of the root helper, under a Seatbelt profile.
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
        "--native-helper-socket"
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
/// job, or has no helper socket (which [`open`] refuses), mounts nothing.
///
/// # Errors
///
/// [`Failure::Usage`] naming `--scratch-root` and the command or the answer at fault.
pub(crate) fn mount_scratch(cli: &Cli, services: &Services<'_>) -> Result<(), Failure> {
    if cli.no_jobs || cli.os.native_helper_socket.is_none() {
        return Ok(());
    }
    if let (Some(state), Some(scratch)) = (&cli.state_root, &cli.scratch_root) {
        let (commands, files) = (services.commands, services.files);
        scratch_volume::mount_at_start(commands, files, state, scratch).map_err(Failure::Usage)?;
    }
    Ok(())
}

/// Checks this build's runtime and the flags that belong to it, before the host is described:
/// the runtime is native, over the root helper of `--native-helper-socket`, unless the agent
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
    if os.native_helper_socket.is_none() {
        return Err(usage(
            "--native-helper-socket: required: this macOS build runs native jobs through the \
             root helper (give --no-jobs for an agent that runs no job)"
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
    /// No host jobs.
    pub(crate) allow_host_jobs: bool,
    /// No FUSE device: only the crun runtime gives it.
    pub(crate) allow_fuse_jobs: bool,
}

/// The job flags of a macOS agent: nothing is checked until the roots are known
/// ([`Jobs::finish`]). A macOS agent has no crun settings (`start`) and
/// no egress, settle or taint re-check flags; it keeps the mount roots, which become read
/// denies of every run and the roots a job's mounts lie beneath.
///
/// # Errors
///
/// None; the signature is the Linux build's.
pub(crate) fn jobs(
    _cli: &Cli,
    _start: Option<Settings>,
    mounts: &crate::lifecycle::MountRoots,
) -> Result<Jobs, String> {
    Ok(Jobs {
        native: None,
        mounts: crate::native::roots_of(mounts),
        layout: crate::lifecycle::DirLayout::Private,
        egress: crate::egress::Config::default(),
        settle: crate::lifecycle::DEFAULT_SETTLE,
        taint_recheck: crate::lifecycle::taint::DEFAULT_TAINT_RECHECK,
        allow_host_jobs: false,
        allow_fuse_jobs: false,
    })
}

impl Jobs {
    /// The mount providers the flags name: none, a macOS build has no such flag.
    pub(crate) fn providers(&self) -> crate::lifecycle::MountProviders {
        crate::lifecycle::MountProviders::default()
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
        if native.is_some() && euid != 0 {
            return Err(
                "--native-helper-socket: the agent must run as root: it gives each run's \
                 directory to the run's user"
                    .to_owned(),
            );
        }
        self.native = native;
        Ok(())
    }

    /// The flag that turned the jobs on, for a refusal that needs it; none for no jobs.
    pub(crate) fn needs(&self) -> Option<&'static str> {
        self.native.as_ref().map(|_| "--native-helper-socket")
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
pub(crate) fn complete(local: &mut Local, cli: &Cli, start: &Start) -> Result<(), String> {
    let backend = local.backend.take();
    local.backend = match backend {
        Some(Backend::Native(native)) => {
            let native = match &start.bundles {
                Some(store) => native.with_bundles(Arc::clone(store))?,
                None => native,
            };
            let native = native.with_xcodes(start.xcodes.clone());
            Some(Backend::Native(native.with_log_file_cap(cli.log_file_cap)))
        }
        None => None,
    };
    local.vm = start.vm.clone();
    Ok(())
}

/// A macOS agent takes no claims: its helper is dedicated to it.
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
    let found = report_native(hello, local.files.is_some(), native.config().network);
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

/// The bits of a native runtime's term that are random. A lease user is named
/// `kbf-lease-<term>-<sequence>` and a user name is at most 31 bytes, so the term has at
/// most 13 digits (40 bits is below 10^13) and leaves room for a sequence of seven.
const TERM_BITS: u32 = 40;

/// The native runtime over the helper `build`'s settings name, whose runs the helper also
/// bounds by their wall limit and the stop grace. Its term is random, so a second process of
/// the agent never asks the helper for a lease user the first one had.
pub(crate) fn runtime(
    _system: &System,
    build: &RuntimeBuild<'_>,
) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
    let native = build.backend.settings();
    let mut bytes = [0u8; 8];
    // A source of randomness that fails leaves a term of zero, which the helper's ledger
    // refuses the second time: the start fails, loudly, and never runs as a used user.
    let _ = getrandom::getrandom(&mut bytes);
    let term = u64::from_le_bytes(bytes) >> (u64::BITS - TERM_BITS);
    native::self_test(native.config(), native.sandbox_exec())
        .map_err(|why| format!("--native-helper-socket: the sandbox self-test failed: {why}"))?;
    let helper = js_mac_session::client::Client::new(native.socket());
    let opened = NativeRuntime::open(helper, native.config().clone(), term);
    let runtime = opened
        .map_err(|why| format!("--native-helper-socket: {why}"))?
        .with_stop_grace(build.stop_grace);
    let runtime = match native.bundles() {
        Some(store) => runtime.with_bundles(Arc::clone(store)),
        None => runtime,
    };
    let runtime = runtime
        .with_xcodes(native.xcodes().to_vec())
        .with_live_logs(native.logs().live());
    Ok((Box::new(runtime), Vec::new()))
}

/// The post-job checks of a backend's jobs, and the cgroup directory the usage of the leases
/// is read from (or why there is none).
pub(crate) type Checks = (Box<dyn CheckHook>, Result<String, &'static str>);

/// The checks after a native job, the helper's leak scan ([`NativeChecks`]) over the
/// backend's helper socket, and why its usage is not read.
///
/// # Errors
///
/// None; the signature is the Linux build's.
pub(crate) fn checks(
    backend: &Backend,
    _machine: &dyn Machine,
    _watch: Vec<PathBuf>,
    _settle: Duration,
) -> Result<Checks, String> {
    let helper = js_mac_session::client::Client::new(backend.settings().socket());
    Ok((Box::new(NativeChecks::over(helper)), Err(NO_NATIVE_USAGE)))
}

/// Why the heartbeat of a native agent carries no load.
const NO_NATIVE_USAGE: &str = "usage: native runs report no load yet: the heartbeat carries none";

/// Adds the capabilities of the host contract to the hello of an agent that runs native
/// jobs: `runtime` and `exec` are `sandbox-exec`, `run-files` when `run_files` (the agent
/// has a files root), and `egress` is the agent's `--native-network` ceiling as a job level
/// ([`Ceiling::advertised`]: `off` and `loopback` are `none`, `on` is `allow-list`, `open` is
/// `open`), with no `egress-ports`, since a Mac forwards no port, and `log-upload`, since the
/// native runtime uploads live logs (`native/logs.rs`), and the job half (`per-job`,
/// `start-fence`, `held-beat`, `dispatch-poll`, `layered-rootfs`: [`contract::report_native_jobs`]). The hello is sent only after
/// the job half has started, which asked the helper for its boot reset, so no hello names a
/// mode the agent cannot serve. Returns the warnings the operator should read: none.
fn report_native(hello: &mut Hello, run_files: bool, ceiling: Ceiling) -> Vec<String> {
    let mode = ExecMode::SandboxExec.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    contract::report_run_files(hello, run_files);
    // The native runtime uploads the live logs of every run whose start carries a sink.
    hello
        .capabilities
        .insert(LOG_UPLOAD_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    contract::report_native_jobs(hello);
    ceiling.advertised().insert_into(&mut hello.capabilities);
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_agent_wire::{HeldRun, Hello, NetworkCeiling, Phase, ProtocolRange, WireShape};
    use js_caps::{EXEC, FLAG_SET};

    use super::{Ceiling, LOG_UPLOAD_CAPABILITY, report_native};

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
        assert!(report_native(&mut native, true, Ceiling::Off).is_empty());
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
        assert_eq!(flag(js_caps::LAYERED_ROOTFS).as_deref(), Some(FLAG_SET));
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
            assert!(report_native(&mut hello, false, ceiling).is_empty());
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
}
