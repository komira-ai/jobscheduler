# First live launch on EC2

This is the checklist for the first time a scheduler spends money on an EC2 instance. A lead and the owner
of the account run it together. The design is `docs/design/pools-providers-network.md`; the provider is
`crates/js-provider-ec2`. The steps that do not depend on the cloud (the caps, the records file, the
teardown order, the abort table) are those of `docs/runbooks/first-live-launch.md`; this file says what
differs for EC2 and refers to that one for the rest.

Each item is marked:

- **CI** a test in this repository proves it against fakes. It says nothing about the real cloud.
- **LIVE** only a person looking at the real systems can prove it. Nothing in CI covers it.

The service is started with `--config <file>` and nothing else. Stop at the first failed pass criterion; do
not go on to see what happens. Write down each step's output (with every secret masked) in the launch
record: the date, the scheduler version, the head of this repository, and who ran it.

The first live launch needs an explicit go from the CEO. Until then the pool stays at `launch = "plan"`.

## 1. Prerequisites

### 1.1 The machine provider

```toml
[[machine_provider]]
name   = "aws"
kind   = "ec2"
region = "<region>"
images = { arm64 = "<ami id>", x86_64 = "<ami id>" }     # at least one architecture
# optional: purchase = ["on-demand"]      # the default; "spot" is the other term launched. Capacity Block offers are listed in the catalog but not launched yet
# optional: allowed_instance_types = ["<instance type>"]
# optional: capacity_block_hours = <hours>
```

The kind refuses a key it does not know, so a mistyped key stops the start. There is no key for a
credential: a `secret_access_key`, `api_key` or the like is refused naming the key. **CI**
(`kind_ec2_builds_and_gce_is_still_refused`). An image id is `ami-` followed by 8 to 17 lower-case
hexadecimal digits; `aarch64` is accepted as another spelling of `arm64`. **CI** (the `images_config_is_validated_at_parse`
test of `js-provider-ec2`).

Name the provider in a `[[platform]]` of kind `default` after `static`, and name the platform in the
`[[pool]]`, as in section 1 of `first-live-launch.md`. The pool sets `max_instances = 1`, the spend caps, and
`launch = "plan"`. The provider's own `max_machines` is the pool's `max_instances`. With `launch = "plan"`
the provider is built disabled and in dry run: it makes no launch call. **CI**.

### 1.2 Credentials

The service never reads a credential from the file. It uses, in this order:

1. the provider-standard variables `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and, when the keys are
   temporary, `AWS_SESSION_TOKEN`, set in the service's environment by the supervisor (a systemd
   credential or an environment file, mode 0600, owned by the service user); or
2. the instance role of the machine the scheduler runs on, read from the metadata service (version 2).

`AWS_REGION` is not read: `region` in the file wins. With neither source the calls fail as `NoCredential`
and the pool launches nothing. **CI** (credential lookup against fakes; no credential appears in any
`Debug` or log line). **LIVE** that the credential is the intended one: use a credential for an account used
only for this scheduler, or one where every instance is ours to touch. `server.scheduler_id` is unique on
the account.

### 1.3 IAM

The credential is allowed these actions and nothing else on the account:

- launch side: `ec2:RunInstances`, `ec2:DescribeInstances`, `ec2:TerminateInstances`, `ec2:CreateTags`
- catalog side: `ec2:DescribeInstanceTypes`, `ec2:DescribeInstanceTypeOfferings`,
  `ec2:DescribeSpotPriceHistory`, `ec2:DescribeCapacityBlockOfferings`, `pricing:GetProducts`,
  `servicequotas:GetServiceQuota`

`RunInstances` also needs the permissions on the resources it uses (the image, a subnet, a security group,
the volume); give them for the one image and the one subnet of the run. **LIVE**.

### 1.4 Pre-checks

| # | item | pass criterion | mark |
|---|---|---|---|
| 1.4.1 | An image for each architecture the pool's offers have. An offer whose architecture has no image is not launched. | `describe-images` in the account's console shows the image `available` in the region, with the architecture the config names. | LIVE |
| 1.4.2 | The vCPU quota of the instance family covers one instance of the smallest allowed type. | The Service Quotas console shows the quota above the type's vCPUs and above what is in use. The catalog reads the same quota, and an offer above it is not launched. | LIVE (the figure), CI (the check) |
| 1.4.3 | The zone offers the instance type. | The console lists the type in the zone. | LIVE |
| 1.4.4 | The default network gives the host a route to `advertise_url`, and no inbound is open (including port 22). The provider sets no security group; the account's default decides. | The security group of the default network has no inbound rule. | LIVE |
| 1.4.5 | `max_instances = 1`, and the hour and day caps set to the smallest allowed type's price for one hour and about twice that for a day. | `/v1/pools` shows `instances.max` 1 and the caps in cents. | CI (arithmetic), LIVE (the price is the cloud's) |
| 1.4.6 | `launch = "plan"`. | `/v1/pools` shows `launch: plan`. | CI |

On fail: fix the item. Nothing has been spent.

## 2. Plan mode

Start the service with `launch = "plan"`. Read `/v1/pools`: the pool is `available` once the catalog has
been read. The catalog reads are the only calls made in plan mode, and they spend nothing. Request a lease
and check that the status shows what would be launched (the type, the zone, the term) and that the EC2
console lists no new instance. **CI** (`a_plan_pool_launches_nothing`), **LIVE** (the console).

## 3. The go

Both people agree, aloud, in the launch record: the caps, the abort steps of section 5, who watches the EC2
console during the run, and that the CEO has given the go for this live launch.

1. Set `launch = "live"` in the pool and `boot = "initialise"` for this start only, as in step 4 of
   `first-live-launch.md`. These two lines are the only changes. Restart the service.
2. Request one lease (`js lease acquire --class whole-node --max-wall 30m`). The answer is 202 `pending`.
3. Watch `/v1/pools` every 10 s and the EC2 console.

| what you see | pass |
|---|---|
| One instance of the allowed type, in the zone, in the state `pending` then `running`. It carries the tags `js-scheduler=<scheduler id>`, `js-acquire-id=<acquire id>` and `Name=<host name>`. | tags and name as above (**LIVE**) |
| A retry of the same request, or a restart, does not start a second instance: the launch carries a client token derived from the acquire id and the scheduler id. | one instance in the console (**CI**: the fake keeps the token; **LIVE**) |
| `last_launch.outcome` is `accepted`, then the machine is `booting`, then `registered` once the agent says hello within `boot_timeout`. | **CI** for the order, **LIVE** for the real boot |
| `instances.n` is 1 and `spend_cents_per_hour` is the catalog price. | **CI** (arithmetic), **LIVE** (the price is the cloud's) |
| The lease moves from 202 `pending` to held. GPU jobs on EC2 run on the fallback runtime until GPU passthrough exists; this run does not test one. | **LIVE** |

A launch the cloud refused leaves no record and no instance; the reason is in the log (a quota or capacity
refusal is held back for the offer's cool-down, an authorisation refusal is not retried). If the launch call
was lost and the scheduler cannot tell whether an instance exists, it holds the machine for `unknown_hold`
(600 s by default) and finds it by its tags. **CI** (`acquire` and `list` tests of the fake).

## 4. Teardown

Release the lease (`js lease release <id>`) or let `max_wall` end. The host drains after the pool's `idle`
time, its token is revoked before the release is sent, and the instance is terminated, then read back until
the cloud reports it `terminated`. The order is the one in section 6 of `first-live-launch.md`. **CI**
(release and read-back against the fake), **LIVE** (the console agrees).

Pass for the run: the instance carried our tags and name; a hello arrived; the lease ran; the instance is
`terminated` in the console; `instances.n` is 0; no spend after the release. Open the billing page after an
hour and compare the charge with `max_spend_per_hour`. Then return the pool to `launch = "plan"`, set
`boot = "resume"` (check `<state file>.scaler` exists first), and restart, so the next spend is a new go.

## 5. Abort and rollback

Stop spend at the first doubt.

| situation | action |
|---|---|
| Before the flip | Nothing was launched. Leave `launch = "plan"`. |
| After the flip, before a launch | Set `launch = "plan"` and restart. Release the lease request. |
| An instance exists and the scheduler is healthy | Release the lease; let the teardown run. Watch it. |
| An `alerts` entry about a machine, or a registered host that never heartbeated past `boot_timeout` | Terminate the instance in the EC2 console. Instances are found by the tag `js-scheduler=<scheduler id>`. |
| The scheduler is wedged or unreachable | The same action. Stopping the service does not terminate instances; they bill until terminated. |
| A secret may have leaked | Terminate first. Then rotate the access key, or detach the instance role's policy, and rotate any join token. |

After an abort, verify by hand: the console lists no instance with the tag `js-scheduler=<scheduler id>`
that is not `terminated`, `/v1/pools` shows `instances.n` 0, and `orphans` and `alerts` are empty.

## What this run does not prove

One instance of one type in one region and one zone. It does not prove spot or a Capacity Block, the second
architecture, a launch against the quota edge, a restart of the scheduler while the instance is up
(**CI** only, with fakes), or the signed instance identity: the agent sends none yet, so a host is matched
by its name and instance id.
