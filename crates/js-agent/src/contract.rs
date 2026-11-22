//! The host contract the hello reports (design: pools, providers and host networking,
//! section 3.6): `runtime`, `exec` and `gpu-toolkit`, from what the agent actually found, and
//! `gpu-passthrough`.
//!
//! The capabilities are not flags. An operator cannot set them with `--capability`
//! ([`js_caps::DETECTED_KEYS`]), so a pool that requires them is not satisfied by a
//! host's say-so:
//!
//! - `runtime` and `exec` are `container` for an agent that runs container jobs: the engine
//!   of `--allow-container-engine --container-bin`, or `crun`. The hello is sent only after the job
//!   half has started, which for the engine opened the program the flag names, ran it
//!   (`--version`) and found Docker or Podman; a runtime that does not start ends the agent
//!   first, so no hello names a runtime that was not found. An agent that runs no jobs
//!   (`--no-jobs`) reports neither.
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
//! - `gpu-passthrough` is `true` for a Linux agent (the crun runtime) given `--gpu-device`, whose
//!   flags passed the start check: its runtime gives a job the device nodes, the driver mounts
//!   and the variables of the GPUs it booked. It is for the operator; a GPU job is placed by
//!   the GPUs a host reports, as before.
//! - `gpu-toolkit` is `true` when the host offers GPUs and `--gpu-toolkit-hook` ran and
//!   exited 0. The hook is the operator's check that a container can use a GPU (for
//!   example, a container that runs `nvidia-smi`); the agent knows only its exit status.
//!   With no hook, with no GPU offered, or with a hook that fails, the capability is
//!   absent, and a pool that needs it refuses the host `host-contract`.
//! - `per-job` is `true` for every agent, a container one and a native one: it takes the
//!   job-keyed start and stop and lists those jobs in `jobs`
//!   ([`js_agent_wire::PER_JOB_CAPABILITY`]), so a server sends them only to a host that
//!   reports it. A native agent runs a job start whose image is a bundle and which carries
//!   a `run`; it refuses any other. An operator cannot set it either.
//! - `layered-rootfs` is `true` for a crun agent on a kernel with the calls it needs, and
//!   for a native agent: the native runtime gives a job mounts of host paths below `/work`
//!   (links in its `exec/`, with Seatbelt grants) and refuses layers, a mount target
//!   outside `/work` and a remapped host port at admission.
//! - `start-fence` is `true` wherever `per-job` is: the agent judges a job start by its
//!   fence (the job id with the allocation id), keeps the fences it retired in its state root,
//!   and stops every job after a silence of the server
//!   ([`js_agent_wire::START_FENCE_CAPABILITY`]). An operator cannot set it.
//! - `held-beat` is `true` wherever `per-job` is: the session sends its next heartbeat at
//!   once after a reply the server held open ([`js_agent_wire::HELD_BEAT_CAPABILITY`]).
//! - `dispatch-poll` is `true` wherever `per-job` is: the agent polls
//!   [`js_agent_wire::DISPATCH_PATH`] on its own thread for a session whose welcome says
//!   `dispatch_poll`, and takes its starts and stops from the poll's answers
//!   ([`js_agent_wire::DISPATCH_CAPABILITY`], [`crate::dispatch`]). A server that serves the poll
//!   leaves them out of the heartbeat's reply only for a host that reports it. An operator
//!   cannot set it.
//! - `load-admission` is `true` wherever `per-job` is: the agent admits a job start by the job
//!   service's placement and never refuses one for the summed CPU or memory bookings of the
//!   jobs it holds; it still refuses one larger than the machine and still sums disk, GPUs and
//!   slots ([`js_agent_wire::LOAD_ADMISSION_CAPABILITY`]). A server packs a host that reports it
//!   by measured load, and one that does not by summed bookings. An operator cannot set it.
//! - `host-jobs` is `true` for an agent started with `--allow-host-jobs`, which only
//!   the crun runtime takes: its runtime launches a job with `isolation = host`
//!   ([`js_agent_wire::HOST_JOBS_CAPABILITY`]). The server taints a host when it places such
//!   a job, so no other agent reports it. An operator cannot set it.
//! - `fuse-jobs` is `true` for a crun agent started with `--allow-fuse-jobs` whose kernel is at
//!   least 6.14 and whose `/dev/fuse` is the FUSE device, and `fuse-uring` with it where the
//!   fuse module's `enable_uring` is on and `kernel.io_uring_disabled` is 0 (the probe is
//!   [`crate::fuse_env`]); a flag whose probe fails is a warning naming what is missing and
//!   reports neither. A server sends a start with `fuse` only to a host that reports
//!   `fuse-jobs`. An operator cannot set either.
//! - `mount-provider.<name>` is `true` for each node-local mount provider a crun agent was
//!   given (`--mount-provider`), where the agent reports `layered-rootfs` (a provider's tree
//!   is a layer or a mount of the job). A server sends a job that names a provider only to a
//!   host that reports it. An operator cannot set it (`js_caps::is_mount_provider_key`).

use js_agent_wire::{
    DISPATCH_CAPABILITY, HELD_BEAT_CAPABILITY, Hello, LOAD_ADMISSION_CAPABILITY,
    PER_JOB_CAPABILITY, START_FENCE_CAPABILITY,
};
use js_caps::{FLAG_SET, LAYERED_ROOTFS, RUN_FILES};

#[cfg(target_os = "linux")]
mod container;
#[cfg(target_os = "linux")]
pub(crate) use container::{
    ToolkitHook, report, report_fuse, report_host_jobs, report_mount_providers,
};

/// Says that the agent takes the job-keyed start and stop, that it fences them (it refuses a
/// start whose fence it retired and stops its jobs after a silence), and that it beats again
/// at once after a held reply (`held-beat`, the session's), and that it polls the dispatch
/// route when its welcome says the server serves it (`dispatch-poll`, [`crate::dispatch`]), and
/// that it admits a job start by the job service's placement, never refusing one for the
/// summed CPU or memory bookings it holds (`load-admission`, the lifecycle's admission):
/// every agent does, the container runtime and the native one.
pub(crate) fn report_per_job(hello: &mut Hello) {
    hello
        .capabilities
        .insert(PER_JOB_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    hello
        .capabilities
        .insert(START_FENCE_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    hello
        .capabilities
        .insert(HELD_BEAT_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    hello
        .capabilities
        .insert(DISPATCH_CAPABILITY.to_owned(), FLAG_SET.to_owned());
    hello
        .capabilities
        .insert(LOAD_ADMISSION_CAPABILITY.to_owned(), FLAG_SET.to_owned());
}

/// Puts `layered-rootfs` into `hello` when the runtime gives jobs mounts of host paths
/// (`layered`: the crun runtime when the kernel has the calls; the native runtime always,
/// and it refuses layers itself).
pub(crate) fn report_layered(hello: &mut Hello, layered: bool) {
    if layered {
        hello
            .capabilities
            .insert(LAYERED_ROOTFS.to_owned(), FLAG_SET.to_owned());
    }
}

/// The job half of a native agent's hello: `per-job`, `start-fence`, `held-beat`,
/// `dispatch-poll` and `layered-rootfs`.
/// The native runtime takes the job-keyed start (a bundle image with a `run`) and gives a
/// job mounts of host paths below `/work`; layers are refused at admission.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn report_native_jobs(hello: &mut Hello) {
    report_per_job(hello);
    report_layered(hello, true);
}

/// Says that the agent writes a start's run files, when it has a files root (`run_files`);
/// an older agent would ignore them, and one without a root refuses them.
pub(crate) fn report_run_files(hello: &mut Hello, run_files: bool) {
    if run_files {
        hello
            .capabilities
            .insert(RUN_FILES.to_owned(), FLAG_SET.to_owned());
    }
}
