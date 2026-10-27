# Configuration example

Companion to [docs/design.md](../design.md), section 5.7. The rules that govern this file are in 5.7; section numbers refer to design.md.

```toml
[[shape]]                                   # what one host offers; pools and providers refer to it by name
name = "gpu-a100x8"
gpu_class = "nvidia-a100"
[shape.resources]
cpu_millis = 96000
memory_bytes = 1099511627776
gpus = 8
[shape.capabilities]
isa = "x86-64-v3"

[[provider]]                                # operator-listed machines (the merged --pool-config hosts)
name = "lab"
kind = "static"

[[host]]                                    # the merged fields (id, gpu_class, resources, capabilities) plus the provider;
provider = "lab"                            # `shape`, when given, supplies resources and capabilities
id = "gpu-01"
shape = "gpu-a100x8"

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
gpu-a100x8 = "<instance-type>"

[[pool]]                                    # provider + shape + min_machines/max_machines + labels
name = "gpu-lab"
provider = "lab"
shape = "gpu-a100x8"
class = "gpu-host"
labels = { tier = "gpu", site = "lab" }

[[pool]]
name = "gpu-edge"
provider = "edge"
shape = "gpu-a100x8"
class = "gpu-host"
max_machines = 8                            # never exceeded, in-flight and tainted hosts counted (5.11)
join_token_file = "<path-to-token-file>"    # one file per registering pool; may hold several tokens, so rotation overlaps

[[pool]]
name = "gpu-burst"
provider = "cloud"
shape = "gpu-a100x8"
class = "gpu-host"
min_machines = 0
max_machines = 4                            # the provider never exceeds it (5.11)
max_step = 2                                # most machines acquired in one tick; default 2
idle_timeout = "10m"                        # a host idle this long is released (5.10); the one idle delay
cooldown = "2m"                             # no re-acquire after a refusal, a boot failure or a release
boot_timeout = "15m"
max_taints_in_a_row = 3                     # replacement stops, with an alert, after this many (5.12)
allow_host_jobs = false                     # default; true lets a consumer that also allows it run host jobs (5.12)
labels = { tier = "gpu", site = "cloud" }
```

A consumer picks pools in its own configuration, not the scheduler's. For kbf, a sketch of the section that maps its action classes
to selectors (the file is kbf's; the scheduler never reads it):

```toml
[[scheduler.class]]
kbf_class = "whole_machine"                 # leases a whole gpu host from any pool labelled tier=gpu
class = "gpu-host"
labels = { tier = "gpu" }

[scheduler]
max_leased_machines = 16                    # kbf's own cap on machine leases held at once; it queues the rest (5.11)
allow_host_jobs = false                     # default; a host job also needs the pool's and the agent's consent (5.12)
```

Pool keys are `min_machines` and `max_machines` (not `min` and `max`); the loader refuses the old names and names the new ones (5.7).
