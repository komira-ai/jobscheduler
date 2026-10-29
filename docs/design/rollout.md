# Rolling out js-agent with kbf

Companion to [docs/design.md](../design.md), section 17, and to [kbf-on-js-agent.md](kbf-on-js-agent.md), which is the design this file follows
and which carries the reasons, the storage branches and the open decisions. Decisions D42 to D45 are in [decisions.md](decisions.md); the
risks K35 and K36 are in [risks-f.md](risks-f.md); the checks are step 55 ([extraction-steps-d.md](extraction-steps-d.md)).

This file replaces the first version of the plan, which assumed a separate jobscheduler server and a trusted `kbf-executor` shipped with
`js-agent` in one kbf host package. Neither holds under the owner's model: a js-lease-server is deployed and kbf-server leases whole hosts from it, submitting each
action as a run through its run API (the placement libraries run inside kbf's frontends); `js-agent` is standalone on every host (no kbf
process beside it on the task path); and there is no `kbf-executor`. A release therefore coordinates three things: the kbf service (frontends
with the embedded libraries and the lease client, and kbf's storage and daemon roles), the `js-agent` service (both roll through kbf's update
coordinator), and the lease server, whose update path is open (design Q23). This file says who updates what, which versions must
work together, and in what order a change rolls out.

## What exists: kbf's update coordinator

The facts below are from kbf at commit `89af8f2ef` and describe the coordinator as it is.

- It is multi-service. A service is data (`crates/kbf-coordinator/src/services.rs`): a name, the roles it runs on with limits, and `after`,
  the services whose live hosts must all run their desired version before any host of this service is ordered. `kbf` is built in. A services
  file defines others (kbf#1818 to #1825). One update per host at a time.
- Described services need the updater and the helper to handle them: kbf#1826 (open) covers the Linux local socket only, and the Mac helper
  serves `kbf` only.
- On the host, a helper installs one signed package, verified against kbf's release signer, with per-node drain, switch, health check and
  confirm-or-revert. Updates are host-driven and pull-shaped.

## Who updates what

| Part | Updated by | How |
|---|---|---|
| kbf frontends with the embedded jobscheduler libraries | kbf's update coordinator, service `kbf` | The libraries are a git dependency at one revision, raised by a pull request of its own in kbf (`Cargo.toml`). The embedded server version is whatever kbf pins. |
| js-lease-server | jobscheduler's release | Deployed. Its runs live in memory, so an update drops all of them (`410 run-forgotten`) and kbf re-queues; roll it with no run in flight or accept the requeue. |
| `js-agent` | kbf's update coordinator, service `js-agent`, `after = ["kbf"]` | A service defined in the coordinator's services file. The agent ships from jobscheduler's release; until the signer decision below it is republished under kbf's signer. |
| Action images and `kbf-task` | the job (action image), kbf (`kbf-task`) | Action images are digest-pinned and pre-pulled; `kbf-task` is mounted from the `kbf-tools` agent root. The frontend names the image digest and the tools version in each run, and both are on every host before frontend N places work. |
| Tierstore front | kbf's release workflow | A tarball and example unit, not started at install and not managed by the host updater. Its presigned-PUT verifier is rolled before the frontend that mints for it. |
| Hot and cold Silo | the operator | A pinned Silo release under systemd; the update and rollback path is the storage design's. |
| Elastic machines | not updated | A machine launched by an elastic provider boots the current pinned agent, so scale-up is its update path; an old machine leaves by the idle rule (5.10). |
| Macs | the Mac helper | Until a Mac task runtime exists the Macs stay on the kbf daemon. Task bundles and the VM base are OCI artifacts pinned by digest in the signed release manifest and written to a root-owned allow file by the updater. |

**D42: the coordinator is already multi-service, and `js-agent` is one of its services.** The earlier decision not to generalise it rested on
every agent shipping inside the kbf package. That premise is gone: the agent is a separate service with its own release.

## Version compatibility

Things that talk across a version boundary: the agent and the embedded server (the agent wire, 10.3), kbf frontends of version N and N-1 on
one shared Postgres, a run spec and the action image and `kbf-tools` version it names, the lease client and the lease server's run API, and the task tool's pack writer and the frontend's pack verifier. The rules (D44):

1. **Protocol majors, additive fields inside a major.** A change inside a major is an optional field (R3); an old peer skips what it does not
   know. A new major is a new package or service that runs beside the old one until no peer uses the old (10.3).
2. **The server that serves agents (the deployed lease server) speaks every protocol major its agents use.** It retires a major only after the last agent using it has gone. The
   agent is released from main while the embedded server is at kbf's pin, so skew between them is normal, and a skew test in jobscheduler's CI
   runs the newest agent against kbf's pinned server and the reverse.
3. **The server refuses an agent major it does not speak.** A `Hello` whose major is unknown gets a `Welcome` with a refusal code (5.8); the
   agent exits with status 2 and keeps its containers for adoption, so a host left behind fails closed and visibly (K35).
4. **Security fields are gated on `Hello.capabilities`, never on version text.** An old receiver silently ignores unknown fields, so a field
   that carries a secret, a network profile or an upload grant is sent only to an agent that advertises it. An old agent that ignores `upload` returns no upload outcome, which the frontend treats as a failed action, never as "no outputs".
5. **Run spec versus the images.** The frontend names the action image digest and the `kbf-tools` version in the run; a host without them refuses the run with a
   plain constant code. This replaces the version-pair check between the agent and an executor.
6. **One pack format, one crate.** The task tool writes and the frontend verifies the same `js-pack-format`. The frontend verifies versions N and N-1
   and sends an upload grant only with a `kbf-tools` version whose tool writes a format it verifies.

## Rollout order

1. **The kbf service first, in kbf's own order** (D43): storage and daemon roles, then frontends last. The embedded server is the kbf frontend.
2. **Then `js-agent`**, ordered `after = ["kbf"]`, so no agent is updated until every kbf host runs its desired version. Never two services on
   one host at once (kbf#1825). Per host: the updater installs on trial, the host is cordoned and drained until it holds no run (bounded by a
   cap, because the agent's own SIGTERM stops jobs after `--drain-wait`), the helper switches, the agent says `Hello`, the ready check passes,
   the helper confirms or reverts, and the host is uncordoned.
3. **Action images and `kbf-tools`** are staged before the frontend that names them places work. Rollback of the executor path is the flag: `--executor kbf-daemon` (the default) needs no package change.

A change that adds a field or feature rolls in this order and needs nothing more. A change that retires a major adds a step at the end: the
server drops the major only after the agents that used it have gone (rule 2). Rollback is the reverse: kbf by setting an older version, the
agent by the helper's trial-and-revert (containers are adopted by the restarted agent; a taint persists), images by re-issuing the previous
digest; no rollback needs a format change, because formats change additively (R3).

## What would change the plan (D45)

- **A `js-agent` population that does not run kbf and must be updated.** Then a second coordinator instance for that population, or
  replace-not-update for elastic machines.
- **Agent fixes that must outrun kbf releases.** The agent has its own service, so this is the normal case; the limit is kbf's republish step.
- **The signer decision (K36).** The kbf helper verifies against kbf's release signer. Trusting jobscheduler's release workflow directly
  would make anyone who can push its `main` root on every host, because the agent runs as root and `main` has no rulesets. The plan is for kbf
  to republish the agent under its own signer, with branch protection on jobscheduler first; trusting jobscheduler's workflow is reopened only
  after rulesets and a protected release environment. A security decision for the project owner, taken before any production install.
- **A Mac task runtime.** Until one exists the Macs are not on this plan.

## What stays open

The storage branch (tierstore with Silo, presigned-PUT writes) and the Mac packaging direction are decided; the credential holder (the whole task holds the presigned URL: decided 2026-10-29), the lease-server integration, the task tool and its inputs (decided 2026-10-29), the Mac default, the elastic provisioner's home, and the other remaining decisions are in
[kbf-on-js-agent.md](kbf-on-js-agent.md), section 15. Deploying a service, installing a package on hosts and any registry upload are
operator acts that need the project owner's go for that action (R7).
