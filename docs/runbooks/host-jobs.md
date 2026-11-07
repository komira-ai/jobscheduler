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

**No runtime of `js-agent` launches a host job yet.** Until one does, `js-agent` refuses
`--allow-host-jobs` at start with an error that names the missing launch path, no agent reports
`host-jobs`, and `--capability host-jobs=...` is refused as a detected key. Every host job is
therefore refused `no-capable-host` at submit, and no host is tainted for a job it cannot start.
The server's side above is in place for the agent that will report the capability.
