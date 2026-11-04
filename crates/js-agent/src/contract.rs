//! The host contract the hello reports (design: pools, providers and host networking,
//! section 3.6): `runtime`, `exec` and `gpu-toolkit`, from what the agent actually found, and
//! `gpu-passthrough`.
//!
//! The capabilities are not flags. An operator cannot set them with `--capability`
//! ([`js_caps::DETECTED_KEYS`]), so a pool that requires them is not satisfied by a
//! host's say-so:
//!
//! - `runtime` and `exec` are `container` for an agent given `--container-bin`. The hello
//!   is sent only after the job half has started, which opened the program the flag names,
//!   ran it (`--version`) and found Docker or Podman; an engine that does not run ends the
//!   agent first, so no hello names a runtime that was not found. An agent that runs no
//!   jobs (no `--container-bin`) reports neither.
//! - `run-files` is `true` for an agent with a run files root (`--run-files-root`, on a
//!   tmpfs): it writes a start's `files` and mounts them at `/run/js-files`. The server sends a start with files to a host that reports it only.
//! - `scratch-quota` is `project` and `scratch-default-gib` is the GiB a run without
//!   `disk_bytes` books, for an agent started with `--scratch-quota` whose scratch root
//!   passed the probe (an agent whose root did not pass does not start). The server books
//!   that default for such a run and places a job that sets `disk_bytes` only on a host
//!   that reports `scratch-quota`.
//! - `egress` is the ceiling of a container agent's jobs (`--egress`: `none`, `allow-list` or
//!   `open`; always `none` for Docker and Podman, which refuse the other two), and of a native
//!   agent's (`--native-network`: `off` and `loopback` report `none`, `on` `allow-list`,
//!   `open` `open`), as [`js_agent_wire::NetworkCeiling`] writes it. `egress-ports` (the host ports a job may
//!   declare) is not reported until declared ports are forwarded. A server places a job that
//!   asks for more than `none` only on a host whose `egress` covers it; an agent from before
//!   the key reports none, which reads as `none`.
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

use std::path::Path;

use js_agent_wire::{Hello, NetworkCeiling, PER_JOB_CAPABILITY};
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
    /// job).
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
/// jobs: `runtime` and `exec` are `sandbox-exec`, and `egress` is the agent's
/// `--native-network` ceiling as a job level ([`Ceiling::advertised`]: `off` and `loopback`
/// are `none`, `on` is `allow-list`, `open` is `open`), with no `egress-ports`, since a Mac
/// forwards no port. The hello is sent only after the job half has started, which asked the
/// helper for its boot reset, so no hello names a mode the agent cannot serve. Returns the
/// warnings the operator should read: none.
pub(crate) fn report_native(hello: &mut Hello, ceiling: Ceiling) -> Vec<String> {
    let mode = ExecMode::SandboxExec.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    ceiling.advertised().insert_into(&mut hello.capabilities);
    Vec::new()
}

/// Says that the agent takes the job-keyed start and stop: every agent that runs containers
/// does. A native agent does not say it: a job start carries an image, which its runtime
/// cannot run.
fn report_per_job(hello: &mut Hello) {
    hello
        .capabilities
        .insert(PER_JOB_CAPABILITY.to_owned(), FLAG_SET.to_owned());
}

/// Adds the capabilities of the host contract to the hello of an agent that runs
/// containers. `scratch_default_gib` is the default of an agent whose scratch quota is on;
/// `gpu_passthrough` says the runtime gives jobs the GPUs they book, and `ceiling` is the most
/// network its jobs may ask for. Returns the warnings the operator should read.
pub(crate) fn report(
    hello: &mut Hello,
    hook: Option<&ToolkitHook>,
    commands: &dyn CommandRunner,
    run_files: bool,
    scratch_default_gib: Option<u64>,
    (gpu_passthrough, ceiling): (bool, &NetworkCeiling),
) -> Vec<String> {
    let mode = ExecMode::Container.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    ceiling.insert_into(&mut hello.capabilities);
    report_per_job(hello);
    // This agent writes a start's run files (when it has a files root); an older one
    // would ignore them.
    if run_files {
        hello
            .capabilities
            .insert(RUN_FILES.to_owned(), FLAG_SET.to_owned());
    }
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
            (false, &config.advertised()),
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

    /// Catches a container agent that does not say its ceiling, says it under another key, or
    /// claims ports it does not forward: built from `--egress none` (the default) the hello
    /// carries `egress=none`, from `--egress open` with forward ports `egress=open` and no
    /// `egress-ports`, the wire's reader reads each back as written, and both hellos are
    /// valid. The agent still holds a job to the ports it was given.
    #[test]
    fn the_ceiling_is_advertised_in_hello() {
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
            let ports = ["80".to_owned(), "8000-8010".to_owned()];
            let program = crate::egress::stand_in::StandIn::new("contract-ceiling");
            let program = program.path();
            let config = Config::from_flags(&Flags {
                mode: Some("open"),
                forward_ports: &ports,
                ip: Some(&program),
                nft: Some(&program),
                nsenter: Some(&program),
                ..Flags::default()
            })
            .expect("open");
            let open = reported(&config);
            assert_eq!(open.capabilities["egress"], "open");
            assert!(
                !open.capabilities.contains_key("egress-ports"),
                "ports are not forwarded yet"
            );
            let read = NetworkCeiling::from_capabilities(&open.capabilities);
            assert_eq!(read.map(|ceiling| ceiling.level), Ok(JobNetwork::Open));
            assert_eq!(open.validate(), Ok(()));
            assert_eq!(config.ceiling().ports_text(), "80,8000-8010");
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
            assert!(report_native(&mut hello, ceiling).is_empty());
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
