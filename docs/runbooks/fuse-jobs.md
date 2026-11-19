# The FUSE test environment (`--allow-fuse-jobs`)

A kernel test of a FUSE file system needs to mount one, and a contained job cannot: its container
has no `/dev/fuse` and no capability. An operator can opt a host in so that a job that asks for it
is given the FUSE device and a capability that works inside its own user namespace only. This is
the farm's privileged-test environment for such tests. It is not `isolation = host`
([host-jobs.md](host-jobs.md)): the host is not tainted, the job does not run as root, and it has
no network.

## What the job gets, and what it does not

A start with `fuse` set, on an agent that allows it, runs the job in its usual container with three
differences:

- **The device.** `/dev/fuse` (character device 10:229, mode 0666) is in the job's `/dev`, and the
  device cgroup allows the job to read and write it.
- **One capability.** `CAP_SYS_ADMIN` is in all five of the process's capability sets. It is a
  capability of the container's own user namespace: the job's ids map to the job user's
  subordinate ids, and `crun` runs as the non-root job user. It gives the job power over its own
  namespaces (it can mount a FUSE file system, and any other type a user namespace may mount, and
  undo the masked and read-only paths of its own mount namespace) and none over the host's.
- **io_uring.** The seccomp filter is the pinned profile resolved for `CAP_SYS_ADMIN`, and the
  three io_uring system calls (`io_uring_setup`, `io_uring_enter`, `io_uring_register`) are
  allowed on top of it, so a FUSE file system can use FUSE over io_uring.

Everything else is unchanged: no host root; the network stays `none`; `noNewPrivileges` stays set
(so the setuid `fusermount3` is not a route: the job mounts with `mount(2)` and an `fd=` option
naming the descriptor it opened on `/dev/fuse`); the namespaces, the cgroup limits and the
mounts are set up as before. A job that asks for the device with `isolation = host` is refused:
a host container sees the host's own devices.

The capability lets the job reach more of the kernel than a plain job (mounts, its network
namespace's settings, `bpf` and `perf_event_open` under the kernel's own sysctls), and a kernel bug
there is a way to the host. Start a host with `--allow-fuse-jobs` only where every job that can be
placed on it is trusted as much as a kernel test is: a pool kept for kernel tests, pinned by
`fuse-jobs`. docs/design/agent-privilege.md ("The FUSE device") gives the argument.

## What the host needs

| Need | For | How |
|---|---|---|
| Kernel 6.14 or newer | `fuse-jobs` | The agent reads `/proc/sys/kernel/osrelease`. |
| `/dev/fuse` is the FUSE device (char 10:229) | `fuse-jobs` | The `fuse` module loaded or built in. |
| `fuse.enable_uring=1` | `fuse-uring` | On the kernel command line, set at boot by the node image (hostkeeper). Never at runtime. |
| `kernel.io_uring_disabled=0` | `fuse-uring` | The sysctl's default; a node image that sets it to 1 or 2 turns io_uring off. |

A kernel without the `enable_uring` parameter counts as off. A kernel without the
`io_uring_disabled` sysctl counts as on.

## Opting a host in

Add the line `--allow-fuse-jobs` to `/etc/js-agent/flags` and restart the agent service. The flag
is Linux only and is taken with the `crun` runtime only: `js-agent` refuses it at start with
`--allow-container-engine`, which has no way to give a job the device.

The agent then checks the host. When the kernel and `/dev/fuse` are right, its hello carries
`fuse-jobs=true`; when the `enable_uring` parameter is also on and `kernel.io_uring_disabled` is
0, it carries `fuse-uring=true` as well (never `fuse-uring` without `fuse-jobs`). When a condition
fails, the agent still starts, logs a warning that names what is missing
(`--allow-fuse-jobs: fuse-jobs is not reported: ...` or `--allow-fuse-jobs: fuse-uring is not
reported: ...`), and does not report that key. A host that does not report `fuse-jobs` refuses a
start with `fuse` (`fuse-jobs-not-allowed`), creating nothing.

`--capability fuse-jobs=...` and `--capability fuse-uring=...` are refused as detected keys: only
the checks above set them.

## Asking for it, and pinning a pool to such hosts

A test asks for the environment with the platform or exec properties `fuse-jobs=true` and, for a
case that needs FUSE over io_uring, `fuse-uring=true`. `js_caps::Request::from_platform` reads
them as it reads the other capability keys, so the scheduler places the job on a host whose hello
reported the keys, and on no other. The start the agent receives carries `fuse`.

A pool that must hold only such hosts names `fuse-jobs = "true"` in its `[capabilities]` table
(see [farm-node-enroll.md](farm-node-enroll.md)). A host that joins it without reporting the key
is refused `below-floor`: give it the flag, a kernel of 6.14 or newer, and a restart.

## What is not in this change

The job API (`JobSpec`) has no field yet to ask for the device, so a server sends `fuse` to no
host until that field lands in a later change. The agent side, the host keys and the container
are in place and tested; a job's sandbox is the same everywhere else.

## The farm smoke test

Two ignored tests in `crates/js-agent/tests/crun_runtime/fuse.rs` show the environment on a real
host:

- `a_fuse_job_mounts_fuse_in_its_own_namespace_and_reads_init` runs a busybox job with `fuse`:
  it sees `/dev/fuse` as 10:229, holds `CAP_SYS_ADMIN` and `NoNewPrivs`, sees only `lo`, mounts
  a FUSE file system on its own and reads the kernel's `FUSE_INIT` request from the descriptor.
  A second run without `fuse` sees no device, holds no capability and cannot mount. It needs
  what the target needs plus `/dev/fuse`, so the `js-agent-crun-runtime` workflow runs it.
- `this_host_offers_fuse_jobs` runs the agent's own check on the host and asserts the host
  offers `fuse-jobs`; it prints the `fuse-uring` answer without asserting it. It needs kernel
  6.14 or newer; the workflow's runners have 6.8, so it skips the test by name.

To run them on a farm node: cordon the node ([cordon-a-node.md](cordon-a-node.md)), build the
target (`cargo test -p js-agent --test crun_runtime --locked --no-run`), run the target's binary
as root in a `Delegate=yes` unit with the job user and image the workflow gives it (the
workflow's step "Run the conformance suite over CrunRuntime in a Delegate=yes unit, as root"
shows the command), with `--include-ignored` replaced by `--ignored --exact` and the two tests'
full names (`fuse::a_fuse_job_mounts_fuse_in_its_own_namespace_and_reads_init`,
`fuse::this_host_offers_fuse_jobs`) as the filters. `cargo xtask crun-tests` runs js-contain's
`crun_root` target, not this one, so it is not the way. Uncordon the node afterwards.
