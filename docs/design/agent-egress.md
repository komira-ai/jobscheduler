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
3. For a job that declares ports, after the default route, `attach` adds the forwards
   (below, "Declared ports"): a host port another run forwards is refused
   (`EgressError::PortTaken`) and the whole attach undone. When the wiring and the forwards stand
   and the agent has a state root, `attach` writes the run's record in the egress store
   (below, "The egress store"); a failed write is an error and takes the wiring down.
4. When the run ends the agent calls `release`: the forwards' rules, the set member and the
   host end go, and then the run's record; the namespace is the kernel's to free with the
   container's last process.

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
endpoints (TCP) for an interface in the set `jobifs`, under the ceiling `open` accepts IPv4 to
any address for an interface in the set `openifs` (one rule, after every refusal and the
allow-list's acceptances), and refuses everything else from `jsv*`; its `input` chain refuses
everything a job sends to the host itself. After the metadata refusal one rule,
`forward-reply` (`ct direction reply ct status dnat`), accepts what a job sends back on a
connection that reached it through a forward; no connection a job opens is DNATed, so it
accepts nothing a job starts. An `open` job is therefore still refused the metadata
range, IPv6, a spoofed source, another job's address (the `inbound` rule) and the host. Under
the ceiling `allow-list` the text has no `openifs` and no `open` rule, and is what it was before
`open` existed. `ip js_crun_egress_nat` has an empty `prerouting` chain (a NAT hook at
`dstnat`) for the forwards, and masquerades the pool toward anything outside it. Every rule
counts and has a comment, so the tests can read that a refusal was the rule's doing.
`Egress::check` (called before every create of a wired job: `allow-list` or `open`; a `none`
job's namespace has loopback alone and nothing of the firewall is its, so it is not checked)
checks that IPv4 forwarding is on, reads both
tables back, sets apart the forwards' rules (comment `fwd-...`), and compares the rest, rule for
rule, with what start loaded (counters, handles and the members of both sets are not compared),
and the forwards' rules with the forwards of the runs wired now (below).

### Declared ports

New traffic toward a job is refused (`inbound`), and a declared port is the only exception
(design 5.8). Each port `{host, container}` of a wired (`allow-list` or `open`) job becomes two
rules, both commented `fwd-<host>`; the prefix `fwd-` is reserved for them
(`egress/forward.rs`):

- in `prerouting` of `ip js_crun_egress_nat`: `iifname != "jsv*" fib daddr type local tcp
  dport <host> counter dnat to <job address>:<container>`. `fib daddr type local` leaves alone
  a packet the host merely routes, and `iifname != "jsv*"` a packet of a job: a job that
  connects to the host's address on a forwarded port meets the `input` refusal, so no job
  reaches another job (or itself) through a forward;
- at the head of `forward` of `inet js_crun_egress` (`insert rule`, so before `inbound`):
  `oifname "<jsvN>" ip daddr <job address> tcp dport <container> ct status dnat counter
  accept`. It names the one interface, address and port, and needs the packet to have been
  DNATed, so traffic aimed straight at the job's address, at another of its ports or at
  another job still meets `inbound`.

`attach` adds them after the default route, all of a job's forwards in one `nft -f`
transaction, reads both tables back and requires them to hold exactly the forwards it built;
otherwise the attach fails (`EgressError::Rules`) and is undone. It refuses a host port another
run of the agent forwards (`EgressError::PortTaken`) before loading anything. Placement books a
job's host ports with its allocation, so it never offers a second job a host port a live
allocation on that host holds, and it places a job only on a host whose ceiling covers its level
and ports. `release` lists each table, deletes the run's rules by handle and frees their ports;
a rule already gone is forgiven as a gone veth is, and a release that leaves a rule keeps the
run's address block out of use. A
start does not remove a forward of a run that still stands: it loads both tables again with those
forwards put back (see "Configuration" below), and the next process's adoption takes
them over. `Egress::check` requires every
forward of a run wired now to be there, whole (its two rules, each exactly as written), its
acceptance ahead of the other rules of `forward`, and no other rule under the prefix: a forward
missing, left over from no run, changed or twice is `EgressError::Rules`, and the start is
refused `egress-unchecked`.

TCP only: a declared port carries no protocol. IPv4 only. Reaching a forward from the host's own
loopback, or from another job on the same host, is not promised: a packet the host itself sends
does not pass `prerouting`, and a job's packet to the host is refused by `input`.

## Configuration

The agent's flags are a ceiling, and each job asks for a level at or under it (design 5.8): a
start's `network` is `none` (the default), `allow-list` or `open`, and it may declare `ports`.
Flags only: `--egress none` (the default), `allow-list` or `open`, with `--egress-allow`,
`--egress-dns`, `--egress-forward-port`, `--egress-pool`, `--egress-ip`, `--egress-nft`,
`--egress-nsenter`. `allow-list` needs at least one `--egress-allow`; `open` needs no other flag
(an `allow-list` job under it gets the list, and with no `--egress-allow` reaches only the
resolvers); every other flag needs `allow-list` or `open`, and each refusal names its flag.
`--egress-forward-port` (`PORT` or `LO-HI`, at most 16) lists the host ports a job may declare.

Admission compares a start with the ceiling before anything is made (`egress::admit`, which the
lifecycle calls with the runtime's `network_ceiling` after the mounts and before
`Egress::check`): a level above it is refused `network-above-ceiling` (the text names the level
and the ceiling), any declared port on an agent that forwards none (the ceiling `none`, or no
`--egress-forward-port`) `ports-not-supported`, a declared host port the flags do not list
`port-above-ceiling`, and a declared port of a `none` job `ports-not-supported` (a forward
needs a wired namespace). A job is never run with less than it asked for. A Docker or Podman run
has the ceiling `none`. The hello reports the ceiling as the capability `egress` (`none`,
`allow-list` or `open`; `NetworkCeiling` in `js-agent-wire` writes and reads it), and
`egress-ports`, the host ports the agent forwards (`--egress-forward-port`, under `allow-list`
or `open` only). An agent from before the keys reports neither, which
reads as `none`, and ignores a start's `network` and `ports`, so a server places a job that asks
for more only on a host whose `egress` covers it.

Mode none does nothing: it runs no program and writes no file; every container's namespace has
loopback only. Under `allow-list` or `open` a `none` job is not wired either: its bundle has the
loopback `resolv.conf` and no route wait (`Oci::create_loopback`), and its namespace keeps `lo`
alone. A start under `allow-list` or `open`
first binds the abstract socket `@js-agent-egress` (`egress::HOST_LOCK`) and holds it while the
agent's egress or any run it wired lives; the kernel frees it when the process ends, however it ends. The names it
makes and removes are the host's, not an agent's, so a second agent with a wired ceiling is
refused at start (`EgressError::Held`) instead of taking or removing the first one's live wiring.
Then it reads what a previous process left: the records of the egress store (below) and the
interfaces named `jsv<digits>` and `jsp<digits>` (exactly). It reads nothing of the old tables'
members or forwards: the records are the source. A record is usable when its level is one the
ceiling allows (`allow-list` always, `open` only under the ceiling `open`), every host port of
its ports is one the ceiling forwards, and its block lies in the pool and is not another usable
record's; an unreadable or unusable record is logged and set aside for `settle`. Every `jsv` or
`jsp` interface whose block no usable record names is a stray. It loads the firewall in one
`nft -f` transaction that replaces both tables and, in the same transaction, puts back the
member of each usable record (in the set of its level) and its forwards, reads both tables back
and requires them to hold exactly the firewall and those forwards (otherwise the start fails,
`EgressError::Rules`), and switches IPv4 forwarding on.
nftables applies a batch whole, so a packet of a running job meets the old tables or the new
ones, never none: a job is not left unfiltered or without its forward. The address blocks of every
interface found, kept or stray, are kept out of use until the runtime has settled. The wiring is
the kernel's, so a process that ends leaves it, and its records with it: the crun runtime leaves
every run's wiring when it is dropped (`Attachment::leave`) and the jobs go on under the same
rules. The host-port relay connections (`--host-port`) the agent process itself carries are not
the kernel's: they end with the process, and the next one offers them again. That switch is host-wide and is never switched back: other users of the host
may rely on it by then, and the agent cannot know. A start does not kill or remove a standing run (the unit's `KillMode=process` leaves the
containers alive), and an update's hand-over keeps a job's egress before, during and after the
restart. The tables are named `js_crun_egress` and
`js_crun_egress_nat`; the Podman path's table `js_egress` and its flags were removed. The kernel's
own reverse-path filter is not relied on; the firewall's `spoof` rule is, because it counts.

DNS: a configured ceiling names its resolvers; the backend writes a `resolv.conf` with those and
no other (`OciConfig::resolv_conf`) for a wired job, and the firewall accepts port 53 to those
addresses only for an `allow-list` job (an `open` job reaches them through its own rule).

## The egress store

What the kernel holds says which rules exist, not which run they belong to or at what level, so
the agent keeps one record per wired run in the directory `egress` of its state root
(`--state-root`; mode 0700): `<run>.json`, the run being the container's name, a JSON object
`{version, index, level, ports}` with the run's block index, its level (`allow-list` or `open`)
and its declared ports (host and container). A record is written (to a temporary file, synced,
renamed, the directory synced, mode 0600) by `Egress::attach` once the wiring and forwards
stand, and removed by `release` once the wiring is down. A process that ends, or a hand-over,
removes nothing: the records, like the veth pairs, the set members and the forwards, stay, and
the kernel's rules keep enforcing throughout. An agent with no state root has no store and
hands nothing over. Not in the store, and not handed over: the host-port relay connections
(`--host-port`) the agent process carries, which end with it and are offered again by the
next process.

## What is and is not wired

- `egress::{Config, Egress, Attachment}`, `OciConfig::{resolv_conf, route_wait}`,
  `Oci::container_pid` and `js-init --wait-route` are the parts. The `crun` runtime
  (`crates/js-agent/src/crun/net.rs`) calls them in this order: `Egress::check` before every
  create of a wired job (the lifecycle's `check_egress`, never asked for a `none` job; a failure
  is `egress-unchecked` and creates nothing),
  the create, `Oci::start`, `Oci::container_pid` (asked again until `crun` names the pid, until
  `crun run` ends, or for the init's route wait at most), `Egress::attach` with the job's level
  (its interface goes in `jobifs` for `allow-list`, `openifs` for `open`), and
  `Attachment::release` when the runtime reads the run's end or removes the run, whichever
  comes first. A process that ends while a run stands drops its runtime, which leaves each run's
  wiring and its record (`Attachment::leave`) instead of releasing them. The next process adopts
  the wiring: `Egress::adopt(run, level, ports)` for each run of the ledger that still stands,
  after the runtime has read the run's labels (`jobscheduler.dev/network`, and
  `jobscheduler.dev/network-ports` as `H:C,H:C` when ports are declared, written at stage). It
  takes the run's record the start kept and requires it to name the level and (compared as a
  set) the ports the run's job declared; a level above the ceiling, a record that is not there
  or differs, or a host port already held is refused (`EgressError::AboveCeiling`,
  `Unadoptable`, `PortTaken`). It enters no namespace and reads nothing of the container. A run
  that does not fit, or a run of an agent with a ceiling whose container carries no network
  label, is removed and reported removed, like any run the agent cannot take over. Once every run
  is adopted `Egress::settle` takes down the wiring of each kept record no run took over (a run
  removed, one that ended while no agent ran) and removes the record, removes the files of
  unusable records, and deletes every stray `jsv<digits>` and `jsp<digits>` (a pair a killed
  start left); a block is given out again only when its removal succeeded, and the first error
  fails the open. A container whose pid or wiring fails is removed with its lease, and its job never
  ran (the init still waits for the route). A release that fails is logged and the removal goes
  on, and its /30 block is not given out again until the next start settles the host: the sets
  hold interface names, so a member left in `openifs` would otherwise let the next run that
  took the name (an `allow-list` one, say) reach any address. A wired run on a runtime with no
  egress is refused at `stage`, before its container exists. For a wired job (`allow-list` or `open`) the backend writes the configured resolvers in
  the container's `resolv.conf` and starts the init with `--wait-route`; a `none` job, and every
  job in mode none, gets neither and is not attached.
- The flags are parsed and checked at start. The agent starts the egress once, after it holds
  the claims of its scratch root and host id (the start keeps what an earlier agent left, as the store's records say, for
  the runtime to adopt or settle); `--state-root` also holds the store;
  an allow-list that cannot be set up ends the agent with status 2. The wiring of the
  leases an earlier process left is adopted or removed by the runtime when it opens, as above;
  no `Attachment` of that process exists. Only
  the crun runtime enforces a ceiling, so the container engine is refused `allow-list` and `open` at
  start (its jobs get `none` only). Declared ports are forwarded through the DNAT rules of
  "Declared ports". The old Podman flags (`--container-network`, `--container-egress-allow`,
  `--container-nft`), their check and the operator script that set up the Podman network were
  removed: a job on the container-engine fallback (Docker or Podman, `--container-bin`) gets
  network none, and an agent started with one of those flags is refused at start with a message
  that names the replacement.
- The hosts must not drop forwarded packets in another chain: Docker's forward policy does. The CI
  job stops Docker; a node with Docker's firewall needs its own allowance. The farm's kernel and
  AppArmor policy are not exercised in CI.

## Measured launch time

The program `js-launch-measure` (`crates/js-agent/tests/bin/launch_measure.rs`; the method is in
`crates/js-agent/tests/launch_measure/run.rs`) times a `crun` job's start through the agent's own
`CrunRuntime`, with egress mode none and with an allow-list wired. The workflow
`js-launch-measure.yml` runs it by manual dispatch only. It is a measurement, not a test: nothing
asserts or gates on these numbers.

**The interval.** It runs from the call of `CrunRuntime::start_staged` to the first byte the job
writes, read by the program's blocking read of a FIFO it holds open. `create_and_start` is
`CrunRuntime::stage` then `start_staged`, so this is the second half of the runtime's start entry:
`crun run` and, for `wired`, the container's pid, `Egress::attach` (the veth, the /30, the
firewall's set member) and the init's wait for the default route, with the resolver file the
backend writes. Outside the interval: the lifecycle's per-start firewall check of a wired job
(`check_egress`), the stage (the lease's directories, cgroup and bundle), image pull and unpack
(the image is in the store before the first sample), and the teardown between samples.

The job is `sh -c 'echo up > /scratch/up'`. The FIFO is in the run's scratch directory, which the
runtime binds at `/scratch`. It is not at `/work/up` because `/work` is an overlayfs, and a FIFO
in an overlay's upper layer is a different pipe from the one the host opens: in a probe run with
the FIFO there, no job's write ever reached the program. The allow-list names one endpoint
(`192.0.2.10:9300`) and nothing listens there: the job opens no connection, so `wired` covers the
wiring, not a connect.

**The run.** One run of the workflow on a GitHub-hosted runner: image `ubuntu-24.04` (image
version `20261026.354.1`), 4 CPUs (`nproc` and `std::thread::available_parallelism`), kernel
`7.0.0-1017-azure`, `crun version 1.14.1`, the program built with the release profile. 50 measured
samples per variant after 5 discarded, the variants alternating sample by sample; no sample
failed. Milliseconds, one decimal place, rounded half away from zero; p90 by nearest rank:

| variant | runs | failed | min | median | p90 | max |
|---|---:|---:|---:|---:|---:|---:|
| none | 50 | 0 | 25.2 | 26.8 | 27.2 | 27.4 |
| wired | 50 | 0 | 56.3 | 57.8 | 68.0 | 71.1 |

wired - none (median): 31.0 ms

**Measured.** The median start was 26.8 ms with egress mode none and 57.8 ms with the allow-list
wired. The wired median is 31.0 ms longer (57.8 - 26.8 = 31.0) and 2.16 times the none median
(57.8 / 26.8 = 2.157, rounded to two places). The wired samples spread wider: 56.3 to 71.1 ms
(p90 68.0) against 25.2 to 27.4 ms (p90 27.2).

**Inferred, not measured.** A shared hosted runner of four virtual CPUs is not a reference node,
and these figures bound nothing about bare metal or a production host's kernel. The image was in the store
for every sample (a warm store), so a first start that pulls is not covered. There was one run of
the workflow, so the run-to-run spread is not known. The table does not say which step of the
wiring takes the 31.0 ms.

## The held canary (not run here)

On one production host, with the `crun` runtime and an allow-list: a job reaches its endpoint
on another host and is refused elsewhere (the metadata address included), the veth, the set
member and the `jsv*` links are gone after the run, a job that runs across an agent update keeps
its egress and its forwards, and no member or link of a run that ended is left after a restart, and the
distribution's `crun` AppArmor profile is loaded (the restriction applies to the job user's `crun`).
The CI job `agent-egress` runs the same through `CrunRuntime` on the runner's kernel; the
host's kernel (6.8) and its AppArmor policy are exercised only by the canary, which no pull
request runs.
