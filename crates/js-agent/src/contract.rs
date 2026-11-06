//! The host contract the hello reports (design: pools, providers and host networking,
//! section 3.6): `runtime`, `exec` and `gpu-toolkit`, from what the agent actually found, and
//! `gpu-passthrough`.
//!
//! The capabilities are not flags. An operator cannot set them with `--capability`
//! ([`js_caps::DETECTED_KEYS`]), so a pool that requires them is not satisfied by a
//! host's say-so:
//!
//! - `runtime` and `exec` are `container` for an agent that runs container jobs: the engine
//!   of `--runtime docker --container-bin`, or `crun`. The hello is sent only after the job
//!   half has started, which for the engine opened the program the flag names, ran it
//!   (`--version`) and found Docker or Podman; a runtime that does not start ends the agent
//!   first, so no hello names a runtime that was not found. An agent that runs no jobs
//!   (`--runtime none`) reports neither.
//! - `run-files` is `true` for an agent with a run files root (`--run-files-root`): a
//!   container agent's is on a tmpfs, and it writes a start's `files` and mounts them at
//!   `/run/js-files`; a native agent's is where the operator put it, and it gives a run's
//!   files to the run's user and names their directory in `JS_FILES_DIR`. A native agent
//!   without the flag has no root and does not report it. The server sends a start with
//!   files to a host that reports it only.
//! - `scratch-quota` is `project` and `scratch-default-gib` is the GiB a run without
//!   `disk_bytes` books, for an agent started with `--scratch-quota` whose scratch root
//!   passed the probe (an agent whose root did not pass does not start). The server books
//!   that default for such a run and places a job that sets `disk_bytes` only on a host
//!   that reports `scratch-quota`.
//! - `egress` is the ceiling of a container agent's jobs (`--egress`: `none`, `allow-list` or
//!   `open`; always `none` for Docker and Podman, which refuse the other two), and of a native
//!   agent's (`--native-network`: `off` and `loopback` report `none`, `on` `allow-list`,
//!   `open` `open`, never with ports), as [`js_agent_wire::NetworkCeiling`] writes it.
//!   `egress-ports` is the host ports a job may declare and the agent forwards
//!   (`--egress-forward-port`), reported only under a wired ceiling that lists some
//!   (forwarding needs one). A server places a job that asks for more than `none` only on a
//!   host whose `egress` covers it; an agent from before the key reports none, which reads
//!   as `none`.
//! - `gpu-passthrough` is `true` for a `--runtime crun` agent given `--gpu-device`, whose
//!   flags passed the start check: its runtime gives a job the device nodes, the driver mounts
//!   and the variables of the GPUs it booked. It is for the operator; a GPU job is placed by
//!   the GPUs a host reports, as before.
//! - `gpu-toolkit` is `true` when the host offers GPUs and `--gpu-toolkit-hook` ran and
//!   exited 0. The hook is the operator's check that a container can use a GPU (for
//!   example, a container that runs `nvidia-smi`); the agent knows only its exit status.
//!   With no hook, with no GPU offered, or with a hook that fails, the capability is
//!   absent, and a pool that needs it refuses the host `host-contract`.
//! - `per-job` is `true` for an agent that runs containers: it takes the job-keyed start
//!   and stop and lists those jobs in `jobs` ([`js_agent_wire::PER_JOB_CAPABILITY`]), so a
//!   server sends them only to a host that reports it. A native agent does not report it (a
//!   job start carries an image, which it cannot run). An operator cannot set it either.
//! - `start-fence` is `true` wherever `per-job` is: the agent judges a job start by its
//!   fence (the job id with the allocation id), keeps the fences it retired in its state root,
//!   and stops every job after a silence of the server
//!   ([`js_agent_wire::START_FENCE_CAPABILITY`]). An operator cannot set it.

use std::path::Path;

use js_agent_wire::{
    Hello, LOG_UPLOAD_CAPABILITY, NetworkCeiling, PER_JOB_CAPABILITY, START_FENCE_CAPABILITY,
};
use js_caps::{
    EXEC, FLAG_SET, GPU_PASSTHROUGH, GPU_TOOLKIT, RUN_FILES, RUNTIME, SCRATCH_DEFAULT_GIB,
    SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT,
};
use js_types::ExecMode;

use crate::cli::Cli;
use crate::facts::CommandRunner;
use crate::native::Ceiling;

/// The `PATH` the toolkit hook runs with; it inherits nothing else.
const HOOK_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The checked `--gpu-toolkit-hook`: the absolute path of the program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolkitHook(String);

impl ToolkitHook {
    /// The hook the flags name; none without `--gpu-toolkit-hook`.
    ///
    /// # Errors
    ///
    /// The text names the flag: the path is not absolute, or `--container-bin` is not
    /// given (the toolkit is the container engine's, and an agent without one runs no
    /// engine job).
    pub(crate) fn from_cli(cli: &Cli) -> Result<Option<Self>, String> {
        let Some(path) = &cli.gpu_toolkit_hook else {
            return Ok(None);
        };
        if cli.container_bin.is_none() {
            return Err("--gpu-toolkit-hook: needs --container-bin".to_owned());
        }
        if !Path::new(path).is_absolute() {
            return Err(format!(
                "--gpu-toolkit-hook: {path} is not an absolute path"
            ));
        }
        Ok(Some(Self(path.clone())))
    }
}

/// Adds the capabilities of the host contract to the hello of an agent that runs native
/// jobs: `runtime` and `exec` are `sandbox-exec`, `run-files` when `run_files` (the agent
/// has a files root), and `egress` is the agent's `--native-network` ceiling as a job level
/// ([`Ceiling::advertised`]: `off` and `loopback` are `none`, `on` is `allow-list`, `open` is
/// `open`), with no `egress-ports`, since a Mac forwards no port, and `log-upload`, since the
/// native runtime uploads live logs (`native/logs.rs`). The hello is sent only after
/// the job half has started, which asked the helper for its boot reset, so no hello names a
/// mode the agent cannot serve. Returns the warnings the operator should read: none.
pub(crate) fn report_native(hello: &mut Hello, run_files: bool, ceiling: Ceiling) -> Vec<String> {
    let mode = ExecMode::SandboxExec.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    report_run_files(hello, run_files);
    // The native runtime uploads the live logs of every run whose start carries a sink.
    hello
        .capabilities
        .insert(LOG_UPLOAD_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    ceiling.advertised().insert_into(&mut hello.capabilities);
    Vec::new()
}

/// Says that the agent writes a start's run files, when it has a files root (`run_files`);
/// an older agent would ignore them, and one without a root refuses them.
fn report_run_files(hello: &mut Hello, run_files: bool) {
    if run_files {
        hello
            .capabilities
            .insert(RUN_FILES.to_owned(), FLAG_SET.to_owned());
    }
}

/// Says that the agent takes the job-keyed start and stop, and that it fences them (it
/// refuses a start whose fence it retired and stops its jobs after a silence): every agent
/// that runs containers does. A native agent says neither: a job start carries an image,
/// which its runtime cannot run.
fn report_per_job(hello: &mut Hello) {
    hello
        .capabilities
        .insert(PER_JOB_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    hello
        .capabilities
        .insert(START_FENCE_CAPABILITY.to_owned(), FLAG_SET.to_owned());
}

/// Adds the capabilities of the host contract to the hello of an agent that runs
/// containers. `scratch_default_gib` is the default of an agent whose scratch quota is on;
/// `gpu_passthrough` says the runtime gives jobs the GPUs they book, `ceiling` is the most
/// network its jobs may ask for, and `log_upload` says the runtime uploads live logs (the
/// `crun` runtime; a native agent reports it through [`report_native`]), reported as
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

    use js_agent_wire::{HeldRun, Phase, ProtocolRange, WireShape};

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
    /// jobs after a silence), that the hello still validates, and that a native agent's hello
    /// carries neither. It catches a server told of a fence an agent does not enforce (a
    /// native agent, which takes no job start), and one never told that a container agent
    /// enforces it.
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
        let mut native = hello();
        assert!(report_native(&mut native, true, Ceiling::Off).is_empty());
        assert_eq!(flag(&native, START_FENCE_CAPABILITY), None);
        assert_eq!(flag(&native, PER_JOB_CAPABILITY), None);
        assert_eq!(START_FENCE_CAPABILITY, "start-fence");
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
        let mut native = hello();
        assert!(report_native(&mut native, false, Ceiling::Off).is_empty());
        assert_eq!(
            native
                .capabilities
                .get(LOG_UPLOAD_CAPABILITY)
                .map(String::as_str),
            Some(FLAG_SET)
        );
        assert_eq!(native.validate(), Ok(()));
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
