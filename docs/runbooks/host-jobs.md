# Host jobs (`isolation = host`)

A job normally runs in a container. A job submitted with `isolation = host` runs on the machine
itself, for work that has to change the machine and cannot do that from inside a container. Such a
job leaves the machine in a state nobody can vouch for, so the scheduler taints the machine for
good when the job is placed.

## What the scheduler does

1. **Admission.** `SubmitJob` accepts a host job only when a registered host's agent reported the
   `host-jobs` capability in its hello. Otherwise the job is refused `no-capable-host` and nothing
   is recorded. A machine a provider could launch is never counted: a machine that has not said
   hello has reported nothing.
2. **Placement.** The job is placed only on a host that reported `host-jobs`. The commit that
   places the job also books its allocation and writes the host's taint row in the job store
   (`taint/<host id>`), all in one transaction. If the host was tainted before, the commit fails
   and the job stays queued for another host. The start is sent to the agent only after the commit.
3. **The taint.** Right after the commit the server taints the host in the registry and writes
   the taint into the host's binding: reason `host-job`, run id the job's id. The first taint a
   host has stands, so a host that was already tainted for another reason keeps that reason.
4. **Afterwards.** The host takes no other job and no lease. A host job may be placed on a host
   that already runs contained jobs; those finish as they would, and nothing new is placed
   beside them. The host job itself still starts
   and runs, and its result is recorded like any other job's. The job service's `ListHosts`
   shows `taint_reason` `host-job`; the lease server's own `GET /v1/hosts` shows
   `taint {reason, run_id}`, the run id being the job's id.

The job store's taint row holds the taint until the host's binding does: once the binding with
the taint is written, the row is deleted, and the binding is the taint's record from then on, as
it is for every other taint. If the server stops between the two writes, the next start puts the
row's taint on the binding, writes the bindings and only then deletes the row, so nothing is
lost. The taint survives the host's own hello and a server restart.

A server build from before host jobs reads a host job's row as a contained job and does not read
the taint rows, so do not roll the server back to such a build while a host job is recorded.

The lease-era run API (`POST /v1/leases/{id}/runs`) still refuses host isolation with 422
`host-jobs-not-allowed`. Host jobs are jobs only.

## Taking a host out of service afterwards

Nothing in the scheduler clears a host job's taint: the machine must be reimaged. A host job's
taint is cleared the way any taint is, once the machine is reimaged: stop the server and remove
the `taint_` keys of the host's binding (in the `.agents` file, or in its value in the store).
A job store taint row that a failed write left behind (the server logs a warning that names the
host) is put back on the binding at the next start; start the server once after such a warning,
then stop it and clear the binding. Until it is cleared the host keeps counting toward the
pool's `max_machines`.

## What the agent needs

Start the agent with `--runtime crun --allow-host-jobs`. Only the `crun` runtime launches a host
job, so `js-agent` refuses `--allow-host-jobs` at start on every other runtime (the container
engine of `--container-bin` and the native runtime of a Mac run contained jobs only). An agent
that took the flag reports `host-jobs` in its hello; no other agent does, and
`--capability host-jobs=...` is refused as a detected key. Without an agent that reports it, every
host job is refused `no-capable-host` at submit, and no host is tainted for a job it cannot start.
An agent started without the flag refuses a host start it is sent (`host-jobs-not-allowed`) and
creates nothing.

## What host isolation means

A host job is an OCI container that `crun` runs as root and that shares the machine:

- **The host's namespaces.** Its processes are in the host's pid, network, mount, IPC, UTS and
  cgroup namespaces, with no user namespace: its root is the machine's root, it sees and can signal
  every process, and it has the machine's network whatever the agent's `--egress` ceiling.
- **The host's `/`.** Its root is the machine's own file system, as it is mounted, writable, and it
  is given no mount of its own: it starts in the run's scratch directory and reaches every other
  path at its place on the host. A host job that asks for a mount, run files or a GPU is refused (it
  sees the host's own paths and devices).
- **Every privilege.** It runs as uid 0 with every capability, no seccomp filter and no masked
  path, so it can install packages, load a kernel module or write anywhere.
- **Its own cgroup.** It runs in `ctr` below its lease's cgroup, which the booking limits as it
  does a contained job's: its CPU, memory and pids are accounted and bounded there, and a stop
  (SIGTERM, then the lease's `cgroup.kill` once the grace is spent) or a cancel ends it whole,
  whatever it started. Being root, a host job can move a process of its own out of that cgroup;
  that is one of the reasons the machine is tainted for good.

The image's config still gives the program and the environment (its entrypoint and the start's
arguments, run from the host's root), and the image is pulled when the job's container is made.

The scheduler's taint is permanent: the machine takes no other job or lease, and it must be
reimaged before it is used again (see "Taking a host out of service afterwards" above).
