# Configuration example

Companion to [docs/design.md](../design.md), section 5.7. The rules that govern this file are in 5.7; section numbers refer to design.md.

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

[[provider]]                                # operator-listed machines (the merged --pool-config hosts)
name = "lab"
kind = "static"

[[host]]                                    # the merged fields (id, gpu_class, resources, capabilities) plus the provider;
provider = "lab"                            # `shape`, when given, supplies resources and capabilities
id = "gpu-01"
shape = "mac-studio-gpu"

[[provider]]                                # machines that register themselves (5.8)
name = "edge"
kind = "static"
register = true
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
linux-x86-16 = "<instance-type>"           # a cloud provider offers no GPU shape (D32)

[[pool]]                                    # provider + shape + min_machines/max_machines + labels
name = "gpu-lab"
provider = "lab"
shape = "mac-studio-gpu"
class = "gpu-host"
labels = { tier = "gpu", site = "lab" }

[[pool]]
name = "gpu-edge"
provider = "edge"
shape = "mac-studio-gpu"
class = "gpu-host"
max_machines = 8                            # never exceeded, in-flight and tainted hosts counted (5.11)
join_token_file = "<path-to-token-file>"    # one file per registering pool; may hold several tokens, so rotation overlaps

[[pool]]
name = "cpu-burst"
provider = "cloud"
shape = "linux-x86-16"
class = "action"
min_machines = 0
max_machines = 4                            # the provider never exceeds it (5.11)
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
kbf_class = "whole_machine"                 # leases a whole gpu host from any pool labelled tier=gpu (the GPU pool, D32)
class = "gpu-host"
labels = { tier = "gpu" }

[scheduler]
max_leased_machines = 16                    # kbf's own cap on machine leases held at once; it queues the rest (5.11)
allow_host_jobs = false                     # default; a host job also needs the pool's and the agent's consent (5.12)
```

Capabilities, VMs and the whole-node class (5.13 to 5.15) add this, all optional. The schema declares namespaced keys beyond the built-in
ones; a pool may state the class its hosts serve, the time it waits for a whole-node host to become ready, and a base image its elastic
hosts are launched with (so the scaler can count a constrained request as demand); the agent's VM settings are flags (R4), not file keys:

```toml
[[capability]]                              # beyond the built-in arch, os, os_version, xcode, sim.*, base_images, vms
key = "acme.metal"
kind = "count"                              # text | version | set_text | set_version | count

[[pool]]
name = "mac-vms"
provider = "lab"
shape = "mac-arm64"
class = "vm"                                # runtime_tag vm (6.2)
prepare_timeout = "10m"                     # a whole-node lease gives up when its host is not ready this soon (5.15)
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

Pool keys are `min_machines` and `max_machines` (not `min` and `max`); the loader refuses the old names and names the new ones (5.7).
