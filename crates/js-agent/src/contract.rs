//! The host contract the hello reports (design: pools, providers and host networking,
//! section 3.6): `runtime`, `exec` and `gpu-toolkit`, from what the agent actually found.
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
//! - `gpu-toolkit` is `true` when the host offers GPUs and `--gpu-toolkit-hook` ran and
//!   exited 0. The hook is the operator's check that a container can use a GPU (for
//!   example, a container that runs `nvidia-smi`); the agent knows only its exit status.
//!   With no hook, with no GPU offered, or with a hook that fails, the capability is
//!   absent, and a pool that needs it refuses the host `host-contract`.

use std::path::Path;

use js_agent_wire::Hello;
use js_caps::{EXEC, FLAG_SET, GPU_TOOLKIT, RUN_FILES, RUNTIME};
use js_types::ExecMode;

use crate::cli::Cli;
use crate::facts::CommandRunner;

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

/// Adds the capabilities of the host contract to the hello of an agent that runs
/// containers. Returns the warnings the operator should read.
pub(crate) fn report(
    hello: &mut Hello,
    hook: Option<&ToolkitHook>,
    commands: &dyn CommandRunner,
    run_files: bool,
) -> Vec<String> {
    let mode = ExecMode::Container.name().to_owned();
    hello.capabilities.insert(RUNTIME.to_owned(), mode.clone());
    hello.capabilities.insert(EXEC.to_owned(), mode);
    // This agent writes a start's run files (when it has a files root); an older one
    // would ignore them.
    if run_files {
        hello
            .capabilities
            .insert(RUN_FILES.to_owned(), FLAG_SET.to_owned());
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
