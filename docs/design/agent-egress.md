# Egress of jobs in `crun` containers (privilege option B)

Status: implemented, and wired into the `crun` runtime (see "What is and is not wired").
The agent's privilege model is `agent-privilege.md`: the agent is root, every `crun` runs as
a dedicated job user.

## The premise, re-proved

The earlier egress experiments (the spike in `crates/js-contain/tests/egress_spike`) chose to
make a network namespace as root, put a veth and a firewall around it, and have a root `crun`
join it by path. Under option B as built, `crun` is not root: it runs as the job user and
creates the container's user namespace itself. A network namespace root made is owned by the
initial user namespace, and `setns` into it needs `CAP_SYS_ADMIN` there, which the job user's
new user namespace does not have.

`tests/egress/premise.rs` runs the distribution's `/usr/bin/crun` as the job user, with the
AppArmor user-namespace restriction on, against a namespace made with `ip netns add`: it fails
at `setns` with `Operation not permitted` and the job never runs. The control, the same bundle
with a new network namespace, runs. So the premise does not hold under B.

## The design chosen

`crun` creates the job's user and network namespaces together, as it does today (nothing in
js-contain's bundle changes), and the root agent wires the namespace from the host side, from
outside, while the container's init holds the job:

1. `Oci::start` runs `crun run`; the container's init (`js-init`) starts with
   `--wait-route <secs>` and does not start the job until `/proc/net/route` has a default route.
2. The agent asks `Oci::container_pid` for the init's host pid and calls `Egress::attach`
   (`crates/js-agent/src/egress/host.rs`): it takes a /30 of the pool, makes the veth pair, turns
   IPv6 off on the host end, addresses it, registers the interface in the firewall's set, opens
   `/proc/<pid>/ns/net` once and keeps the descriptor (the host's own namespace is refused by
   comparing device and inode), moves the job end into it and enters it, both by
   `/proc/<agent pid>/fd/<n>`, never by the container's pid again. It enters it with `nsenter` and, in one `ip -batch`, raises `lo`, gives
   `eth0` no IPv6 link-local address, addresses and raises it, and adds the default route last.
3. When the run ends the agent calls `release`: the set member and the host end go; the
   namespace is the kernel's to free with the container's last process.

The privileged part is `egress::host` in the agent process: root, in the host's initial
network namespace, running only the `ip`, `nft` and `nsenter` programs the operator configured. It is never
in the container and never the job user. The rules live in that namespace, which the job cannot
reach: its own namespace holds one interface and no firewall. Because the job's first instruction
runs after the last wiring step (the init gate), no packet of the job leaves an unwired or
half-wired namespace, and a container the agent fails to wire never starts its job (the init
gives up at the wait and the run ends as not run).

Why not the alternative, a namespace the agent makes inside a user namespace owned by the job
user's mapping: it needs a holder process per run, an id-mapping step, and a second user
namespace the container must join by path, all to reach what the agent can do from outside
with the capabilities it already holds.

### The firewall

Two tables, loaded in one `nft -f` transaction that first removes any earlier copy
(`egress/ruleset.rs`). `inet js_crun_egress` has a `forward` chain (priority -10, ahead of the
distribution's rules) that accepts replies to a job, refuses new traffic to a job, drops all IPv6
from a job, refuses a source address that does not route back through the interface it came
in on (`fib saddr . iif oif missing`: a job sends from its own address and no other), refuses
`169.254.0.0/16`, accepts the configured resolvers (port 53, UDP and TCP) and
endpoints (TCP) for an interface in the set `jobifs`, and refuses everything else from `jsv*`;
its `input` chain refuses everything a job sends to the host itself. `ip js_crun_egress_nat`
masquerades the pool toward anything outside it. Every rule counts and has a comment, so the
tests can read that a refusal was the rule's doing. `Egress::check` (called before every create)
reads both tables back and compares them, rule for rule, with what start loaded (counters,
handles and the set's members are not compared), and checks that IPv4 forwarding is on.

## Configuration

One mode, flags only: `--egress none` (the default) or `allow-list` with `--egress-allow`,
`--egress-dns`, `--egress-pool`, `--egress-ip`, `--egress-nft`, `--egress-nsenter`. Mode none does nothing: it runs
no program and writes no file; the container's namespace has loopback only. An allow-list start
first binds the abstract socket `@js-agent-egress` (`egress::HOST_LOCK`) and holds it while the
agent's egress lives; the kernel frees it when the process ends, however it ends. The names it
makes and removes are the host's, not an agent's, so a second agent with an allow-list is refused
at start (`EgressError::Held`) instead of sweeping the first one's live wiring. Then it
removes the interfaces named `jsv<digits>` and `jsp<digits>` (exactly) and both tables a previous
process left (the keeper kills the containers with the agent), loads the firewall and switches
IPv4 forwarding on. That switch is host-wide and is never switched back: other users of the host
may rely on it by then, and the agent cannot know. The tables are named `js_crun_egress` and
`js_crun_egress_nat`, apart from the Podman path's `js_egress`, which stays as it was. The kernel's
own reverse-path filter is not relied on; the firewall's `spoof` rule is, because it counts.

DNS: a configured allow-list names its resolvers; the backend writes a `resolv.conf` with those and
no other (`OciConfig::resolv_conf`), and the firewall accepts port 53 to those addresses only.

## What is and is not wired

- `egress::{Config, Egress, Attachment}`, `OciConfig::{resolv_conf, route_wait}`,
  `Oci::container_pid` and `js-init --wait-route` are the parts. The `crun` runtime
  (`crates/js-agent/src/crun/net.rs`) calls them in this order: `Egress::check` before every
  create (the lifecycle's `check_egress`; a failure is `egress-unchecked` and creates nothing),
  the create, `Oci::start`, `Oci::container_pid` (asked again until `crun` names the pid, until
  `crun run` ends, or for the init's route wait at most), `Egress::attach`, and
  `Attachment::release` when the runtime reads the run's end or removes the run, whichever
  comes first. A container whose pid or wiring fails is removed with its lease, and its job never
  ran (the init still waits for the route). A release that fails is logged and the removal goes
  on. With an allow-list the backend writes the configured resolvers in every container's
  `resolv.conf` and starts the init with `--wait-route`; mode none changes neither.
- The flags are parsed and checked at start. The agent starts the egress once, after it holds
  the claims of its scratch root and host id (the start removes what an earlier agent left);
  an allow-list that cannot be set up ends the agent with status 2. The start-up sweep of the
  leases an earlier process left needs no call of its own: no `Attachment` of that process
  exists, and the egress start has already removed its interfaces and tables. There is no job
  field yet: the agent's `--egress` applies to every `crun` job it runs. Only `--runtime crun`
  enforces an allow-list, so another runtime is refused one at start, and `--container-network`
  is refused beside it. The old Podman flags (`--container-network`, `--container-egress-allow`, `--container-nft`), their
  check and `deploy/js-agent/egress-network.sh` stay as the Podman runtime's own until that runtime
  is removed; deleting them now would take egress away from the runtime nodes run today.
- The hosts must not drop forwarded packets in another chain: Docker's forward policy does. The CI
  job stops Docker; a node with Docker's firewall needs its own allowance. The farm's kernel and
  AppArmor policy are not exercised in CI.

## The held canary (not run here)

On one production host, with the `crun` runtime and an allow-list: a job reaches its endpoint
on another host and is refused elsewhere (the metadata address included), the veth, the set
member and the `jsv*` links are gone after the run and after an agent restart, and the
distribution's `crun` AppArmor profile is loaded (the restriction applies to the job user's `crun`).
The CI job `agent-egress` runs the same through `CrunRuntime` on the runner's kernel; the
host's kernel (6.8) and its AppArmor policy are exercised only by the canary, which no pull
request runs.
