# Rolling out two services

Companion to [docs/design.md](../design.md), section 17. Section numbers refer to design.md. Decisions D42 to D45 are in
[decisions.md](decisions.md); the risks K35 and K36 are in [risks-f.md](risks-f.md); the checks are step 55
([extraction-steps-d.md](extraction-steps-d.md)).

jobscheduler is now the distributed service: a server (scheduler, lease API, agent API; 5.16) and `js-agent` on every host. kbf is a
frontend that talks to a jobscheduler instance, and on a host its executor, `kbf-executor`, runs beside `js-agent` (9.2). A release
therefore coordinates two services. This file says who updates what, which versions must work together, and in what order a change
rolls out. It is the plan adopted on the advice of the owner of kbf's update coordinator, who made a read-only assessment of that
coordinator.

## What exists: kbf's update coordinator

The facts below are from that assessment and describe the coordinator as it is today.

- It is single-service. It keeps one desired version per host, and its limits (how many nodes may update at once) are keyed by kbf's four
  roles: frontend, storage, daemon and Mac daemon.
- Each host reports one heartbeat version and one health.
- On the host, a helper installs one signed kbf package (apt on Linux, a kit on the Mac), verified against kbf's release signer.
- Per node it runs: drain, switch, health check, then confirm or revert.

## Who updates what

| Part | Updated by | How |
|---|---|---|
| jobscheduler server | its own Kubernetes redeploy | An operator act with the project owner's go (R7): the Deployment names the new image by digest, `Recreate` (deployment.md, "Upgrades"). Rollback is a redeploy of the previous image on the same volume. |
| `js-agent` and `kbf-executor` on a host | kbf's update coordinator | Both ship in the one kbf host package the helper already installs. The host keeps one version; the coordinator's one version per host covers both. Its slots, drain, health and confirm-or-revert apply unchanged. |
| kbf frontends | kbf's update coordinator | As today, with kbf. A frontend is a client of the lease and run APIs; it is not a jobscheduler component. |
| Elastic machines | not updated | A machine launched by an elastic provider boots the current image, so scale-up is its update path. An old elastic machine leaves by the idle rule (5.10) and is replaced by a current one. |

**D42: the coordinator is not generalised.** A service map (several services, each with its own version, limits and health per host)
would change a working updater and its trust for no present gain, because every host that runs `js-agent` also runs kbf. The coordinator
stays as it is. When kbf's daemon splits into `js-agent` plus `kbf-executor`, the package carries both, and a host reports one version.

## Version compatibility

Three things talk across a version boundary: the agent and the server (the agent wire, 10.3), kbf and the server (the lease and run
APIs), and on a host `js-agent` and `kbf-executor` (the agent's local seam, 5.13 and 9.2). The rules (D44):

1. **Protocol majors, additive fields inside a major.** A change inside a major is an optional field (R3); an old peer skips what it
   does not know. A new major is a new package or service that runs beside the old one until no peer uses the old (10.3).
2. **The server speaks every protocol major its agents and kbf clients use.** It retires a major only after the last peer using it has
   gone, which the agents' reported `agent_version` and the clients' versions show.
3. **The server refuses an agent major it does not speak.** A `Hello` whose major is unknown gets a `Welcome` with a refusal code (5.8),
   the host is not entered in the table, and the agent logs it and keeps its current state. A host left behind therefore fails closed and
   visibly (K35); it does not register and then misbehave.
4. **A frontend checks what the server advertises.** `Welcome` and the server's version reply carry the protocol majors and `features`
   (10.3). A kbf client sends a request only if the server lists what it needs, and otherwise fails closed naming the missing feature,
   as it already does for `needs`. A frontend that is newer than the server it calls waits for the server rather than sending fields the
   server would skip.
5. **A version-pair check between `js-agent` and `kbf-executor`, only if they ever ship apart.** While both come in one package, one
   version covers both and no pair check exists. If they are released separately, the check sits at the agent: the agent knows the
   executor's version from the local seam and refuses a `Start` it cannot pair, with a plain constant code. It is not a service map in
   the coordinator.

## Rollout order

1. **The jobscheduler server first** (D43). Servers before agents is the wire rule (10.3). The new server speaks every protocol major
   that the current agents and kbf clients use, so nothing that runs now breaks when it starts. The outage of a `Recreate` redeploy is
   the stop of the old pod plus the start of the new one; agents and clients retry inside the lease TTL (step 52, deployment.md).
2. **Then hosts.** `js-agent` with `kbf-executor` rolls through kbf's coordinator as part of the kbf host package, with the
   coordinator's own slots and gates. Draining a host takes it out of capacity for new requests (D41) while its leases finish.
3. **Then kbf frontends,** as clients, after the server they call supports what they send. They check the server's advertised version
   (rule 4); a frontend that finds the server too old does not send.

A change that adds a field or feature rolls in this order and needs nothing more. A change that retires a major adds a step at the end:
the server drops the major only after the hosts and frontends that used it have gone (rule 2). Rollback is the reverse: frontends are
reverted by kbf, hosts by the coordinator's revert, the server by a redeploy of the previous image; no rollback needs a format change,
because formats change additively (R3) and an older reader skips new keys (13.6).

## What would change the plan (D45)

- **A `js-agent` population that does not run kbf and must be updated.** The kbf package does not reach it. Then a second coordinator
  instance for that population, or replace-not-update for elastic machines (replace the machine with one booted from the current image).
- **`js-agent` and `kbf-executor` on different release cadences with a required order.** Then the version-pair check at the agent
  (rule 5) first, not a service map in the coordinator.
- **jobscheduler releasing faster than kbf, with urgent agent fixes.** Package coupling then hurts (K35): the fix waits for a kbf host
  release. The options are reopened.
- **The kbf helper's single signer not extending to jobscheduler's release signer.** A security decision for the project owner, taken
  before any option that ships jobscheduler packages through the helper (K36). Until then the agent ships only inside the kbf package.

## What stays open

Nothing in this plan is open. The triggers above are conditions for revisiting it, not questions waiting for an answer. Deploying the
server, installing a package on hosts and any registry upload are operator acts that need the project owner's go for that action (R7).
