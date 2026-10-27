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

[[pool]]                                    # provider + shape + min/max + labels
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
max = 8
join_token_file = "<path-to-token-file>"    # one file per registering pool; may hold several tokens, so rotation overlaps

[[pool]]
name = "gpu-burst"
provider = "cloud"
shape = "gpu-a100x8"
class = "gpu-host"
min = 0
max = 4
idle_timeout = "10m"                        # 5.8
boot_timeout = "15m"
labels = { tier = "gpu", site = "cloud" }
```

A consumer picks pools in its own configuration, not the scheduler's. For kbf, a sketch of the section that maps its action classes
to selectors (the file is kbf's; the scheduler never reads it):

```toml
[[scheduler.class]]
kbf_class = "whole_machine"                 # leases a whole gpu host from any pool labelled tier=gpu
class = "gpu-host"
labels = { tier = "gpu" }
```
