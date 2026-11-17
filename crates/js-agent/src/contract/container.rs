//! The host contract of an agent that runs containers (the Linux build's runtimes): `runtime`
//! and `exec` are `container`, with `per-job`, `start-fence`, `gpu-toolkit`, `gpu-passthrough`,
//! the scratch quota, `host-jobs`, `layered-rootfs` and `mount-provider.<name>` (see
//! [`super`]).

use std::path::Path;

use js_agent_wire::{HOST_JOBS_CAPABILITY, Hello, LOG_UPLOAD_CAPABILITY, NetworkCeiling};
use js_caps::{
    EXEC, FLAG_SET, GPU_PASSTHROUGH, GPU_TOOLKIT, RUNTIME, SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA,
    SCRATCH_QUOTA_PROJECT, mount_provider_key,
};
use js_types::ExecMode;

use super::{report_per_job, report_run_files};
use crate::facts::CommandRunner;
use crate::lifecycle::MountProviders;

/// The `PATH` the toolkit hook runs with; it inherits nothing else.
const HOOK_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The checked `--gpu-toolkit-hook`: the absolute path of the program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolkitHook(String);

impl ToolkitHook {
    /// The hook `--gpu-toolkit-hook` names (`path`); none without the flag. The toolkit is
    /// the container engine's: only an agent of the engine takes the flag.
    ///
    /// # Errors
    ///
    /// The text names the flag: the path is not absolute.
    pub(crate) fn from_path(path: Option<&str>) -> Result<Option<Self>, String> {
        let Some(path) = path else {
            return Ok(None);
        };
        if !Path::new(path).is_absolute() {
            return Err(format!(
                "--gpu-toolkit-hook: {path} is not an absolute path"
            ));
        }
        Ok(Some(Self(path.to_owned())))
    }
}

/// Puts `mount-provider.<name>` into `hello` for each provider the agent was given
/// (`--mount-provider`), where it reports `layered-rootfs` (`layered`): a provider's tree is
/// a layer or a mount of the job, which a host that gives neither cannot give, so a server
/// never places a job that names the provider there.
pub(crate) fn report_mount_providers(hello: &mut Hello, providers: &MountProviders, layered: bool) {
    if layered {
        for (name, _, _) in providers.sockets() {
            hello
                .capabilities
                .insert(mount_provider_key(name), FLAG_SET.to_owned());
        }
    }
}

/// Says that the agent launches host jobs, when it was started with `--allow-host-jobs`
/// (`allowed`), which the start check takes only with the crun runtime.
pub(crate) fn report_host_jobs(hello: &mut Hello, allowed: bool) {
    if allowed {
        hello
            .capabilities
            .insert(HOST_JOBS_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    }
}

/// Adds the capabilities of the host contract to the hello of an agent that runs
/// containers. `scratch_default_gib` is the default of an agent whose scratch quota is on;
/// `gpu_passthrough` says the runtime gives jobs the GPUs they book, `ceiling` is the most
/// network its jobs may ask for, and `log_upload` says the runtime uploads live logs (the
/// `crun` runtime; a native agent reports it through [`crate::platform`]), reported as
/// `log-upload`. Returns the warnings the operator should read.
pub(crate) fn report(
    hello: &mut Hello,
    hook: Option<&ToolkitHook>,
    commands: &dyn CommandRunner,
    run_files: bool,
    scratch_default_gib: Option<u64>,
    (gpu_passthrough, ceiling, log_upload): (bool, &NetworkCeiling, bool),
) -> Vec<String> {
    let mode = ExecMode::Container.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    ceiling.insert_into(&mut hello.capabilities);
    report_per_job(hello);
    report_run_files(hello, run_files);
    let upload = (LOG_UPLOAD_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    hello.capabilities.extend(log_upload.then_some(upload));
    if gpu_passthrough {
        hello
            .capabilities
            .insert(GPU_PASSTHROUGH.to_owned(), FLAG_SET.to_owned());
    }
    if let Some(gib) = scratch_default_gib {
        hello
            .capabilities
            .insert(SCRATCH_QUOTA.to_owned(), SCRATCH_QUOTA_PROJECT.to_owned());
        hello
            .capabilities
            .insert(SCRATCH_DEFAULT_GIB.to_owned(), gib.to_string());
    }
    let Some(hook) = hook.filter(|_| hello.shape.gpus > 0) else {
        return Vec::new();
    };
    match commands.run_in(&hook.0, &[], &[("PATH", HOOK_PATH)]) {
        Ok(_) => {
            hello
                .capabilities
                .insert(GPU_TOOLKIT.to_owned(), FLAG_SET.to_owned());
            Vec::new()
        }
        Err(why) => vec![format!(
            "--gpu-toolkit-hook: {why}; the host is not reported as having the GPU toolkit"
        )],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_agent_wire::{
        HeldRun, PER_JOB_CAPABILITY, Phase, ProtocolRange, START_FENCE_CAPABILITY, WireShape,
    };

    use super::*;
    use crate::egress::Config;

    /// A runner whose every program fails: the toolkit hook does not pass.
    struct Failing;

    impl CommandRunner for Failing {
        fn run(&self, program: &str, _args: &[&str]) -> Result<String, String> {
            Err(format!("{program}: exit status 1"))
        }

        fn run_in(
            &self,
            program: &str,
            args: &[&str],
            _env: &[(&str, &str)],
        ) -> Result<String, String> {
            self.run(program, args)
        }
    }

    fn hello() -> Hello {
        Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            host_id: "host-1".into(),
            pool: "builders".into(),
            agent_version: "0.1.0".into(),
            instance: None,
            shape: WireShape {
                cpu_millis: 1000,
                memory_bytes: 1 << 30,
                gpus: 1,
                disk_bytes: 0,
                gpu_class: Some("nvidia-a10".into()),
                max_leases: None,
                accelerator: None,
            },
            capabilities: BTreeMap::from([("os".to_owned(), "linux".to_owned())]),
            gpu_devices: Vec::new(),
            labels: BTreeMap::new(),
            phase: Phase::Serving,
            held: Vec::<HeldRun>::new(),
            accepts_host_credential: false,
            identity: None,
        }
    }

    /// The hello of a container agent whose egress flags are `config`, on a host with one GPU
    /// whose toolkit hook fails: the ceiling is reported whatever the hook says.
    fn reported(config: &Config) -> Hello {
        let mut hello = hello();
        let hook = ToolkitHook("/usr/bin/false".to_owned());
        let warnings = report(
            &mut hello,
            Some(&hook),
            &Failing,
            false,
            None,
            (false, &config.ceiling(), false),
        );
        assert_eq!(
            warnings,
            [
                "--gpu-toolkit-hook: /usr/bin/false: exit status 1; the host is not reported as having the GPU toolkit"
            ]
        );
        assert!(!hello.capabilities.contains_key(GPU_TOOLKIT));
        hello
    }

    /// Asserts that `start-fence` is reported as `true`, beside `per-job`, by the hello of an
    /// agent that runs containers (its lifecycle judges a job start by its fence and stops its
    /// jobs after a silence), and that the hello still validates. It catches a server never
    /// told that a container agent enforces the fence.
    #[test]
    fn the_hello_reports_start_fence_only_when_enforced() {
        let contained = reported(&Config::none());
        let flag = |hello: &Hello, key: &str| hello.capabilities.get(key).cloned();
        assert_eq!(
            flag(&contained, START_FENCE_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(
            flag(&contained, PER_JOB_CAPABILITY).as_deref(),
            Some(FLAG_SET)
        );
        assert_eq!(contained.validate(), Ok(()));
        assert_eq!(START_FENCE_CAPABILITY, "start-fence");
    }

    /// Asserts that the job half of a native agent's hello (`report_native_jobs`, which the
    /// macOS build calls) is `per-job`, `start-fence` and `layered-rootfs`, each `true`, and
    /// that the hello still validates. It catches a native agent that hides that it takes
    /// job starts and gives mounts, so no server sends it the kbf action jobs.
    #[test]
    fn a_native_agents_job_half_reports_per_job_start_fence_and_layered_rootfs() {
        let mut native = hello();
        crate::contract::report_native_jobs(&mut native);
        for key in [
            PER_JOB_CAPABILITY,
            START_FENCE_CAPABILITY,
            js_caps::LAYERED_ROOTFS,
        ] {
            assert_eq!(
                native.capabilities.get(key).map(String::as_str),
                Some(FLAG_SET),
                "{key}"
            );
        }
        assert_eq!(native.validate(), Ok(()));
    }

    /// Asserts that `log-upload` is in the hello of an agent whose runtime uploads live logs
    /// (the `crun` agent reports with `log_upload` set, and a native agent always) and in no
    /// other: not an engine agent's. It catches a server that sends a sink to an agent that
    /// cannot upload, and one never told that a crun or a native agent can.
    #[test]
    fn the_agent_reports_log_upload_on_the_crun_and_native_runtimes_only() {
        let ceiling = Config::none().ceiling();
        let mut crun = hello();
        assert!(
            report(
                &mut crun,
                None,
                &Failing,
                false,
                None,
                (false, &ceiling, true)
            )
            .is_empty()
        );
        assert_eq!(
            crun.capabilities
                .get(LOG_UPLOAD_CAPABILITY)
                .map(String::as_str),
            Some(FLAG_SET)
        );
        assert_eq!(crun.validate(), Ok(()));
        let engine = reported(&Config::none());
        assert!(!engine.capabilities.contains_key(LOG_UPLOAD_CAPABILITY));
        assert_eq!(LOG_UPLOAD_CAPABILITY, "log-upload");
    }

    /// Catches a container agent that does not say its ceiling, says it under another key,
    /// claims ports where nothing forwards them, or hides the ports it forwards: built from
    /// `--egress none` (the default) the hello carries `egress=none` and no `egress-ports`;
    /// from `--egress open` with forward ports `80` and `8000-8010` it carries `egress=open`
    /// and `egress-ports=80,8000-8010`; from `--egress allow-list` with `--egress-forward-port
    /// 8080` `egress-ports=8080`; from `--egress open` with no forward port no `egress-ports`.
    /// The wire's reader reads each back as the agent's ceiling, and every hello is valid.
    #[test]
    fn egress_ports_is_advertised_only_when_forwarding_is_possible() {
        let none = reported(&Config::none());
        assert_eq!(none.capabilities["egress"], "none");
        assert!(!none.capabilities.contains_key("egress-ports"));
        assert_eq!(
            NetworkCeiling::from_capabilities(&none.capabilities),
            Ok(NetworkCeiling::default())
        );
        assert_eq!(none.validate(), Ok(()));

        #[cfg(target_os = "linux")]
        {
            use crate::egress::Flags;
            use js_types::JobNetwork;
            let program = crate::egress::stand_in::StandIn::new("contract-ceiling");
            let program = program.path();
            let allow = ["192.0.2.10:443".to_owned()];
            let config = |mode: &'static str, ports: &[String]| {
                Config::from_flags(&Flags {
                    mode: Some(mode),
                    allow: &allow,
                    forward_ports: ports,
                    ip: Some(&program),
                    nft: Some(&program),
                    nsenter: Some(&program),
                    ..Flags::default()
                })
                .expect("a sound configuration")
            };
            let cases = [
                (
                    "open",
                    vec!["80".to_owned(), "8000-8010".to_owned()],
                    Some("80,8000-8010"),
                ),
                ("allow-list", vec!["8080".to_owned()], Some("8080")),
                ("open", Vec::new(), None),
            ];
            for (mode, ports, advertised) in cases {
                let config = config(mode, &ports);
                let hello = reported(&config);
                assert_eq!(hello.capabilities["egress"], mode);
                assert_eq!(
                    hello.capabilities.get("egress-ports").map(String::as_str),
                    advertised,
                    "{mode} {ports:?}"
                );
                let read = NetworkCeiling::from_capabilities(&hello.capabilities);
                assert_eq!(read, Ok(config.ceiling()), "{mode} {ports:?}");
                assert_eq!(hello.validate(), Ok(()));
            }
            assert_eq!(config("open", &[]).ceiling().level, JobNetwork::Open);
        }
    }

    /// Asserts that `host-jobs` is reported as `true` only by an agent started with
    /// `--allow-host-jobs` (the start check takes it only with the crun runtime), that such
    /// a hello still validates, and that without the flag the key is absent. It catches a
    /// server sent to taint a host whose agent cannot launch the host job, and one never
    /// told that a crun agent can.
    #[test]
    fn host_jobs_is_reported_only_with_the_flag() {
        let mut allowed = reported(&Config::none());
        report_host_jobs(&mut allowed, true);
        assert_eq!(
            allowed
                .capabilities
                .get(HOST_JOBS_CAPABILITY)
                .map(String::as_str),
            Some(FLAG_SET)
        );
        assert_eq!(allowed.validate(), Ok(()));
        let mut refused = reported(&Config::none());
        report_host_jobs(&mut refused, false);
        assert!(!refused.capabilities.contains_key(HOST_JOBS_CAPABILITY));
        assert_eq!(HOST_JOBS_CAPABILITY, "host-jobs");
    }

    /// Asserts that `mount-provider.<name>` is reported as `true` for each provider the agent
    /// was given where it reports layered rootfs, and for none where it does not (the kernel
    /// lacks the calls) or where it was given none, and that such a hello validates. Catches
    /// a server told to place a job that names a provider on a host that cannot give the
    /// layer or mount, and a provider the agent was given that its hello never names.
    #[test]
    fn mount_providers_are_reported_only_where_layered_rootfs_is() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "report-providers");
        let socket = |name: &str| (name.to_owned(), dir.path().join(format!("{name}.sock")));
        let given = [socket("tierstore"), socket("scratch-cache")];
        let providers =
            MountProviders::from_flags(&given, &[], None, std::path::Path::new("/nonexistent"))
                .expect("sound flags");
        let mut layered = reported(&Config::none());
        report_mount_providers(&mut layered, &providers, true);
        let named = |hello: &Hello| -> Vec<String> {
            let keys = hello.capabilities.keys();
            keys.filter(|key| key.starts_with("mount-provider."))
                .cloned()
                .collect()
        };
        assert_eq!(
            named(&layered),
            ["mount-provider.scratch-cache", "mount-provider.tierstore"]
        );
        assert_eq!(layered.capabilities["mount-provider.tierstore"], FLAG_SET);
        assert_eq!(layered.validate(), Ok(()));
        let mut bare = reported(&Config::none());
        report_mount_providers(&mut bare, &providers, false);
        assert!(named(&bare).is_empty());
        let mut none = reported(&Config::none());
        report_mount_providers(&mut none, &MountProviders::default(), true);
        assert!(named(&none).is_empty());
    }
}
