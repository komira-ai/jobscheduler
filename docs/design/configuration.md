# Configuration example

Companion to [docs/design.md](../design.md), section 5.7. The rules that govern this file are in 5.7; section numbers refer to design.md.

## The server's file today

The server (`js-lease-server`) reads one file, `--config <path>`, in place of its flags: a `[server]` table and `[[pool]]` tables, each
refusing a field it does not know, with the secrets as paths of owner-only files. The format and every refusal are in
[pools-providers-network.md](pools-providers-network.md), section 4; the crate docs list the keys. The flags still work when `--config`
is absent. With it, **any other flag is refused**, naming the first one on the command line and the key that takes its place, with exit
status 2 and before the file is read: `--listen: not allowed with --config; set server.listen`.

| flag | in the file |
|---|---|
| `--listen`, `--state-file`, `--boot`, `--default-ttl`, `--max-ttl`, `--tick`, `--tls-cert`, `--tls-key` | `server.<name>`, the flag's name with `_` for `-` |
| `--max-connections`, `--handshake-timeout`, `--header-timeout`, `--read-timeout`, `--write-timeout`, `--max-requests`, `--drain-timeout` | `server.<name>`, same defaults |
| `--agent-hello-burst`, `--agent-hello-interval`, `--require-host-credential` | `server.agent_hello_burst`, `server.agent_hello_interval`, `server.require_host_credential`, same defaults; refused unless a pool registers agents |
| `--token-file` | `server.client_token_file` |
| `--pool-config` | a `[[pool]]` with `provider = { kind = "static", hosts_file = ... }` |
| `--agent-pool-config` | a `[[pool]]` with `kind = "joined"` for machines started by hand (the pool's shape and timing are its keys), or with an elastic provider for machines the server launches |
| `--agent-token-file` | `join_token_file` of that pool |

A pool of hand-joined agents is one `[[pool]]` of kind `joined`. It launches nothing, so it has no `provider`, `network`, `launch`,
`max_instances`, `idle`, `boot_timeout` or spend key, and each of those is refused by name. It registers agents, so it counts as the one
pool that may; `server.agent_listen` is required as for an elastic pool.

```toml
[[pool]]
kind  = "joined"
name  = "nodes"                       # the `pool` of a hello, and the `provider` of a one-time token
class = "whole-node"
exec  = "container"                   # optional; absent or "none": no host contract
shape = { cpu_millis = 16000, memory_bytes = 68719476736 }   # optional keys: gpus, gpu_class
capabilities = { os = "linux", arch = "x86_64" }               # optional; as the agent pool file's [capabilities]
max_machines    = 6                   # required; from 1 to 262144, as in the agent pool file
join_token_file = "/run/credentials/js/join-token"           # optional shared pool token
heartbeat_interval = "10s"            # optional; as are grace ("1m") and lost_retention ("24h")
```

A node joins with a one-time token (the mint refuses any other `provider` than the pool's name with a 400): `POST /v1/agent/join-tokens` with the client token and `{"provider": "nodes", "instance_id": "node-1"}`
mints it, and the node's `--instance-file` holds `nodes:node-1`. The first hello swaps the token for a host credential. What the old
`--agent-pool-config` file had and this kind lacks: the whole-node keys (`max_whole_node`, `drain_after`, ...).

**Memory headroom.** One key, `memory_schedulable_percent`, a whole number from 1 to 100 (default 100), sets how much of the memory an
operator configured the server books; there is no other memory knob. It is written in three places, each in its own format and read the
same way (0, 101, a negative number or a word is refused at the check that reads it, naming the key):

| where | what it cuts |
|---|---|
| a `[[pool]]` of the `--config` file | the hosts of a static pool (the pool's key replaces the hosts file's, once, never on top of it; it may be larger as well as smaller than the file's own value), and the floor memory (1 GiB) of an elastic pool |
| the hosts file of a static pool (top level, before the first `[[host]]`; `--pool-config` or a `hosts_file`) | the `memory_bytes` of every host the file lists |
| the agent pool file (`--agent-pool-config`; top level, before `[shape]`) | the `[shape]` `memory_bytes` the pool books for each host |

The server books `floor(memory x percent / 100)` bytes, computed once where the booked shape is built, and every booking path sees that
number: a lease, a whole-node lease, a run's memory limit, a reservation and the host listing. It applies to the memory the operator
configured and never to a figure an agent reports about itself: a host that claims more than the pool lists is still booked at the cut. A
host's hello is held to the whole listed memory, not the cut (a host must really have the memory; the pool books less of it). `GET
/v1/hosts` shows, per host, `memory: {configured_bytes, schedulable_percent, bookable_bytes}` (`shape.memory_bytes` is the bookable
figure), and `GET /v1/pools` and `/status` show the same three for an elastic pool's floor.

The rest of this page is the example of the scheduler's own configuration of section 5.7, which the server does not read yet.

```toml
[[shape]]                                   # what one host offers; pools and providers refer to it by name
name = "mac-studio-gpu"                     # GPUs exist only on the Mac Studio machines of the GPU pool (D32)
gpu_class = "apple-m"
[shape.resources]
cpu_millis = 24000
memory_bytes = 206158430208
gpus = 1                                    # the machine's one GPU, leased with the whole host
[shape.capabilities]
os = "macos"
arch = "aarch64"

[[shape]]                                   # a shape with no GPU
name = "linux-x86-16"
[shape.resources]
cpu_millis = 16000
memory_bytes = 68719476736
gpus = 0
[shape.capabilities]
os = "linux"
arch = "x86_64"

[[provider]]                                # the DefaultProvider: hosts that register themselves (5.8); it creates and destroys nothing
name = "registered"
kind = "default"
deny_hosts_file = "<path-to-deny-file>"     # host ids refused and evicted (5.8)

[[provider]]                                # launches and terminates instances
name = "cloud"
kind = "aws"
enabled = false                             # a provider that bills an account starts disabled (12.4)
role_arn = "arn:aws:iam::<account-id>:role/<role-name>"
region = "<region>"
launch_template = "<launch-template-name>"  # the image, the agent bootstrap and the security group live here
subnets = ["<subnet-a>", "<subnet-b>"]
[provider.instance_types]                   # shape name to instance type
linux-x86-16 = "<instance-type>"           # a cloud provider offers no GPU shape (D32); launch options, price and VM facts: hosts.md

[[pool]]                                    # providers (in order) + shape + max_machines + labels; there is no minimum (D38)
name = "gpu"                                # the GPU machines run js-agent and register (D32, D36); no elastic provider has a GPU shape
providers = ["registered"]                  # one entry: nothing to fall through to
shape = "mac-studio-gpu"
class = "whole-node"                        # a dedicated whole-node pool; the merged name gpu-host is read as this (5.17). Every lease is a whole host
max_whole_node_per_consumer = 2             # default 1; max_whole_node defaults to max_machines here, and max_draining to 1 (whole-node.md)
max_machines = 8                            # never exceeded, in-flight and tainted hosts counted (5.11)
regain_grace = "5m"                         # a queued request waits this long after its last capable host left (D41); default 5m
join_token_file = "<path-to-token-file>"    # one file per pool that admits registering hosts; may hold several tokens, so rotation overlaps
labels = { tier = "gpu" }

[[pool]]
name = "cpu-burst"
providers = ["registered", "cloud"]         # the hierarchy: registered hosts first, an aws machine only when none can serve now (5.8)
shape = "linux-x86-16"
class = "action"
max_machines = 24                           # the pool's total over every provider, registered hosts included (5.11)
provider_max_machines = { cloud = 4 }       # at most 4 of the 24 are aws machines; the provider never exceeds it
join_token_file = "<path-to-token-file>"    # the registered hosts of this pool (a machine aws launches presents its instance identity instead)
max_step = 2                                # most machines acquired in one tick; default 2
idle_timeout = "10m"                        # a host idle this long is released (5.10); the one idle delay
cooldown = "2m"                             # no re-acquire after a refusal, a boot failure or a release
boot_timeout = "15m"
max_taints_in_a_row = 3                     # replacement stops, with an alert, after this many (5.12)
allow_host_jobs = false                     # default; true lets a consumer that also allows it run host jobs (5.12)
labels = { tier = "cpu", site = "cloud" }
```

A consumer picks pools in its own configuration, not the scheduler's. For kbf, a sketch of the section that maps its action classes
to selectors (the file is kbf's; the scheduler never reads it):

```toml
[[scheduler.class]]
kbf_class = "whole_machine"                 # kbf's name; leases a whole gpu host from any pool labelled tier=gpu (the GPU pool, D32)
class = "whole-node"                        # the generic class (5.17); the merged name gpu-host is read as this
labels = { tier = "gpu" }

[scheduler]
max_leased_machines = 16                    # kbf's own cap on machine leases held at once; it queues the rest (5.11)
allow_host_jobs = false                     # default; a host job also needs the pool's and the agent's consent (5.12)
```

Capabilities, VMs and the whole-node class (5.13, 5.14, 5.17) add this, all optional. The schema declares namespaced keys beyond the built-in
ones; a pool may state the class its hosts serve, the time it waits for a whole-node host to become ready, and a base image its elastic
hosts are launched with (so the scaler can count a constrained request as demand); the agent's VM settings are flags (R4), not file keys:

```toml
[[capability]]                              # beyond the built-in arch, os, os_version, xcode, sim.*, base_images, vms
key = "acme.metal"
kind = "count"                              # text | version | set_text | set_version | count

[[pool]]
name = "mac-vms"
shape = "mac-arm64"
providers = ["registered"]
class = "vm"                                # runtime_tag vm (6.2)
max_machines = 4
prepare_timeout = "10m"                     # a whole-node lease gives up when its host is not ready this soon (5.17)
max_whole_node = 2                          # this mixed pool offers whole-node leases only because this is set: most hosts reserved or held at once
max_draining = 1                            # most hosts reserved and not yet held; default 1
max_whole_node_per_consumer = 1             # default 1
keep_for_ordinary = 1                       # hosts kept unmarked for ordinary work; default 1 in a mixed pool; max_whole_node + this <= max_machines
drain_deadline = "4h"                       # a reservation is given up after this; default the pool's max wall time (whole-node.md, section 4)
base_images = ["macos15-xcode16-a"]         # what an elastic host of this pool is launched with; the scaler counts a guest constraint as demand only if one of these can meet it
[pool.capabilities]
os = "macos"
arch = "aarch64"
```

```sh
# the agent's own flags (5.14): the VM backend is off unless --vms is above zero
js-agent --vms 2 --vm-driver <absolute-path> --vm-store <dir> --vm-network none \
  --vm-boot-timeout 5m \
  --cap acme.metal=1 --facts-interval 5m
```

A pool lists providers in order, and an entry may be a tier of equal providers (several clouds, hosts.md section 8): `providers =
["registered", ["cloud-a", "cloud-b"], "cloud"]`; a flat list means what it did.

A pool specifies `max_machines` and no minimum: `min_machines` and `min` are refused with a message saying so (5.7, D38). `provider = "x"` is
shorthand for a one-entry `providers`, except that an elastic `x` gets the `default` provider in front of it (5.8).

**Test provider.** The operator-listed pool exists for tests and conformance and for the single-host stepping stone. A file that declares
it is refused unless the scheduler runs with `--allow-test-providers`; no production example uses it. Its shape is:

```toml
[[provider]]                                # TESTS ONLY: hosts the operator lists (the merged --pool-config hosts)
name = "listed"
kind = "static"

[[host]]                                    # the merged fields (id, gpu_class, resources, capabilities) plus the provider
provider = "listed"
id = "test-host-01"
shape = "linux-x86-16"

[[pool]]
name = "test-pool"
providers = ["listed"]                      # max_machines defaults to the hosts listed
shape = "linux-x86-16"
class = "action"
```
